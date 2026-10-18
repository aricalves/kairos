//! KAIROS-T-0602 — the board route and search send cards without content,
//! against the booted production router.
//!
//! - `GET /api/boards/{id}/items` sends no `content`, `created_by` or
//!   `updated_by` on a card, unless the query has `fields=full`;
//! - `fields=full` sends them, as the route did before cards;
//! - an unknown `fields` value is refused;
//! - a search hit has a `snippet` near the match, and no `content`;
//! - the test prints the size of each response on a seeded board, with
//!   about the content of the live kairos board (599 cards, 2.85 MB of
//!   content on 2026-10-16).
//!
//! Runs against the LIVE compose stack (`angreal services up`). The test
//! owns a scratch database.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::http::{Method, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, request, user_token,
    with_database,
};
use kairos_client::types_org::BoardItemsResponse;
use kairos_client::types_search::SearchResponse;
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const TENANT: [(&str, &str); 1] = [("x-tenant", "acme")];

/// The number of tasks on the seeded board: the size of the live kairos
/// board on 2026-10-16.
const CARDS: usize = 600;

/// The content of one task: about 4.8 KB, the mean of the live board
/// (2.85 MB for 599 cards).
fn content_of(number: usize) -> String {
    let paragraph = "The route reads the rows of the page and sends each card to the \
                     board. The board shows the code, the title, the type, the lane, \
                     the claim and the state line of the card. ";
    let mut text = format!("## What\n\nTask {number} has the word quokka{number}.\n\n");
    while text.len() < 4_800 {
        text.push_str(paragraph);
    }
    text
}

/// The booted router on a scratch database, with `svc` as the org admin.
struct Stack {
    scratch_db: &'static str,
    admin_conn: common::ScratchConn,
    conn: PgConnection,
    pool: TenantPool,
    router: Router,
    token: String,
}

impl Stack {
    async fn boot(scratch_db: &'static str) -> Self {
        let admin_conn = recreate_scratch_db(scratch_db);
        let scratch_url = with_database(&common::admin_database_url(), scratch_db);
        let mut conn =
            PgConnection::establish(&scratch_url).expect("connecting to scratch database");
        run_public_migrations(&mut conn).expect("running public migrations");
        provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
        let org_id: Uuid = organizations::table
            .filter(organizations::slug.eq("acme"))
            .select(organizations::id)
            .first(&mut conn)
            .expect("acme org row");

        let http = reqwest::Client::new();
        let token = user_token(&http, "svc").await;
        let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
        let auth = Arc::new(
            Authenticator::discover(ISSUER, AUDIENCE)
                .await
                .expect("OIDC discovery against live Dex"),
        );
        let router = app::router(app::state_with(
            base_config(&scratch_url),
            pool.clone(),
            auth,
        ));

        let _ = request(
            &router,
            Method::GET,
            "/api/whoami",
            Some(&token),
            &TENANT,
            None,
        )
        .await;
        let svc_id: Uuid = users::table
            .filter(users::email.eq("svc@kairos.test"))
            .select(users::id)
            .first(&mut conn)
            .expect("svc provisioned");
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id: svc_id,
                role: OrgRole::Admin,
            })
            .execute(&mut conn)
            .expect("granting membership");

        Stack {
            scratch_db,
            admin_conn,
            conn,
            pool,
            router,
            token,
        }
    }

    async fn send(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        request(&self.router, method, uri, Some(&self.token), &TENANT, body).await
    }

    async fn ok(&self, method: Method, uri: &str, body: Option<Value>) -> Value {
        let (status, answer) = self.send(method.clone(), uri, body).await;
        assert!(status.is_success(), "{method} {uri}: {status} {answer}");
        answer
    }

    fn shutdown(mut self) {
        drop(self.pool);
        drop(self.conn);
        drop_scratch_db(&mut self.admin_conn, self.scratch_db);
    }
}

