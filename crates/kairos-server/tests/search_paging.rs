//! KAIROS-T-0623: the paging of `POST /api/search` over a 1000-item
//! fixture, against the booted production router on a real port.
//!
//! The fixture is a scratch tenant (`kairos_search_paging_t0623_test`):
//! 900 tasks with about 7 KB of content each, 40 initiatives, 30
//! documents and 30 ADRs. Titles repeat (ties for the title sort) and mix
//! case and non-ASCII letters (the byte order of a Rust `str`); scores
//! repeat (ties for the relevance sort); ten tasks are archived; one task
//! in five has a priority; one initiative is the parent of 150 tasks.
//!
//! Each search pages through its whole result 100 at a time. Each page is
//! compared with an oracle: the rows read from the tables, sorted the way
//! the search sorts (the field, then the short code), and cut. The page
//! must hold the oracle's slice, each group in the oracle's order, and
//! `total` must be the oracle's count. A page past the end keeps the total.
//!
//! The test also prints the time and the size of a search of 100 tasks
//! (`--nocapture`), the evidence of KAIROS-T-0623.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{Nullable, Text, Timestamptz, Uuid as SqlUuid};
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, spawn_server, user_token,
    with_database,
};
use kairos_db::items::{self, CreateAdr, CreateDocument, CreateInitiative, CreateTask};
use kairos_db::models::{
    BoardLevel, FieldType, MetadataDefinition, NewItemMetadata, NewMetadataDefinition,
    NewOrganizationMember, NewUser, OrgRole, RelationshipType, TaskType, User,
};
use kairos_db::schema::{
    boards, item_metadata, metadata_definitions, organization_members, organizations, users,
};
use kairos_db::{TenantPool, create_board, graph, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

/// Uniquely named scratch database for this test binary.
const SCRATCH_DB: &str = "kairos_search_paging_t0623_test";

/// The page size of every search here: the cap of `limit`.
const PAGE: usize = 100;

/// One row of the oracle: what the sort reads.
#[derive(Debug, Clone, QueryableByName)]
struct OracleRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = Text)]
    entity_type: String,
    #[diesel(sql_type = Text)]
    short_code: String,
    #[diesel(sql_type = Text)]
    title: String,
    #[diesel(sql_type = Timestamptz)]
    created_at: DateTime<Utc>,
    #[diesel(sql_type = Timestamptz)]
    updated_at: DateTime<Utc>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    deleted_at: Option<DateTime<Utc>>,
}

#[derive(QueryableByName)]
struct ScoreRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Float)]
    score: f32,
}

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
}

/// Every item of the tenant, archived ones too.
fn oracle_rows(conn: &mut PgConnection) -> Vec<OracleRow> {
    let arm = |table: &str, entity_type: &str| {
        format!(
            "SELECT id, '{entity_type}'::text AS entity_type, short_code, title, created_at, \
             updated_at, deleted_at FROM {table}"
        )
    };
    let sql = [
        arm("strategies", "strategy"),
        arm("initiatives", "initiative"),
        arm("tasks", "task"),
        arm("documents", "document"),
        arm("adrs", "adr"),
    ]
    .join(" UNION ALL ");
    sql_query(sql).load(conn).expect("oracle rows")
}

/// The `ts_rank_cd` of each item that matches `q`. The vector is the
/// expression over the title and the content of the item tables, not the
/// stored vector of `item_search` (KAIROS-T-0624): so the pages prove that
/// the stored vector matches and ranks as the expression does.
fn oracle_scores(conn: &mut PgConnection, q: &str) -> HashMap<Uuid, f32> {
    let arm = |table: &str| {
        format!(
            "SELECT id, setweight(to_tsvector('english', coalesce(title, '')), 'A') \
             || setweight(to_tsvector('english', coalesce(content, '')), 'B') AS tsv FROM {table}"
        )
    };
    let items = ["strategies", "initiatives", "tasks", "documents", "adrs"]
        .map(arm)
        .join(" UNION ALL ");
    let rows: Vec<ScoreRow> = sql_query(format!(
        "SELECT id, ts_rank_cd(tsv, websearch_to_tsquery('english', $1)) AS score \
         FROM ({items}) i WHERE tsv @@ websearch_to_tsquery('english', $1)"
    ))
    .bind::<Text, _>(q)
    .load(conn)
    .expect("oracle scores");
    rows.into_iter().map(|r| (r.id, r.score)).collect()
}

