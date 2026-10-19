//! KAIROS-T-0632 — the outbox of the change events.
//!
//! A write inserts its events into `public.event_outbox` and sends no
//! NOTIFY. An emitter ([`events::send_outbox`]) sends them in batches. This
//! test checks, against the compose Postgres (A-0012 tier 2), in its own
//! scratch database:
//!
//! - a write leaves an unsent row and sends no NOTIFY; a rollback leaves
//!   no row;
//! - one pass sends one NOTIFY with a JSON array of the events, with
//!   `tenant` and `blocks_boards`, and marks the rows sent;
//! - two emitters at the same time send each row once;
//! - an emitter that fails before its commit sends nothing, and the rows
//!   wait for the next pass; a write whose connection ends after the
//!   commit loses no event;
//! - prune deletes only the rows sent more than a minute ago;
//! - a notification to many persons is one row with the list, in parts of
//!   at most 100;
//! - the agent key, the tenant tag, a cascade by codes and a board event.

#[path = "common/scratch.rs"]
mod scratch;

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Text};
use serde_json::Value;
use uuid::Uuid;

use kairos_core::notifications::NotificationKind;
use kairos_db::events::{self, EventKind, ThinEvent};
use kairos_db::items::CreateTask;
use kairos_db::models::enums::{BoardLevel, TaskType, WorkClass};
use kairos_db::models::teams::NewTeam;
use kairos_db::models::{
    NewOrganizationMember, NewUser, OrgRole, RelationshipType, TeamType, User,
};
use kairos_db::{
    create_board, graph, items, notifications, provision_tenant, run_public_migrations,
};

static SCRATCH_DB: scratch::ScratchName = scratch::ScratchName::new("kairos_event_outbox_t0632");

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("DATABASE_URL must contain a database path segment");
    format!("{base}/{}", scratch::scratch_db_name(db_name))
}

/// A connection to the scratch database, in the schema of `tenant`.
fn tenant_conn(url: &str, tenant: &str) -> PgConnection {
    let mut conn = PgConnection::establish(url).expect("connecting to scratch database");
    sql_query(format!("SET search_path TO org_{tenant}, public"))
        .execute(&mut conn)
        .expect("pinning search_path");
    conn
}

fn insert_user(conn: &mut PgConnection, external_id: &str) -> Uuid {
    use kairos_db::schema::users;
    diesel::insert_into(users::table)
        .values(NewUser {
            external_id: external_id.to_string(),
            user_name: external_id.to_string(),
            email: format!("{external_id}@acme.test"),
            display_name: external_id.to_string(),
        })
        .returning(User::as_returning())
        .get_result(conn)
        .expect("inserting user")
        .id
}

fn join_acme(conn: &mut PgConnection, user: Uuid) {
    use kairos_db::schema::{organization_members, organizations};
    let org: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(conn)
        .expect("acme");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org,
            user_id: user,
            role: OrgRole::Member,
        })
        .execute(conn)
        .expect("membership");
}

/// A team and its delivery board with the prefix `prefix`.
fn seed_board(conn: &mut PgConnection, slug: &str, prefix: &str, actor: Uuid) -> Uuid {
    use kairos_db::schema::teams;
    let team_id: Uuid = diesel::insert_into(teams::table)
        .values(NewTeam {
            name: slug.to_string(),
            slug: slug.to_string(),
            team_type: TeamType::StreamAligned,
        })
        .returning(teams::id)
        .get_result(conn)
        .expect("inserting team");
    create_board(
        conn,
        BoardLevel::Delivery,
        slug,
        &format!("{slug}-delivery"),
        kairos_db::CodePrefix::Given(prefix),
        Some(team_id),
        Some(actor),
    )
    .expect("creating delivery board")
    .id
}

fn create_task(
    conn: &mut PgConnection,
    board_id: Uuid,
    title: &str,
    actor: Uuid,
) -> (Uuid, String) {
    let task = items::create_task(
        conn,
        CreateTask {
            board_id,
            column_id: None,
            title,
            content: "",
            task_type: TaskType::Task,
            work_class: WorkClass::Planned,
            repository_id: None,
        },
        actor,
    )
    .expect("create task");
    (task.id, task.short_code)
}