/// Each task card of a board response.
fn task_cards(response: &Value) -> Vec<&Value> {
    response["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .flat_map(|column| column["tasks"].as_array().expect("tasks"))
        .collect()
}

/// The size of a response on the wire: compact JSON, as the server sends
/// it.
fn bytes(value: &Value) -> usize {
    serde_json::to_string(value).expect("serializes").len()
}

#[tokio::test]
async fn the_board_and_search_send_cards_without_content_against_live_stack() {
    let stack = Stack::boot("kairos_board_cards_t0602_test").await;

    let team = stack
        .ok(
            Method::POST,
            "/api/teams",
            Some(json!({"name": "Payments", "slug": "payments", "code_prefix": "PAY"})),
        )
        .await;
    let board_id = team["delivery_board_id"]
        .as_str()
        .expect("board id")
        .to_string();
    let board = stack
        .ok(Method::GET, &format!("/api/boards/{board_id}"), None)
        .await;
    let columns: Vec<String> = board["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .filter(|column| column["is_done"] == false)
        .map(|column| column["id"].as_str().expect("column id").to_string())
        .collect();

    // The seed: CARDS tasks of about 4.8 KB each; a third of them have a
    // state line.
    let mut first_code = String::new();
    for number in 0..CARDS {
        let task = stack
            .ok(
                Method::POST,
                "/api/tasks",
                Some(json!({
                    "board_id": board_id,
                    "column_id": columns[number % columns.len()],
                    "title": format!("Task {number}: the board sends cards without content"),
                    "content": content_of(number),
                })),
            )
            .await;
        let code = task["short_code"].as_str().expect("code").to_string();
        if number % 3 == 0 {
            stack
                .ok(
                    Method::PUT,
                    &format!("/api/tasks/{code}/state"),
                    Some(json!({
                        "branch": format!("task-{number}"),
                        "pushed": true,
                        "next_step": "Run the gate, then open the PR and write the closing note.",
                    })),
                )
                .await;
        }
        if number == 0 {
            first_code = code;
        }
    }

    // --- the default: cards -------------------------------------------------
    let items_uri = format!("/api/boards/{board_id}/items?limit=1000");
    let cards = stack.ok(Method::GET, &items_uri, None).await;
    assert_eq!(cards["total"], CARDS, "{}", cards["total"]);
    let tasks = task_cards(&cards);
    assert_eq!(tasks.len(), CARDS);
    for card in &tasks {
        for field in ["content", "created_by", "updated_by", "snippet"] {
            assert!(card.get(field).is_none(), "a card has no {field}: {card}");
        }
        for field in [
            "id",
            "short_code",
            "title",
            "column_id",
            "task_type",
            "work_class",
            "version",
            "updated_at",
        ] {
            assert!(card.get(field).is_some(), "a card has {field}: {card}");
        }
    }
    assert!(
        tasks
            .iter()
            .filter(|card| card.get("state").is_some())
            .count()
            >= CARDS / 3,
        "the cards have the state line"
    );
    // The typed client decodes the cards (the CLI `boards show`).
    let typed: BoardItemsResponse = serde_json::from_value(cards.clone()).expect("cards decode");
    assert_eq!(typed.item_count(), CARDS);

    // --- fields=full: the shape before cards --------------------------------
    let full = stack
        .ok(Method::GET, &format!("{items_uri}&fields=full"), None)
        .await;
    let full_tasks = task_cards(&full);
    assert_eq!(full_tasks.len(), CARDS);
    for card in &full_tasks {
        for field in ["content", "created_by", "updated_by"] {
            assert!(card.get(field).is_some(), "a full card has {field}: {card}");
        }
    }
    let first = full_tasks
        .iter()
        .find(|card| card["short_code"] == first_code.as_str())
        .expect("the first task");
    assert_eq!(first["content"], content_of(0));
    let item = stack
        .ok(Method::GET, &format!("/api/tasks/{first_code}"), None)
        .await;
    // KAIROS-T-0630: the notes are rows. The read of one item has them in
    // `notes`; a card of a list has none, `fields=full` too.
    assert!(
        item["notes"].is_array(),
        "the read of the item has notes: {item}"
    );
    assert!(
        first.get("notes").is_none(),
        "a full card has no notes: {first}"
    );
    for (key, value) in item.as_object().expect("the task") {
        if key == "notes" {
            continue;
        }
        assert_eq!(&first[key], value, "fields=full has {key} as the item has");
    }

    // --- an unknown value is refused ----------------------------------------
    let (status, refusal) = stack
        .send(Method::GET, &format!("{items_uri}&fields=all"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");
    assert_eq!(
        refusal["error"]["details"]["parameter"], "fields",
        "{refusal}"
    );

    // --- search: a snippet, not the content ---------------------------------
    let found = stack
        .ok(
            Method::POST,
            "/api/search",
            Some(json!({"q": "quokka7", "filter": {"entity_type": ["task"]}})),
        )
        .await;
    let hits = found["results"]["tasks"].as_array().expect("task hits");
    assert_eq!(hits.len(), 1, "{found}");
    let hit = &hits[0];
    assert!(hit.get("content").is_none(), "a hit has no content: {hit}");
    let snippet = hit["snippet"].as_str().expect("a snippet");
    assert!(snippet.contains("quokka7"), "{snippet}");
    assert!(snippet.chars().count() <= 202, "{snippet}");
    let typed: SearchResponse = serde_json::from_value(found.clone()).expect("hits decode");
    assert_eq!(typed.results.tasks[0].snippet.as_deref(), Some(snippet));

    let all_hits = stack
        .ok(
            Method::POST,
            "/api/search",
            Some(json!({"filter": {"entity_type": ["task"]}, "limit": 100})),
        )
        .await;
    assert_eq!(
        all_hits["results"]["tasks"].as_array().map(Vec::len),
        Some(100)
    );

    // --- the sizes ----------------------------------------------------------
    let (card_bytes, full_bytes, search_bytes) = (bytes(&cards), bytes(&full), bytes(&all_hits));
    eprintln!(
        "KAIROS-T-0602 sizes, {CARDS} cards: board {card_bytes} bytes; board with \
         fields=full {full_bytes} bytes; search of 100 tasks {search_bytes} bytes"
    );
    assert!(
        card_bytes * 5 < full_bytes,
        "the cards are less than a fifth of the full board: {card_bytes} of {full_bytes}"
    );
    assert!(
        card_bytes < 500_000,
        "the board is under 500 KB: {card_bytes}"
    );

    stack.shutdown();
}