/// The sort of the search: the field in the order, then the short code.
fn oracle_sort(rows: &mut [OracleRow], field: &str, desc: bool, scores: &HashMap<Uuid, f32>) {
    rows.sort_by(|a, b| {
        let primary = match field {
            "created_at" => a.created_at.cmp(&b.created_at),
            "updated_at" => a.updated_at.cmp(&b.updated_at),
            "title" => a.title.as_str().cmp(b.title.as_str()),
            "relevance" => {
                let score = |r: &OracleRow| scores.get(&r.id).copied().unwrap_or(0.0);
                score(a).total_cmp(&score(b))
            }
            other => panic!("no oracle for the sort {other}"),
        };
        let primary = if desc { primary.reverse() } else { primary };
        primary.then_with(|| a.short_code.cmp(&b.short_code))
    });
}

/// The JSON key of the result group of a type.
fn group_of(entity_type: &str) -> &'static str {
    match entity_type {
        "strategy" => "strategies",
        "initiative" => "initiatives",
        "task" => "tasks",
        "document" => "documents",
        "adr" => "adrs",
        other => panic!("unknown type {other}"),
    }
}

/// A raw search: the status, the body, and the size of the body in bytes.
async fn post_search(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    body: &Value,
) -> (u16, Value, usize) {
    let response = http
        .post(format!("{base_url}/api/search"))
        .bearer_auth(token)
        .header("x-tenant", "acme")
        .json(body)
        .send()
        .await
        .expect("search request");
    let status = response.status().as_u16();
    let bytes = response.bytes().await.expect("search body");
    let value: Value = serde_json::from_slice(&bytes).expect("JSON body");
    (status, value, bytes.len())
}

/// Page through `request` 100 at a time and compare each page with
/// `expected`, the oracle's whole sorted result.
async fn assert_pages(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    label: &str,
    request: Value,
    expected: &[OracleRow],
) {
    let total = expected.len();
    let mut seen: HashSet<String> = HashSet::new();
    let pages = total.div_ceil(PAGE).max(1);
    for page in 0..pages {
        let offset = page * PAGE;
        let mut body = request.clone();
        body["limit"] = json!(PAGE);
        body["offset"] = json!(offset);
        let (status, response, _) = post_search(http, base_url, token, &body).await;
        assert_eq!(status, 200, "{label} page {page}: {response}");
        assert_eq!(
            response["total"].as_u64(),
            Some(total as u64),
            "{label} page {page}: the total"
        );
        let slice = &expected[offset.min(total)..(offset + PAGE).min(total)];
        for group in ["strategies", "initiatives", "tasks", "documents", "adrs"] {
            let got: Vec<&str> = response["results"][group]
                .as_array()
                .map(|cards| {
                    cards
                        .iter()
                        .map(|c| c["short_code"].as_str().expect("short_code"))
                        .collect()
                })
                .unwrap_or_default();
            let want: Vec<&str> = slice
                .iter()
                .filter(|r| group_of(&r.entity_type) == group)
                .map(|r| r.short_code.as_str())
                .collect();
            assert_eq!(got, want, "{label} page {page}: the {group} in order");
            for code in got {
                assert!(seen.insert(code.to_string()), "{label}: {code} twice");
            }
        }
    }
    assert_eq!(seen.len(), total, "{label}: each hit on one page");

    // A page past the end has no hit, and keeps the total.
    let mut body = request.clone();
    body["limit"] = json!(PAGE);
    body["offset"] = json!(total + 50);
    let (status, response, _) = post_search(http, base_url, token, &body).await;
    assert_eq!(status, 200, "{label} past the end: {response}");
    assert_eq!(
        response["total"].as_u64(),
        Some(total as u64),
        "{label} past the end: the total"
    );
    for group in ["strategies", "initiatives", "tasks", "documents", "adrs"] {
        assert!(
            response["results"][group]
                .as_array()
                .is_none_or(|cards| cards.is_empty()),
            "{label} past the end: no {group}"
        );
    }
}