#[derive(QueryableByName)]
struct Row {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    schema_name: String,
    #[diesel(sql_type = Text)]
    payload: String,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    sent: bool,
}

/// The rows of the outbox after `after`, in id order.
fn rows_after(conn: &mut PgConnection, after: i64) -> Vec<(i64, String, Value, bool)> {
    sql_query(
        "SELECT id, schema_name, payload::text AS payload, sent_at IS NOT NULL AS sent \
         FROM public.event_outbox WHERE id > $1 ORDER BY id",
    )
    .bind::<BigInt, _>(after)
    .load::<Row>(conn)
    .expect("outbox rows")
    .into_iter()
    .map(|r| {
        let payload = serde_json::from_str(&r.payload).expect("payload JSON");
        (r.id, r.schema_name, payload, r.sent)
    })
    .collect()
}

fn last_id(conn: &mut PgConnection) -> i64 {
    rows_after(conn, 0).last().map_or(0, |row| row.0)
}

/// The NOTIFY payloads that `listener` got, after a short wait.
fn received(listener: &mut PgConnection) -> Vec<Value> {
    thread::sleep(Duration::from_millis(300));
    listener
        .notifications_iter()
        .map(|n| {
            let n = n.expect("notification");
            assert_eq!(n.channel, events::EVENT_CHANNEL);
            serde_json::from_str(&n.payload).expect("payload JSON")
        })
        .collect()
}

/// The events of the batches, in order.
fn flatten(batches: &[Value]) -> Vec<Value> {
    batches
        .iter()
        .flat_map(|batch| batch.as_array().expect("a batch is an array").clone())
        .collect()
}

/// Send until the outbox has no unsent row. Returns the passes.
fn drain(conn: &mut PgConnection) -> Vec<events::Sent> {
    let mut passes = Vec::new();
    loop {
        let sent = events::send_outbox(conn, events::SEND_BATCH).expect("send");
        if sent.rows == 0 {
            return passes;
        }
        passes.push(sent);
    }
}

