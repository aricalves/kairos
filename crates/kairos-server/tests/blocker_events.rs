//! The events of a blocker (KAIROS-T-0568, COLLIERY-I-0641):
//!
//! 1. An unlink of the last open `blocks` edge fires the `unblocked` rule
//!    and tells the claimant (`blocker_cleared`). An unlink with an open
//!    blocker left tells the claimant only. An unlink of the edge of a done
//!    blocker does nothing.
//! 2. A purge of an open blocker frees its dependants in the same way.
//! 3. A dependant can be an initiative: the rule of its board fires, and
//!    its owner is told.
//! 4. A done blocker that leaves the done column tells the claimant of each
//!    dependant, or its creator when nobody has the claim (`blocked_again`).
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_blocker_events_t0568_test`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_types::{Text, Uuid as SqlUuid};
use serde_json::Value;
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::KairosClient;
use kairos_client::types::{CreateInitiativeRequest, CreateTaskRequest};
use kairos_client::types_dispatch::{CreateDispatchRuleRequest, CreateWebhookTargetRequest};
use kairos_client::types_meta::CreateRelationshipRequest;
use kairos_client::types_org::{AddTeamMemberRequest, CreateTeamRequest, CreateTransitionRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;
use kairos_server::secrets::SecretsKey;

const SCRATCH_DB: &str = "kairos_blocker_events_t0568_test";

#[derive(QueryableByName, Debug)]
struct Note {
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = Text)]
    kind: String,
    #[diesel(sql_type = Text)]
    text: String,
}

#[derive(QueryableByName, Debug)]
struct Sent {
    #[diesel(sql_type = Text)]
    event: String,
    #[diesel(sql_type = Text)]
    body: String,
}

/// The notifications after the first `seen`, as (person, kind, text).
fn new_notes(conn: &mut PgConnection, seen: &mut usize) -> Vec<(Uuid, String, String)> {
    let rows: Vec<Note> = diesel::sql_query(
        "SELECT user_id, kind, text FROM org_acme.notifications ORDER BY created_at, id",
    )
    .load(conn)
    .expect("notifications");
    let new: Vec<(Uuid, String, String)> = rows
        .into_iter()
        .skip(*seen)
        .map(|n| (n.user_id, n.kind, n.text))
        .collect();
    *seen += new.len();
    new
}

/// The `data` of each `task.unblocked` delivery after the first `seen`
/// deliveries.
fn new_unblocked(conn: &mut PgConnection, seen: &mut usize) -> Vec<Value> {
    let rows: Vec<Sent> = diesel::sql_query(
        "SELECT event, body FROM org_acme.webhook_deliveries ORDER BY created_at, id",
    )
    .load(conn)
    .expect("deliveries");
    let new: Vec<Sent> = rows.into_iter().skip(*seen).collect();
    *seen += new.len();
    new.into_iter()
        .filter(|d| d.event == "task.unblocked")
        .map(|d| {
            let body: Value = serde_json::from_str(&d.body).expect("body");
            body["data"].clone()
        })
        .collect()
}

/// `(code, blocker, entity_type)` of each delivery, sorted.
fn summary(data: &[Value]) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = data
        .iter()
        .map(|d| {
            (
                d["code"].as_str().unwrap_or_default().to_string(),
                d["blocker"].as_str().unwrap_or_default().to_string(),
                d["entity_type"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

fn user(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

async fn task(client: &KairosClient, board: &str, title: &str) -> String {
    client
        .create_task(&CreateTaskRequest {
            board_id: Some(board.to_string()),
            column_id: None,
            title: title.into(),
            content: String::new(),
            task_type: None,
            work_class: None,
            team_id: None,
            repository: None,
            from_team: None,
        })
        .await
        .expect("task")
        .short_code
}

async fn blocks(client: &KairosClient, source: &str, target: &str) -> String {
    client
        .create_relationship(&CreateRelationshipRequest {
            source_short_code: source.into(),
            target_short_code: target.into(),
            relationship: "blocks".into(),
        })
        .await
        .expect("blocks edge")
        .id
}

/// Move a task through the columns, one hop each.
async fn moves(
    client: &KairosClient,
    code: &str,
    columns: &HashMap<String, String>,
    path: &[&str],
) {
    for name in path {
        client
            .transition_task(code, &columns[*name])
            .await
            .unwrap_or_else(|e| panic!("{code} to {name}: {e}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocker_that_goes_away_or_comes_back_tells_the_dependant() {
    let _admin_conn = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    let org_id: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org row");

    let http = reqwest::Client::new();
    let svc_token = user_token(&http, "svc").await;
    let alice_token = user_token(&http, "alice").await;
    let bob_token = user_token(&http, "bob").await;
    let carol_token = user_token(&http, "carol").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let mut config = base_config(&scratch_url);
    config.secrets_key = Some(SecretsKey::from_bytes([9u8; 32]));
    config.webhooks.allow_private = true;
    let state = app::state_with(config, pool, auth);
    let server = spawn_server(app::router(state)).await;
    let admin = server.client(&svc_token, "acme");
    let alice = server.client(&alice_token, "acme");
    let bob = server.client(&bob_token, "acme");
    let carol = server.client(&carol_token, "acme");
    for client in [&admin, &alice, &bob, &carol] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
        ("bob@kairos.test", OrgRole::Member),
        ("carol@kairos.test", OrgRole::Member),
    ] {
        let user_id = user(&mut conn, email);
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }
    let alice_id = user(&mut conn, "alice@kairos.test");
    let bob_id = user(&mut conn, "bob@kairos.test");
    let carol_id = user(&mut conn, "carol@kairos.test");

    let web = admin
        .create_team(&CreateTeamRequest {
            name: "Web".into(),
            slug: "web".into(),
            code_prefix: "WEB".into(),
            team_type: None,
        })
        .await
        .expect("team web");
    for user_id in [alice_id, bob_id, carol_id] {
        admin
            .add_team_member(
                &web.id,
                &AddTeamMemberRequest {
                    user_id: user_id.to_string(),
                },
            )
            .await
            .expect("team member");
    }
    let board = web.delivery_board_id.clone().expect("web board");
    let detail = admin.get_board(&board).await.expect("board");
    let slug = detail.board.slug.clone();
    let columns: HashMap<String, String> = detail
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();
    // A reopen path: Completed -> Todo.
    admin
        .add_transition(
            &board,
            &CreateTransitionRequest {
                from_column_id: columns["Completed"].clone(),
                to_column_id: columns["Todo"].clone(),
            },
        )
        .await
        .expect("Completed -> Todo");

    // One `unblocked` rule on the delivery board and one on the
    // initiatives board, each with no column and no filter.
    let receiver = common::hook_http::serve().await;
    for board_slug in [slug.as_str(), "initiatives"] {
        let target = admin
            .create_webhook_target(
                board_slug,
                &CreateWebhookTargetRequest {
                    name: "Runner".into(),
                    url: receiver.url.clone(),
                },
            )
            .await
            .expect("target")
            .target
            .id;
        admin
            .create_dispatch_rule(
                board_slug,
                &CreateDispatchRuleRequest {
                    trigger: Some("unblocked".into()),
                    column: None,
                    target_id: target,
                    task_type: None,
                    metadata_slug: None,
                    metadata_value: None,
                    repository: None,
                },
            )
            .await
            .expect("unblocked rule");
    }
    let mut notes = 0usize;
    let mut sent = 0usize;
    let _ = new_notes(&mut conn, &mut notes);
    let _ = new_unblocked(&mut conn, &mut sent);

    // --- 1. Unlink -----------------------------------------------------------
    // The last open blocker: the rule fires and the claimant is told.
    let b1 = task(&bob, &board, "Blocker one").await;
    let w1 = task(&alice, &board, "Waits for one").await;
    let edge = blocks(&bob, &b1, &w1).await;
    moves(&alice, &w1, &columns, &["Todo", "Active"]).await;
    let _ = new_notes(&mut conn, &mut notes);
    let _ = new_unblocked(&mut conn, &mut sent);
    bob.delete_relationship(&edge).await.expect("unlink");
    assert_eq!(
        summary(&new_unblocked(&mut conn, &mut sent)),
        vec![(w1.clone(), b1.clone(), "task".to_string())]
    );
    assert_eq!(
        new_notes(&mut conn, &mut notes),
        vec![(
            alice_id,
            "blocker_cleared".to_string(),
            format!("{b1} does not block {w1} now: the blocks link is removed.")
        )]
    );

    // An open blocker is left: the claimant is told, the rule does not
    // fire.
    let b2 = task(&bob, &board, "Blocker two").await;
    let b3 = task(&bob, &board, "Blocker three").await;
    let w2 = task(&alice, &board, "Waits for two").await;
    let edge = blocks(&bob, &b2, &w2).await;
    blocks(&bob, &b3, &w2).await;
    moves(&alice, &w2, &columns, &["Todo", "Active"]).await;
    let _ = new_notes(&mut conn, &mut notes);
    bob.delete_relationship(&edge).await.expect("unlink");
    assert!(new_unblocked(&mut conn, &mut sent).is_empty());
    let told = new_notes(&mut conn, &mut notes);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        (told[0].0, told[0].1.as_str()),
        (alice_id, "blocker_cleared")
    );

    // The edge of a done blocker: it blocked nothing, so nothing happens.
    let b4 = task(&bob, &board, "Blocker four").await;
    let w3 = task(&alice, &board, "Waits for four").await;
    let edge = blocks(&bob, &b4, &w3).await;
    moves(&alice, &w3, &columns, &["Todo", "Active"]).await;
    moves(&bob, &b4, &columns, &["Todo", "Active", "Completed"]).await;
    assert_eq!(
        summary(&new_unblocked(&mut conn, &mut sent)),
        vec![(w3.clone(), b4.clone(), "task".to_string())],
        "the done blocker fired once"
    );
    let _ = new_notes(&mut conn, &mut notes);
    bob.delete_relationship(&edge).await.expect("unlink");
    assert!(new_unblocked(&mut conn, &mut sent).is_empty());
    assert!(new_notes(&mut conn, &mut notes).is_empty());

    // --- 2. Purge ------------------------------------------------------------
    let p = task(&bob, &board, "Blocker to purge").await;
    let w4 = task(&alice, &board, "Waits for the purge").await;
    blocks(&bob, &p, &w4).await;
    moves(&alice, &w4, &columns, &["Todo", "Active"]).await;
    let _ = new_notes(&mut conn, &mut notes);
    admin.purge_task(&p).await.expect("purge");
    assert_eq!(
        summary(&new_unblocked(&mut conn, &mut sent)),
        vec![(w4.clone(), p.clone(), "task".to_string())]
    );
    assert_eq!(
        new_notes(&mut conn, &mut notes),
        vec![(
            alice_id,
            "blocker_cleared".to_string(),
            format!("{p} does not block {w4} now: it is purged.")
        )]
    );

    // --- 3. An initiative waits for a task --------------------------------
    let initiative = admin
        .create_initiative(&CreateInitiativeRequest {
            board_id: "initiatives".into(),
            column_id: None,
            title: "Waits for a task".into(),
            content: String::new(),
            complexity: None,
            bucket_type: None,
        })
        .await
        .expect("initiative")
        .short_code;
    diesel::sql_query(
        "INSERT INTO org_acme.item_owners (item_id, user_id) \
         SELECT id, $1 FROM org_acme.initiatives WHERE short_code = $2",
    )
    .bind::<SqlUuid, _>(alice_id)
    .bind::<Text, _>(&initiative)
    .execute(&mut conn)
    .expect("alice owns the initiative");
    let t = task(&bob, &board, "Blocks the initiative").await;
    blocks(&bob, &t, &initiative).await;
    let _ = new_notes(&mut conn, &mut notes);
    moves(&bob, &t, &columns, &["Todo", "Active", "Completed"]).await;
    let data = new_unblocked(&mut conn, &mut sent);
    assert_eq!(
        summary(&data),
        vec![(initiative.clone(), t.clone(), "initiative".to_string())]
    );
    assert!(data[0]["task_type"].is_null(), "{}", data[0]);
    assert_eq!(data[0]["board"]["slug"], "initiatives");
    assert_eq!(
        new_notes(&mut conn, &mut notes),
        vec![(
            alice_id,
            "blocker_cleared".to_string(),
            format!("{t} does not block {initiative} now: it is done.")
        )]
    );

    // --- 4. A done blocker opens again -------------------------------------
    let r = task(&bob, &board, "Blocker to reopen").await;
    let claimed = task(&alice, &board, "Claimed dependant").await;
    let unclaimed = task(&carol, &board, "Dependant with no claim").await;
    blocks(&bob, &r, &claimed).await;
    blocks(&bob, &r, &unclaimed).await;
    moves(&alice, &claimed, &columns, &["Todo", "Active"]).await;
    moves(&bob, &r, &columns, &["Todo", "Active", "Completed"]).await;
    assert_eq!(summary(&new_unblocked(&mut conn, &mut sent)), {
        let mut want = vec![
            (claimed.clone(), r.clone(), "task".to_string()),
            (unclaimed.clone(), r.clone(), "task".to_string()),
        ];
        want.sort();
        want
    });
    let _ = new_notes(&mut conn, &mut notes);
    moves(&bob, &r, &columns, &["Todo"]).await;
    let mut told = new_notes(&mut conn, &mut notes);
    told.sort();
    let mut want = vec![
        (
            alice_id,
            "blocked_again".to_string(),
            format!("{r} blocks {claimed} again: it is not done now."),
        ),
        (
            carol_id,
            "blocked_again".to_string(),
            format!("{r} blocks {unclaimed} again: it is not done now."),
        ),
    ];
    want.sort();
    assert_eq!(told, want);
    // It blocks again, so its next close fires again.
    moves(&bob, &r, &columns, &["Active", "Completed"]).await;
    assert_eq!(summary(&new_unblocked(&mut conn, &mut sent)).len(), 2);
}