/// About 7 KB of content: the size of a task on the live board.
fn task_content(n: usize) -> String {
    let mut out = String::with_capacity(7_400);
    out.push_str(&format!("Task {n} of the paging fixture.\n\n"));
    // A score that repeats: the count of `lantern` goes 0..=3.
    for _ in 0..(n % 4) {
        out.push_str("The lantern lights the path. ");
    }
    while out.len() < 7_000 {
        out.push_str("Each paging step reads one window of rows and no more than that window. ");
    }
    out
}

/// The title of the item `n`: ties every 50, case and non-ASCII letters.
fn title(kind: &str, n: usize) -> String {
    const PREFIXES: [&str; 7] = ["alpha", "Beta", "Ärger", "zeta", "Zulu", "_under", "émile"];
    format!("{} {kind} {:02}", PREFIXES[n % PREFIXES.len()], n % 50)
}

#[tokio::test]
async fn search_pages_in_sql_over_a_thousand_items() {
    // --- scratch database + tenant ------------------------------------------
    let mut admin_conn = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    let org_id: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org row");

    // --- live server -----------------------------------------------------------
    let http = reqwest::Client::new();
    let token = user_token(&http, "alice").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let router = app::router(app::state_with(base_config(&scratch_url), pool, auth));
    let server = spawn_server(router).await;
    let base_url = server.base_url.clone();

    // The first request provisions alice; then she becomes a member.
    let (status, _, _) = post_search(&http, &base_url, &token, &json!({"q": "x"})).await;
    assert_eq!(status, 403, "not a member yet");
    let alice_id: Uuid = users::table
        .filter(users::email.eq("alice@kairos.test"))
        .select(users::id)
        .first(&mut conn)
        .expect("alice provisioned");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: alice_id,
            role: OrgRole::Member,
        })
        .execute(&mut conn)
        .expect("granting alice membership");

    // --- the fixture -------------------------------------------------------------
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning seeding connection to the tenant schema");
    let seeder: Uuid = diesel::insert_into(users::table)
        .values(NewUser {
            external_id: "dex|seeder".into(),
            user_name: "dex|seeder".into(),
            email: "seeder@acme.test".into(),
            display_name: "Seeder".into(),
        })
        .returning(User::as_returning())
        .get_result(&mut conn)
        .expect("inserting seed user")
        .id;
    let board_by_slug = |conn: &mut PgConnection, slug: &str| -> Uuid {
        boards::table
            .filter(boards::slug.eq(slug))
            .filter(boards::deleted_at.is_null())
            .select(boards::id)
            .first(conn)
            .unwrap_or_else(|e| panic!("board {slug:?}: {e}"))
    };
    let initiative_board = board_by_slug(&mut conn, "initiatives");
    let adr_board = board_by_slug(&mut conn, "adrs");
    let team = common::seed_team(&mut conn, "Paging Team", "paging-team");
    let delivery_board = create_board(
        &mut conn,
        BoardLevel::Delivery,
        "Paging",
        "paging",
        kairos_db::CodePrefix::FromSlug,
        Some(team),
        Some(seeder),
    )
    .expect("creating delivery board")
    .id;
    let priority: Uuid = match metadata_definitions::table
        .filter(metadata_definitions::slug.eq("priority"))
        .select(metadata_definitions::id)
        .first::<Uuid>(&mut conn)
        .optional()
        .expect("priority lookup")
    {
        Some(id) => id,
        None => {
            diesel::insert_into(metadata_definitions::table)
                .values(NewMetadataDefinition {
                    name: "Priority".into(),
                    slug: "priority".into(),
                    field_type: FieldType::String,
                    is_system_default: false,
                })
                .returning(MetadataDefinition::as_returning())
                .get_result(&mut conn)
                .expect("priority definition")
                .id
        }
    };

    let seeding = Instant::now();
    let mut initiatives = Vec::new();
    for n in 0..40 {
        let content = format!(
            "Initiative {n} of the paging fixture. {}",
            "The lantern lights the path. ".repeat(n % 3)
        );
        initiatives.push(
            items::create_initiative(
                &mut conn,
                CreateInitiative {
                    board_id: initiative_board,
                    column_id: None,
                    title: &title("initiative", n),
                    content: &content,
                    complexity: None,
                    bucket_type: None,
                },
                seeder,
            )
            .expect("creating initiative"),
        );
    }
    let mut tasks = Vec::new();
    for n in 0..900 {
        let task = items::create_task(
            &mut conn,
            CreateTask {
                board_id: delivery_board,
                column_id: None,
                title: &title("task", n),
                content: &task_content(n),
                task_type: if n % 3 == 0 {
                    TaskType::Bug
                } else {
                    TaskType::Task
                },
                work_class: kairos_db::models::enums::WorkClass::Planned,
                repository_id: None,
            },
            seeder,
        )
        .expect("creating task");
        if n % 5 == 0 {
            diesel::insert_into(item_metadata::table)
                .values(NewItemMetadata {
                    item_id: task.id,
                    metadata_definition_id: priority,
                    value: "high".into(),
                })
                .execute(&mut conn)
                .expect("priority");
        }
        if n < 150 {
            graph::link_items(
                &mut conn,
                initiatives[0].id,
                task.id,
                RelationshipType::Parent,
                seeder,
            )
            .expect("parent edge");
        }
        tasks.push(task);
    }
    for n in 0..30 {
        let content = format!(
            "Document {n} of the paging fixture. {}",
            "The lantern lights the path. ".repeat(n % 2)
        );
        items::create_document(
            &mut conn,
            CreateDocument {
                board_id: initiative_board,
                title: &title("document", n),
                content: Some(&content),
                template_id: None,
            },
            seeder,
        )
        .expect("creating document");
        let content = format!(
            "ADR {n} of the paging fixture. {}",
            "The lantern lights the path. ".repeat(n % 4)
        );
        items::create_adr(
            &mut conn,
            CreateAdr {
                board_id: Some(adr_board),
                column_id: None,
                title: &title("adr", n),
                content: &content,
                decision_maker: None,
                decision_date: None,
            },
            seeder,
        )
        .expect("creating adr");
    }
    // Ten archived tasks, three of them under the parent.
    for task in tasks.iter().step_by(90) {
        items::soft_delete_item(
            &mut conn,
            kairos_core::short_code::ItemType::Task,
            task.id,
            seeder,
        )
        .expect("archiving a task");
    }
    eprintln!("seeded 1000 items in {:?}", seeding.elapsed());

    let all = oracle_rows(&mut conn);
    assert_eq!(all.len(), 1000, "the fixture has 1000 items");
    let live: Vec<OracleRow> = all
        .iter()
        .filter(|r| r.deleted_at.is_none())
        .cloned()
        .collect();
    let no_scores = HashMap::new();

    // 1. The board filter, the default sort (created_at desc).
    let mut expected: Vec<OracleRow> = live
        .iter()
        .filter(|r| r.entity_type == "task")
        .cloned()
        .collect();
    oracle_sort(&mut expected, "created_at", true, &no_scores);
    assert_eq!(expected.len(), 890);
    assert_pages(
        &http,
        &base_url,
        &token,
        "board",
        json!({"filter": {"board": "paging"}}),
        &expected,
    )
    .await;

    // 2. A text query, the default sort (relevance desc), every type:
    // ties in the score fall to the short code.
    let scores = oracle_scores(&mut conn, "lantern");
    let mut expected: Vec<OracleRow> = live
        .iter()
        .filter(|r| scores.contains_key(&r.id))
        .cloned()
        .collect();
    oracle_sort(&mut expected, "relevance", true, &scores);
    assert!(expected.len() > 600, "{} hits", expected.len());
    let types: HashSet<&str> = expected.iter().map(|r| r.entity_type.as_str()).collect();
    assert_eq!(types.len(), 4, "the hits mix four types");
    assert_pages(
        &http,
        &base_url,
        &token,
        "q relevance",
        json!({"q": "lantern"}),
        &expected,
    )
    .await;

    // 3. Each item: the title, ascending, in the byte order of the title.
    let scores = oracle_scores(&mut conn, "paging fixture");
    let mut expected: Vec<OracleRow> = live
        .iter()
        .filter(|r| scores.contains_key(&r.id))
        .cloned()
        .collect();
    oracle_sort(&mut expected, "title", false, &no_scores);
    assert_eq!(expected.len(), 990);
    assert_pages(
        &http,
        &base_url,
        &token,
        "q title asc",
        json!({"q": "paging fixture", "sort": {"field": "title", "order": "asc"}}),
        &expected,
    )
    .await;

    // 4. Archived work too, with `include_deleted`: the title, descending.
    let mut expected: Vec<OracleRow> = all
        .iter()
        .filter(|r| scores.contains_key(&r.id))
        .cloned()
        .collect();
    oracle_sort(&mut expected, "title", true, &no_scores);
    assert_eq!(expected.len(), 1000);
    assert_pages(
        &http,
        &base_url,
        &token,
        "include_deleted title desc",
        json!({
            "q": "paging fixture",
            "filter": {"include_deleted": true},
            "sort": {"field": "title", "order": "desc"}
        }),
        &expected,
    )
    .await;

    // 5. Two types, `metadata_missing` and `task_type`: updated_at asc.
    let with_priority: HashSet<Uuid> = sql_query(
        "SELECT im.item_id AS id FROM item_metadata im \
         JOIN metadata_definitions md ON md.id = im.metadata_definition_id \
         WHERE md.slug = 'priority' AND btrim(im.value) <> ''",
    )
    .load::<IdRow>(&mut conn)
    .expect("priority ids")
    .into_iter()
    .map(|r| r.id)
    .collect();
    let bugs: HashSet<Uuid> = tasks
        .iter()
        .filter(|t| t.task_type == TaskType::Bug)
        .map(|t| t.id)
        .collect();
    let mut expected: Vec<OracleRow> = live
        .iter()
        .filter(|r| bugs.contains(&r.id) && !with_priority.contains(&r.id))
        .cloned()
        .collect();
    oracle_sort(&mut expected, "updated_at", false, &no_scores);
    assert!(expected.len() > 200, "{} bugs", expected.len());
    assert_pages(
        &http,
        &base_url,
        &token,
        "metadata_missing",
        json!({
            "filter": {"task_type": ["bug"], "metadata_missing": ["priority"]},
            "sort": {"field": "updated_at", "order": "asc"}
        }),
        &expected,
    )
    .await;

    // 6. A traverse: the children of the first initiative, the title desc.
    let children: HashSet<Uuid> = tasks.iter().take(150).map(|t| t.id).collect();
    let mut expected: Vec<OracleRow> = live
        .iter()
        .filter(|r| children.contains(&r.id))
        .cloned()
        .collect();
    oracle_sort(&mut expected, "title", true, &no_scores);
    assert_eq!(expected.len(), 148);
    assert_pages(
        &http,
        &base_url,
        &token,
        "traverse",
        json!({
            "traverse": {"from": {"short_code": initiatives[0].short_code},
                          "relationships": ["parent"], "direction": "outbound", "depth": 2},
            "sort": {"field": "title", "order": "desc"}
        }),
        &expected,
    )
    .await;

    // --- the evidence: a search of 100 tasks, time and size -------------------
    for (label, body) in [
        (
            "100 tasks, board filter",
            json!({"filter": {"board": "paging"}, "limit": 100}),
        ),
        (
            "100 tasks, q",
            json!({"q": "paging", "filter": {"entity_type": ["task"]}, "limit": 100}),
        ),
    ] {
        // One warm-up, then the median of seven.
        post_search(&http, &base_url, &token, &body).await;
        let mut times: Vec<Duration> = Vec::new();
        let mut size = 0;
        for _ in 0..7 {
            let started = Instant::now();
            let (status, response, bytes) = post_search(&http, &base_url, &token, &body).await;
            times.push(started.elapsed());
            assert_eq!(status, 200, "{label}: {response}");
            assert_eq!(
                response["results"]["tasks"].as_array().map(Vec::len),
                Some(100)
            );
            size = bytes;
        }
        times.sort();
        eprintln!(
            "search evidence: {label}: median {:?} (min {:?}, max {:?}), {size} bytes",
            times[3], times[0], times[6]
        );
    }

    drop(conn);
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