#[test]
fn the_outbox_sends_each_event_once_in_batches() {
    let admin_url = scratch::admin_database_url();
    let mut admin_conn = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
        panic!(
            "cannot connect to compose postgres at {admin_url}: {e} \
             (is the stack up? `angreal services up`)"
        )
    });
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin_conn)
        .expect("dropping scratch database");
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin_conn)
        .expect("creating scratch database");
    let _scratch_guard = scratch::ScratchGuard::new(&SCRATCH_DB);
    let url = with_database(&admin_url, &SCRATCH_DB);
    {
        let mut conn = PgConnection::establish(&url).expect("connecting to scratch database");
        run_public_migrations(&mut conn).expect("running public migrations");
        provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
        provision_tenant(&mut conn, "widgets", "Widgets Ltd").expect("provisioning widgets");
    }
    let mut conn = tenant_conn(&url, "acme");
    let alice = insert_user(&mut conn, "alice");
    let bob = insert_user(&mut conn, "bob");
    let carol = insert_user(&mut conn, "carol");
    for user in [alice, bob, carol] {
        join_acme(&mut conn, user);
    }
    let web = seed_board(&mut conn, "web", "WEB", alice);
    let ops = seed_board(&mut conn, "ops", "OPS", alice);
    // The seed wrote events: send them, so each step starts empty.
    drain(&mut conn);

    let mut listener = PgConnection::establish(&url).expect("listener");
    sql_query("LISTEN kairos_events")
        .execute(&mut listener)
        .expect("LISTEN");

    // --- 1. a write leaves an unsent row and sends no NOTIFY ----------------
    let start = last_id(&mut conn);
    let (a_id, a_code) = create_task(&mut conn, web, "Task A", alice);
    let rows = rows_after(&mut conn, start);
    assert_eq!(rows.len(), 1, "one event for a create: {rows:?}");
    let (_, schema, payload, sent) = &rows[0];
    assert_eq!(schema, "org_acme");
    assert!(!sent, "not sent before a pass");
    assert_eq!(payload["event"], "item_created");
    assert_eq!(payload["entity_type"], "task");
    assert_eq!(payload["short_code"], a_code.as_str());
    assert_eq!(payload["board_id"], web.to_string());
    assert!(payload["column_id"].is_string(), "a task has a column");
    assert!(
        payload.get("tenant").is_none() && payload.get("agent_key_id").is_none(),
        "the row has no tenant and no agent key: {payload}"
    );
    assert!(
        received(&mut listener).is_empty(),
        "a write sends no NOTIFY"
    );

    // A rollback leaves no row.
    let before = last_id(&mut conn);
    let rolled: Result<(), diesel::result::Error> = conn.transaction(|conn| {
        create_task(conn, web, "Rolled back", alice);
        Err(diesel::result::Error::RollbackTransaction)
    });
    assert!(rolled.is_err());
    assert_eq!(last_id(&mut conn), before, "a rollback leaves no row");

    // --- 2. one pass: one NOTIFY with the batch, rows marked sent -----------
    let (b_id, b_code) = create_task(&mut conn, ops, "Task B", alice);
    graph::link_items(&mut conn, a_id, b_id, RelationshipType::Blocks, alice).expect("A blocks B");
    events::emit_item_event_by_id(&mut conn, EventKind::ItemTransitioned, "task", a_id, alice)
        .expect("emit A");
    events::emit_item_event_by_id(&mut conn, EventKind::ItemDeleted, "task", b_id, alice)
        .expect("emit B");
    let passes = drain(&mut conn);
    assert_eq!(passes.len(), 1, "{passes:?}");
    assert_eq!(passes[0].notifies, 1, "one NOTIFY for the batch");
    assert_eq!(passes[0].dropped, 0);
    let batches = received(&mut listener);
    assert_eq!(batches.len(), 1, "one NOTIFY: {batches:?}");
    let sent = flatten(&batches);
    assert_eq!(sent.len(), passes[0].rows);
    let kinds: Vec<&str> = sent
        .iter()
        .map(|e| e["event"].as_str().expect("event"))
        .collect();
    assert_eq!(
        kinds,
        [
            "item_created",
            "item_created",
            "relationship_changed",
            "relationship_changed",
            "item_transitioned",
            "item_deleted"
        ],
        "in id order: {sent:?}"
    );
    assert!(sent.iter().all(|e| e["tenant"] == "acme"), "{sent:?}");
    let transitioned = &sent[4];
    assert_eq!(transitioned["short_code"], a_code.as_str());
    assert_eq!(
        transitioned["blocks_boards"],
        serde_json::json!([ops.to_string()]),
        "the board across the blocks edge"
    );
    let deleted = &sent[5];
    assert_eq!(deleted["short_code"], b_code.as_str());
    assert_eq!(
        deleted["blocks_boards"],
        serde_json::json!([web.to_string()])
    );
    assert!(
        sent[0].get("blocks_boards").is_none() && sent[2].get("blocks_boards").is_none(),
        "only the kinds that change a count carry it: {sent:?}"
    );
    assert!(
        rows_after(&mut conn, start).iter().all(|row| row.3),
        "each row is marked sent"
    );
    assert!(drain(&mut conn).is_empty(), "nothing left to send");
    assert!(!events::outbox_pending(&mut conn).expect("pending"));

    // --- 3. two emitters at the same time send each row once ----------------
    let start = last_id(&mut conn);
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        for n in 0..600 {
            events::emit_event(
                conn,
                &ThinEvent {
                    event: EventKind::ItemUpdated,
                    entity_type: "task".into(),
                    short_code: format!("RACE-T-{n:04}"),
                    board_id: Some(web),
                    column_id: None,
                    actor: alice,
                },
            )?;
        }
        Ok(())
    })
    .expect("600 events");
    assert!(events::outbox_pending(&mut conn).expect("pending"));
    let barrier = Arc::new(Barrier::new(2));
    let emitters: Vec<_> = (0..2)
        .map(|_| {
            let (url, barrier) = (url.clone(), Arc::clone(&barrier));
            thread::spawn(move || {
                let mut conn = PgConnection::establish(&url).expect("emitter connection");
                barrier.wait();
                let mut rows = 0;
                loop {
                    let sent = events::send_outbox(&mut conn, 50).expect("send");
                    if sent.rows == 0 && !events::outbox_pending(&mut conn).expect("pending") {
                        return rows;
                    }
                    rows += sent.rows;
                }
            })
        })
        .collect();
    let rows: Vec<usize> = emitters
        .into_iter()
        .map(|h| h.join().expect("emitter"))
        .collect();
    assert_eq!(rows.iter().sum::<usize>(), 600, "each row once: {rows:?}");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for event in flatten(&received(&mut listener)) {
        *counts
            .entry(event["short_code"].as_str().expect("code").to_string())
            .or_default() += 1;
    }
    assert_eq!(counts.len(), 600, "each event arrives");
    assert!(counts.values().all(|&n| n == 1), "no event twice");
    assert!(rows_after(&mut conn, start).iter().all(|row| row.3));

    // --- 4. a crash of an emitter before its commit ------------------------
    let start = last_id(&mut conn);
    {
        // The connection of the write ends after the commit: no loss.
        let mut writer = tenant_conn(&url, "acme");
        create_task(&mut writer, web, "Task C", alice);
    }
    let crashed: Result<(), diesel::result::Error> = conn.transaction(|conn| {
        let sent = events::send_outbox(conn, events::SEND_BATCH)?;
        assert_eq!(sent.rows, 1);
        Err(diesel::result::Error::RollbackTransaction)
    });
    assert!(crashed.is_err());
    assert!(
        received(&mut listener).is_empty(),
        "a rolled back pass sends no NOTIFY"
    );
    assert_eq!(
        rows_after(&mut conn, start)
            .iter()
            .filter(|row| !row.3)
            .count(),
        1,
        "the row waits"
    );
    // A pass on a different connection: a second server, or this one again.
    let mut other = PgConnection::establish(&url).expect("other emitter");
    assert_eq!(drain(&mut other).len(), 1);
    let events = flatten(&received(&mut listener));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "item_created");

    // --- 5. prune: only the rows sent more than a minute ago ----------------
    let start = last_id(&mut conn);
    for n in 0..3 {
        events::emit_board_configured(&mut conn, web, alice, &format!("column_add:{n}"))
            .expect("board event");
    }
    let ids: Vec<i64> = rows_after(&mut conn, start).iter().map(|r| r.0).collect();
    assert_eq!(ids.len(), 3);
    drain(&mut conn);
    received(&mut listener);
    sql_query(
        "UPDATE public.event_outbox SET sent_at = now() - interval '2 minutes' \
         WHERE id <= $1",
    )
    .bind::<BigInt, _>(ids[0])
    .execute(&mut conn)
    .expect("age the rows");
    sql_query("UPDATE public.event_outbox SET sent_at = NULL WHERE id = $1")
        .bind::<BigInt, _>(ids[2])
        .execute(&mut conn)
        .expect("one unsent row");
    let pruned = events::prune_outbox(&mut conn).expect("prune");
    assert!(pruned >= 1, "the old rows go: {pruned}");
    let left: Vec<i64> = rows_after(&mut conn, 0).iter().map(|r| r.0).collect();
    assert_eq!(
        left,
        vec![ids[1], ids[2]],
        "a recent row and an unsent row stay"
    );
    let events = flatten(&{
        drain(&mut conn);
        received(&mut listener)
    });
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "board_configured");
    assert_eq!(events[0]["short_code"], "web-delivery");
    assert_eq!(events[0]["board_id"], web.to_string());
    assert_eq!(events[0]["change"], "column_add:2");

    // --- 6. a notification to many persons is one event ---------------------
    let start = last_id(&mut conn);
    let told = notifications::notify(
        &mut conn,
        &[alice, bob, carol],
        NotificationKind::P0Bug,
        Some((a_id, &a_code)),
        "A is a critical bug",
        alice,
    )
    .expect("notify");
    assert_eq!(told.len(), 2, "not the actor");
    let rows = rows_after(&mut conn, start);
    assert_eq!(rows.len(), 1, "one row for the persons told: {rows:?}");
    let mut expected = vec![bob.to_string(), carol.to_string()];
    expected.sort();
    assert_eq!(rows[0].2["recipients"], serde_json::json!(expected));
    assert_eq!(rows[0].2["event"], events::NOTIFICATION_CREATED);
    assert_eq!(rows[0].2["short_code"], a_code.as_str());

    let many: Vec<Uuid> = (0..250).map(|_| Uuid::new_v4()).collect();
    let start = last_id(&mut conn);
    events::emit_notification_event(&mut conn, events::NOTIFICATIONS_READ, &many, "", alice)
        .expect("emit");
    let parts: Vec<usize> = rows_after(&mut conn, start)
        .iter()
        .map(|r| r.2["recipients"].as_array().expect("list").len())
        .collect();
    assert_eq!(parts, vec![100, 100, 50], "parts of at most 100");
    let passes = drain(&mut conn);
    assert_eq!(passes.iter().map(|p| p.dropped).sum::<usize>(), 0);
    let batches = received(&mut listener);
    // Two parts of 100 do not fit in one NOTIFY: 3900 bytes of UUIDs each.
    assert_eq!(passes.len(), 1);
    assert!(batches.len() >= 2, "{passes:?}");
    assert!(
        batches
            .iter()
            .all(|b| b.to_string().len() <= events::NOTIFY_LIMIT),
        "each NOTIFY under the limit"
    );
    assert_eq!(flatten(&batches).len(), 4);

    // --- 7. the agent key, the tenant tag, a cascade by codes ---------------
    let key = Uuid::new_v4();
    kairos_db::agent_mark::set(&mut conn, Some(key)).expect("agent key");
    let start = last_id(&mut conn);
    events::emit_item_events_by_codes(
        &mut conn,
        EventKind::ItemDeleted,
        &[b_code.clone(), "WEB-T-9999".to_string(), a_code.clone()],
        alice,
    )
    .expect("by codes");
    kairos_db::agent_mark::set(&mut conn, None).expect("no agent key");
    let rows = rows_after(&mut conn, start);
    let codes: Vec<&str> = rows
        .iter()
        .map(|r| r.2["short_code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        codes,
        [b_code.as_str(), a_code.as_str()],
        "in order, unknown skipped"
    );
    assert!(rows.iter().all(|r| r.2["agent_key_id"] == key.to_string()));
    assert!(rows.iter().all(|r| r.2["entity_type"] == "task"));
    assert_eq!(rows[0].2["board_id"], ops.to_string());

    let mut widgets = tenant_conn(&url, "widgets");
    events::emit_event(
        &mut widgets,
        &ThinEvent {
            event: EventKind::CodeIndexBuildChanged,
            entity_type: "repository".into(),
            short_code: "widgets-repo".into(),
            board_id: None,
            column_id: None,
            actor: alice,
        },
    )
    .expect("widgets event");
    drain(&mut conn);
    let events = flatten(&received(&mut listener));
    let tenants: Vec<(&str, &str)> = events
        .iter()
        .map(|e| {
            (
                e["tenant"].as_str().expect("tenant"),
                e["event"].as_str().expect("event"),
            )
        })
        .collect();
    assert_eq!(
        tenants,
        [
            ("acme", "item_deleted"),
            ("acme", "item_deleted"),
            ("widgets", "code_index_build_changed")
        ]
    );
    assert!(events[2].get("agent_key_id").is_none());
    assert!(events[2]["board_id"].is_null());

    drop(widgets);
    drop(other);
    drop(listener);
    drop(conn);
}
