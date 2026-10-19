//! Integration test for the KAIROS-T-0022 `/ws/events` channel (contract
//! per KAIROS-A-0005 §5 / KAIROS-S-0005 "Event Push"): NOTIFY emission
//! from the mutating services → the server's LISTEN task → real WebSocket
//! clients, against the LIVE compose stack (real Postgres, real Dex
//! tokens).
//!
//! The subscribed event stream runs through the typed
//! `kairos_client::EventStream` helper (KAIROS-T-0024): connect with the
//! client's token/tenant, `subscribe_board`/`subscribe_all`, typed
//! `ThinEvent`s (raw frames kept side-by-side for the wire-shape
//! assertions, e.g. "the tenant tag never reaches a client"). Raw
//! tokio-tungstenite stays ONLY for the protocol-level upgrade probes
//! (missing/garbage tokens, non-member, unknown tenant, the
//! `?access_token=` browser fallback) that the helper deliberately cannot
//! express.
//!
//! For isolation the test owns the uniquely named scratch database
//! `kairos_ws_events_t0022_test` (shared-services discipline); the server
//! is bound to an ephemeral local port because WebSocket upgrades need a
//! real connection.
//!
//! Covered: upgrade auth semantics (401 missing/garbage token, 403
//! non-member, 404 unknown tenant), the `?access_token=` browser fallback,
//! event shape for create/update/transition/delete/relationship/metadata,
//! two-tenant isolation (acme socket never sees widgets events), the
//! `board_id` subscribe filter (set and cleared), the precise kinds
//! `claim_changed`, `task_state_changed`, `task_cancelled` and
//! `board_configured` next to their coarse kinds, one `item_deleted` for
//! each item of an archive cascade and one `item_restored` for a restore
//! (KAIROS-T-0574), and clean disconnect/reconnect — every await
//! timeout-bounded.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, spawn_server, user_token,
    with_database,
};
use kairos_client::types_events::ThinEvent;
use kairos_client::types_meta::CreateMetadataDefinitionRequest;
use kairos_client::{EntityKind, Error, EventStream};
use kairos_core::short_code::ItemType;
use kairos_db::items::{ContentUpdate, CreateStrategy, CreateTask};
use kairos_db::models::{
    BoardLevel, NewOrganizationMember, NewUser, OrgRole, RelationshipType, TaskType, User,
};
use kairos_db::schema::{board_transitions, boards, organizations, users};
use kairos_db::{TenantPool, create_board, graph, items, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

/// Uniquely named scratch database for this test binary.
const SCRATCH_DB: &str = "kairos_ws_events_t0022_test";

/// Bound on every socket receive; generous because CI shares the compose
/// Postgres with concurrent work.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

type WsClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A fresh sync connection pinned to `schema` (same mechanism as the
/// pools) — the direct write path the services run on.
fn tenant_connection(scratch_url: &str, schema: &str) -> PgConnection {
    let mut conn = PgConnection::establish(scratch_url).expect("connecting to scratch database");
    sql_query(format!("SET search_path TO {schema}, public"))
        .execute(&mut conn)
        .expect("pinning search_path");
    conn
}

/// `public.users.id` by email (JIT-provisioned by a first request).
fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("user {email} not provisioned: {e}"))
}

/// The tenant board of a level in the CURRENT search_path schema.
fn board_of_level(conn: &mut PgConnection, level: BoardLevel) -> Uuid {
    boards::table
        .filter(boards::board_level.eq(level))
        .filter(boards::deleted_at.is_null())
        .select(boards::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("no {level} board: {e}"))
}

/// A column reachable from `from` per the board's transition graph.
fn transition_target(conn: &mut PgConnection, board: Uuid, from: Uuid) -> Uuid {
    board_transitions::table
        .filter(board_transitions::board_id.eq(board))
        .filter(board_transitions::from_column_id.eq(from))
        .select(board_transitions::to_column_id)
        .first(conn)
        .expect("default boards allow a move out of the first column")
}

/// Unwrap an expected API rejection (panics on success).
fn rejection<T: std::fmt::Debug>(result: Result<T, Error>) -> Error {
    match result {
        Ok(value) => panic!("expected an API rejection, got success: {value:?}"),
        Err(err) => err,
    }
}

/// Open a RAW WebSocket to the test server (protocol-level upgrade probes
/// only — the happy path goes through `KairosClient::connect_events`).
/// `auth`: `Some(token)` sets the Authorization header; the caller
/// controls everything else through `path` (query string) and `tenant`
/// (X-Tenant header).
async fn ws_connect(
    addr: std::net::SocketAddr,
    path: &str,
    auth: Option<&str>,
    tenant: Option<&str>,
) -> Result<WsClient, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}{path}")
        .into_client_request()
        .expect("client request");
    if let Some(token) = auth {
        req.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).expect("header value"),
        );
    }
    if let Some(tenant) = tenant {
        req.headers_mut()
            .insert("x-tenant", HeaderValue::from_str(tenant).expect("header"));
    }
    connect_async(req).await.map(|(socket, _)| socket)
}

/// The HTTP status a rejected raw upgrade came back with.
fn upgrade_status(err: tokio_tungstenite::tungstenite::Error) -> StatusCode {
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            StatusCode::from_u16(response.status().as_u16()).expect("status")
        }
        other => panic!("expected an HTTP rejection, got: {other:?}"),
    }
}

/// Next event from the typed helper stream, bounded by [`RECV_TIMEOUT`]:
/// the typed `ThinEvent` plus the raw frame (for wire-shape assertions).
async fn recv_event(stream: &mut EventStream) -> (ThinEvent, Value) {
    let raw = tokio::time::timeout(RECV_TIMEOUT, stream.next_event_raw())
        .await
        .expect("timed out waiting for an event")
        .expect("event frame");
    let event: ThinEvent =
        serde_json::from_value(raw.clone()).unwrap_or_else(|e| panic!("thin event {raw}: {e}"));
    (event, raw)
}

/// Next Text frame from a RAW socket as JSON (the browser-fallback probe
/// socket), bounded by [`RECV_TIMEOUT`].
async fn recv_raw_ws(socket: &mut WsClient) -> Value {
    tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str::<Value>(&text)
                        .unwrap_or_else(|e| panic!("non-JSON event {text:?}: {e}"));
                }
                Some(Ok(_)) => continue, // ping/pong
                other => panic!("socket ended while awaiting an event: {other:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for an event")
}

/// Assert the S-0005 thin-event shape (typed fields + the raw-frame
/// invariants the DTO cannot see).
fn assert_event_shape(event: &ThinEvent, raw: &Value, kind: &str, entity_type: &str) {
    assert_eq!(event.event, kind, "event kind in {raw}");
    assert_eq!(event.entity_type, entity_type, "entity_type in {raw}");
    assert!(!event.short_code.is_empty(), "short_code in {raw}");
    assert!(!event.actor.is_empty(), "actor in {raw}");
    assert!(!event.occurred_at.is_empty(), "occurred_at in {raw}");
    assert!(
        raw.get("tenant").is_none(),
        "the tenant tag must never reach a client: {raw}"
    );
}

/// The next `n` events of the items (the events of a person skipped),
/// bounded by [`RECV_TIMEOUT`] each (KAIROS-T-0574). The order of the
/// events of one write is not part of the contract, so the caller looks
/// them up by kind ([`of_kind`]).
async fn recv_item_events(stream: &mut EventStream, n: usize) -> Vec<(ThinEvent, Value)> {
    let mut events = Vec::with_capacity(n);
    while events.len() < n {
        let (event, raw) = recv_event(stream).await;
        if event.entity_type != "notification" {
            events.push((event, raw));
        }
    }
    events
}

/// The one event of `kind` in `events`; panics when there is none or more.
fn of_kind<'a>(events: &'a [(ThinEvent, Value)], kind: &str) -> &'a (ThinEvent, Value) {
    let found: Vec<&(ThinEvent, Value)> = events.iter().filter(|(e, _)| e.event == kind).collect();
    let kinds: Vec<&str> = events.iter().map(|(e, _)| e.event.as_str()).collect();
    assert_eq!(found.len(), 1, "one {kind} in {kinds:?}");
    found[0]
}

/// The column of `board` with the name `name`.
fn column_named(conn: &mut PgConnection, board: Uuid, name: &str) -> Uuid {
    use kairos_db::schema::board_columns;
    board_columns::table
        .filter(board_columns::board_id.eq(board))
        .filter(board_columns::name.eq(name))
        .filter(board_columns::deleted_at.is_null())
        .select(board_columns::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("no column {name}: {e}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_events_against_live_stack() {
    // --- scratch database + two tenants ------------------------------------
    let mut admin_conn = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    provision_tenant(&mut conn, "widgets", "Widgets Co").expect("provisioning widgets");
    let acme_org: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org row");

    // --- real server + live tokens + typed clients ---------------------------
    let http = reqwest::Client::new();
    let alice_token = user_token(&http, "alice").await;
    let svc_token = user_token(&http, "svc").await;

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
    let server = spawn_server(router).await;
    let addr = server.addr;
    let alice = server.client(&alice_token, "acme");
    let svc = server.client(&svc_token, "acme");

    // --- JIT-provision users, grant memberships ------------------------------
    for client in [&alice, &svc] {
        let err = rejection(client.whoami().await);
        assert!(matches!(err, Error::Forbidden { .. }), "{err}");
        assert_eq!(err.code(), Some("MEMBERSHIP_REQUIRED"), "{err}");
    }
    let alice_id = user_id(&mut conn, "alice@kairos.test");
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    for (user, role) in [(alice_id, OrgRole::Member), (svc_id, OrgRole::Admin)] {
        diesel::insert_into(kairos_db::schema::organization_members::table)
            .values(NewOrganizationMember {
                organization_id: acme_org,
                user_id: user,
                role,
            })
            .execute(&mut conn)
            .expect("granting acme membership");
    }
    // The widgets-side actor never authenticates; a directory row is enough.
    let widget_actor: Uuid = diesel::insert_into(users::table)
        .values(NewUser {
            external_id: "ws-widgets-actor".into(),
            user_name: "ws-widgets-actor".into(),
            email: "widgets-actor@kairos.test".into(),
            display_name: "Widgets Actor".into(),
        })
        .returning(User::as_returning())
        .get_result(&mut conn)
        .expect("inserting widgets actor")
        .id;

    // --- tenant boards -------------------------------------------------------
    let mut acme = tenant_connection(&scratch_url, "org_acme");
    // COLLIERY-T-0230: a delivery board always has a team.
    let delivery_team = common::seed_team(&mut acme, "Delivery Team", "delivery-team");
    create_board(
        &mut acme,
        BoardLevel::Delivery,
        "Delivery",
        "delivery",
        kairos_db::CodePrefix::FromSlug,
        Some(delivery_team),
        None,
    )
    .expect("acme delivery board");
    let acme_delivery = board_of_level(&mut acme, BoardLevel::Delivery);
    let acme_strategy_board = board_of_level(&mut acme, BoardLevel::Strategy);

    let mut widgets = tenant_connection(&scratch_url, "org_widgets");
    // COLLIERY-T-0230: a delivery board always has a team.
    let delivery_team = common::seed_team(&mut widgets, "Delivery Team", "delivery-team");
    create_board(
        &mut widgets,
        BoardLevel::Delivery,
        "Delivery",
        "delivery",
        kairos_db::CodePrefix::FromSlug,
        Some(delivery_team),
        None,
    )
    .expect("widgets delivery board");
    let widgets_delivery = board_of_level(&mut widgets, BoardLevel::Delivery);

    // =========================================================================
    // Upgrade auth: the standard middleware semantics apply pre-upgrade
    // (raw handshake probes — protocol-level by design)
    // =========================================================================
    let err = ws_connect(addr, "/ws/events", None, Some("acme"))
        .await
        .expect_err("unauthenticated upgrade must be rejected");
    assert_eq!(upgrade_status(err), StatusCode::UNAUTHORIZED);

    let err = ws_connect(addr, "/ws/events", Some("not.a.jwt"), Some("acme"))
        .await
        .expect_err("garbage token must be rejected");
    assert_eq!(upgrade_status(err), StatusCode::UNAUTHORIZED);

    let err = ws_connect(addr, "/ws/events", Some(&alice_token), Some("widgets"))
        .await
        .expect_err("non-member upgrade must be rejected");
    assert_eq!(upgrade_status(err), StatusCode::FORBIDDEN);

    let err = ws_connect(addr, "/ws/events", Some(&alice_token), Some("ghost"))
        .await
        .expect_err("unknown tenant must be rejected");
    assert_eq!(upgrade_status(err), StatusCode::NOT_FOUND);

    // The client helper maps the same rejections onto the typed errors.
    let err = rejection(
        server
            .client(&alice_token, "widgets")
            .connect_events()
            .await,
    );
    assert!(matches!(err, Error::Forbidden { .. }), "{err}");

    // =========================================================================
    // Event shape: create → transition → update → delete
    // =========================================================================
    let mut stream = alice
        .connect_events()
        .await
        .expect("member upgrade through the client helper succeeds");

    let task = items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "Ship the event channel",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating acme task");

    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_created", "task");
    assert_eq!(event.short_code, task.short_code.as_str());
    assert_eq!(
        event.board_id.as_deref(),
        Some(acme_delivery.to_string().as_str())
    );
    assert_eq!(
        event.column_id.as_deref(),
        Some(task.column_id.to_string().as_str())
    );
    assert_eq!(event.actor, alice_id.to_string());

    let target = transition_target(&mut acme, acme_delivery, task.column_id);
    kairos_db::transition_task(&mut acme, task.id, target, alice_id).expect("transitioning task");
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_transitioned", "task");
    assert_eq!(event.short_code, task.short_code.as_str());
    assert_eq!(
        event.column_id.as_deref(),
        Some(target.to_string().as_str()),
        "the NEW column"
    );

    items::update_item_content(
        &mut acme,
        ItemType::Task,
        task.id,
        ContentUpdate {
            new_title: None,
            new_content: "updated",
            expected_version: 1,
        },
        alice_id,
    )
    .expect("updating task content");
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_updated", "task");
    assert_eq!(event.short_code, task.short_code.as_str());

    // =========================================================================
    // item_moved: BOTH boards hear about it (KAIROS-I-0012)
    // =========================================================================
    // COLLIERY-T-0230: a delivery board always has a team.
    let second_delivery_team =
        common::seed_team(&mut acme, "Second Delivery Team", "second-delivery-team");
    create_board(
        &mut acme,
        BoardLevel::Delivery,
        "Second Delivery",
        "second-delivery",
        kairos_db::CodePrefix::FromSlug,
        Some(second_delivery_team),
        None,
    )
    .expect("a second delivery board to move to");
    let second_delivery: Uuid = {
        use kairos_db::schema::boards;
        boards::table
            .filter(boards::slug.eq("second-delivery"))
            .select(boards::id)
            .first(&mut acme)
            .expect("second delivery board")
    };
    kairos_db::boards::move_task(&mut acme, task.id, second_delivery, alice_id)
        .expect("moving the task");
    // The board it LEFT, so that board's subscribers drop the card …
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_moved", "task");
    assert_eq!(event.short_code, task.short_code.as_str());
    assert_eq!(
        event.board_id.as_deref(),
        Some(acme_delivery.to_string().as_str()),
        "first the source board"
    );
    // … then the board it JOINED, with its new placement.
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_moved", "task");
    assert_eq!(
        event.board_id.as_deref(),
        Some(second_delivery.to_string().as_str()),
        "then the target board"
    );
    assert!(event.column_id.is_some(), "carrying the entry column");
    // Put it back so the rest of the walk sees the task where it was.
    kairos_db::boards::move_task(&mut acme, task.id, acme_delivery, alice_id)
        .expect("moving it back");
    for _ in 0..2 {
        let (event, raw) = recv_event(&mut stream).await;
        assert_event_shape(&event, &raw, "item_moved", "task");
    }

    // =========================================================================
    // relationship_changed: one event per endpoint
    // =========================================================================
    let task2 = items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "Blocked work",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating second acme task");
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(event.event, "item_created");

    graph::link_items(
        &mut acme,
        task.id,
        task2.id,
        RelationshipType::Blocks,
        alice_id,
    )
    .expect("linking tasks");
    let (first, first_raw) = recv_event(&mut stream).await;
    let (second, second_raw) = recv_event(&mut stream).await;
    for (event, raw) in [(&first, &first_raw), (&second, &second_raw)] {
        assert_event_shape(event, raw, "relationship_changed", "task");
    }
    let codes: Vec<&str> = [&first, &second]
        .iter()
        .map(|e| e.short_code.as_str())
        .collect();
    assert!(codes.contains(&task.short_code.as_str()), "{codes:?}");
    assert!(codes.contains(&task2.short_code.as_str()), "{codes:?}");

    // =========================================================================
    // metadata_changed: through the typed REST client (svc is org admin)
    // =========================================================================
    svc.create_metadata_definition(&CreateMetadataDefinitionRequest {
        name: "WS Priority".into(),
        slug: "ws_priority".into(),
        field_type: "string".into(),
        enum_options: vec![],
        entity_types: vec![],
    })
    .await
    .expect("creating ws_priority definition");
    svc.update_metadata(
        EntityKind::Task,
        &task.short_code,
        &kairos_client::types_meta::UpdateMetadataRequest {
            values: std::collections::BTreeMap::from([(
                "ws_priority".to_string(),
                Some("high".to_string()),
            )]),
        },
    )
    .await
    .expect("setting ws_priority");
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "metadata_changed", "task");
    assert_eq!(event.short_code, task.short_code.as_str());
    assert_eq!(event.actor, svc_id.to_string());

    // =========================================================================
    // Two-tenant isolation: widgets mutations never reach the acme socket
    // =========================================================================
    items::create_task(
        &mut widgets,
        CreateTask {
            board_id: widgets_delivery,
            column_id: None,
            title: "Widgets-only work",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        widget_actor,
    )
    .expect("creating widgets task");
    // Deterministic ordering probe: the widgets event was emitted BEFORE
    // this acme delete, so if it were (wrongly) delivered it would arrive
    // first. The next frame must be the acme delete.
    items::soft_delete_item(&mut acme, ItemType::Task, task2.id, alice_id)
        .expect("deleting second acme task");
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_deleted", "task");
    assert_eq!(
        event.short_code,
        task2.short_code.as_str(),
        "the widgets event must never be delivered to an acme socket"
    );

    // =========================================================================
    // board_id subscribe filter: set, honored, cleared
    // =========================================================================
    stream
        .subscribe_board(&acme_strategy_board.to_string())
        .await
        .expect("sending subscribe");
    // Give the server a beat to apply the filter before mutating.
    tokio::time::sleep(Duration::from_millis(300)).await;

    items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "Filtered out",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating filtered-out task");
    let strategy = items::create_strategy(
        &mut acme,
        CreateStrategy {
            board_id: acme_strategy_board,
            column_id: None,
            title: "Strategy behind the filter",
            content: "…",
            hypothesis: None,
        },
        alice_id,
    )
    .expect("creating strategy");
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_created", "strategy");
    assert_eq!(
        event.short_code,
        strategy.short_code.as_str(),
        "the delivery-board event must be filtered out"
    );
    assert_eq!(
        event.board_id.as_deref(),
        Some(acme_strategy_board.to_string().as_str())
    );

    stream.subscribe_all().await.expect("clearing the filter");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let task3 = items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "Filter cleared",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating post-clear task");
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(event.event, "item_created");
    assert_eq!(event.short_code, task3.short_code.as_str());

    // =========================================================================
    // Browser fallback: ?access_token= with no Authorization header (raw
    // socket — the client helper always authenticates via the header)
    // =========================================================================
    let mut browser_socket = ws_connect(
        addr,
        &format!("/ws/events?access_token={alice_token}"),
        None,
        Some("acme"),
    )
    .await
    .expect("query-param token upgrade succeeds");

    let task4 = items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "Fan-out to both sockets",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating fan-out task");
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(event.event, "item_created");
    assert_eq!(event.short_code, task4.short_code.as_str());
    let raw = recv_raw_ws(&mut browser_socket).await;
    assert_eq!(raw["event"], "item_created");
    assert_eq!(raw["short_code"], task4.short_code.as_str());

    // =========================================================================
    // COLLIERY-T-0269: an `impacts` link and a change of the owner board of
    // a document emit relationship_changed for the document, so an open
    // page of the document reads it again
    // =========================================================================
    let events_team = common::seed_team(&mut acme, "Events", "events");
    let repository = kairos_db::repositories::create(
        &mut acme,
        kairos_db::models::repositories::NewRepository {
            slug: "fidius".into(),
            forge: kairos_db::models::enums::Forge::Github,
            repo_full_name: "acme/fidius".into(),
            repo_url: "https://github.com/acme/fidius".into(),
            default_branch: "main".into(),
            team_id: events_team,
            description: String::new(),
            created_by: alice_id,
            updated_by: alice_id,
        },
    )
    .expect("creating the repository");
    let vision = items::create_document(
        &mut acme,
        kairos_db::items::CreateDocument {
            board_id: acme_strategy_board,
            title: "The vision of fidius",
            content: Some("…"),
            template_id: None,
        },
        alice_id,
    )
    .expect("creating the document");
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_created", "document");
    assert_eq!(event.short_code, vision.short_code.as_str());
    assert_eq!(
        event.board_id, None,
        "a document that names a board is not a card of that board: {raw}"
    );

    let document_changed = |event: &ThinEvent, raw: &Value, what: &str| {
        assert_event_shape(event, raw, "relationship_changed", "document");
        assert_eq!(event.short_code, vision.short_code.as_str(), "{what}");
        assert_eq!(event.board_id, None, "{what}: {raw}");
        assert_eq!(event.actor, alice_id.to_string(), "{what}");
    };
    kairos_db::impacts::link(&mut acme, vision.id, repository.id, alice_id)
        .expect("the impacts link");
    let (event, raw) = recv_event(&mut stream).await;
    document_changed(&event, &raw, "the add of an impacts link");

    let change = items::set_document_board(&mut acme, vision.id, acme_delivery, alice_id)
        .expect("the change of the owner board");
    assert!(change.changed);
    let (event, raw) = recv_event(&mut stream).await;
    document_changed(&event, &raw, "the change of the owner board");

    // The board that the document has: no write, and no event. The next
    // event is that of the remove of the link.
    let again = items::set_document_board(&mut acme, vision.id, acme_delivery, alice_id)
        .expect("the same owner board");
    assert!(!again.changed);
    kairos_db::impacts::unlink(&mut acme, vision.id, repository.id, alice_id)
        .expect("the remove of the impacts link");
    let (event, raw) = recv_event(&mut stream).await;
    document_changed(&event, &raw, "the remove of an impacts link");

    // =========================================================================
    // KAIROS-T-0574: the precise kinds. Each comes in addition to the coarse
    // kind of the same write, so a subscriber of the coarse kinds keeps
    // working.
    // =========================================================================
    let todo = column_named(&mut acme, acme_delivery, "Todo");
    let active = column_named(&mut acme, acme_delivery, "Active");
    let task6 = items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "The precise kinds",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating the task of the precise kinds");
    let code6 = task6.short_code.as_str();
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(event.event, "item_created");
    kairos_db::transition_task(&mut acme, task6.id, todo, alice_id).expect("to Todo");
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(event.event, "item_transitioned");
    assert!(event.change.is_none(), "no change on a coarse kind");

    // A person moves the task into Active: the claim is taken.
    kairos_db::transition_task(&mut acme, task6.id, active, alice_id).expect("to Active");
    let events = recv_item_events(&mut stream, 2).await;
    of_kind(&events, "item_transitioned");
    let (event, raw) = of_kind(&events, "claim_changed");
    assert_event_shape(event, raw, "claim_changed", "task");
    assert_eq!(event.short_code, code6);
    assert_eq!(event.change.as_deref(), Some("taken"), "{raw}");
    assert_eq!(
        event.board_id.as_deref(),
        Some(acme_delivery.to_string().as_str())
    );
    assert_eq!(event.actor, alice_id.to_string());

    // A write of the state: item_updated, then task_state_changed.
    alice
        .set_task_state(
            code6,
            &kairos_client::types::SetTaskStateRequest {
                branch: Some(Some("t-0574".into())),
                next_step: Some(Some("Write the tests".into())),
                ..Default::default()
            },
        )
        .await
        .expect("writing the state");
    let events = recv_item_events(&mut stream, 2).await;
    of_kind(&events, "item_updated");
    let (event, raw) = of_kind(&events, "task_state_changed");
    assert_event_shape(event, raw, "task_state_changed", "task");
    assert_eq!(event.short_code, code6);
    assert!(raw.get("change").is_none(), "{raw}");

    // A hand-off: the claim goes to svc, and the next step is cleared.
    kairos_db::task_claims::hand_off(&mut acme, task6.id, svc_id, alice_id).expect("hand-off");
    let events = recv_item_events(&mut stream, 3).await;
    of_kind(&events, "item_updated");
    of_kind(&events, "task_state_changed");
    let (event, _) = of_kind(&events, "claim_changed");
    assert_eq!(event.change.as_deref(), Some("handed_off"));

    // A release: the next step is clear already, so no state event.
    kairos_db::task_claims::release(&mut acme, task6.id, alice_id).expect("release");
    let events = recv_item_events(&mut stream, 2).await;
    of_kind(&events, "item_updated");
    let (event, _) = of_kind(&events, "claim_changed");
    assert_eq!(event.change.as_deref(), Some("released"));

    // A claim of the free task.
    kairos_db::task_claims::claim(&mut acme, task6.id, alice_id).expect("claim");
    let events = recv_item_events(&mut stream, 2).await;
    of_kind(&events, "item_updated");
    let (event, _) = of_kind(&events, "claim_changed");
    assert_eq!(event.change.as_deref(), Some("taken"));

    // A cancel: item_transitioned, task_cancelled, and the claim ends.
    let cancelled = kairos_db::task_cancellations::cancel_task(
        &mut acme,
        task6.id,
        "The plan changed",
        alice_id,
    )
    .expect("cancel");
    let done = column_named(&mut acme, acme_delivery, &cancelled.to_column);
    let events = recv_item_events(&mut stream, 3).await;
    of_kind(&events, "item_transitioned");
    let (event, raw) = of_kind(&events, "task_cancelled");
    assert_event_shape(event, raw, "task_cancelled", "task");
    assert_eq!(event.short_code, code6);
    assert_eq!(
        event.column_id.as_deref(),
        Some(done.to_string().as_str()),
        "the done column"
    );
    let (event, _) = of_kind(&events, "claim_changed");
    assert_eq!(event.change.as_deref(), Some("ended"));

    // A change of the configuration of a board.
    kairos_db::boards::set_column_wip_limit(&mut acme, active, Some(3), svc_id)
        .expect("a WIP limit");
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "board_configured", "board");
    assert_eq!(event.short_code, "delivery", "the slug of the board");
    assert_eq!(
        event.board_id.as_deref(),
        Some(acme_delivery.to_string().as_str())
    );
    assert_eq!(event.change.as_deref(), Some("column_wip_limit"));
    assert!(event.column_id.is_none(), "{raw}");
    assert_eq!(event.actor, svc_id.to_string());

    // =========================================================================
    // KAIROS-T-0574: an archive cascade emits item_deleted for each item,
    // with its own board. A socket filtered to the delivery board hears that
    // its task went with an initiative.
    // =========================================================================
    let acme_initiative_board = board_of_level(&mut acme, BoardLevel::Initiative);
    stream
        .subscribe_board(&acme_delivery.to_string())
        .await
        .expect("filter to the delivery board");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let initiative = items::create_initiative(
        &mut acme,
        kairos_db::items::CreateInitiative {
            board_id: acme_initiative_board,
            column_id: None,
            title: "The initiative of the cascade",
            content: "…",
            complexity: None,
            bucket_type: None,
        },
        alice_id,
    )
    .expect("creating the initiative");
    let child = items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "A child of the initiative",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating the child task");
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(
        event.event, "item_created",
        "the initiative is filtered out"
    );
    assert_eq!(event.short_code, child.short_code.as_str());
    graph::link_items(
        &mut acme,
        initiative.id,
        child.id,
        RelationshipType::Parent,
        alice_id,
    )
    .expect("the parent edge");
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(event.event, "relationship_changed");
    assert_eq!(event.short_code, child.short_code.as_str());

    let outcome = items::soft_delete_item(&mut acme, ItemType::Initiative, initiative.id, alice_id)
        .expect("archiving the initiative");
    assert_eq!(outcome.cascaded_short_codes, [child.short_code.as_str()]);
    let (event, raw) = recv_event(&mut stream).await;
    assert_event_shape(&event, &raw, "item_deleted", "task");
    assert_eq!(event.short_code, child.short_code.as_str(), "{raw}");
    assert_eq!(
        event.board_id.as_deref(),
        Some(acme_delivery.to_string().as_str()),
        "the board of the descendant"
    );

    // A restore puts back the initiative only: one item_restored, and none
    // for the child that stays archived.
    stream.subscribe_all().await.expect("clearing the filter");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let restored = items::restore_item(&mut acme, ItemType::Initiative, initiative.id, alice_id)
        .expect("restoring the initiative")
        .expect("nothing blocks the restore");
    assert_eq!(
        restored.still_archived_descendants,
        [child.short_code.as_str()]
    );
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(event.event, "item_restored");
    assert_eq!(event.short_code, initiative.short_code.as_str());
    let probe = items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "The probe after the restore",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating the probe");
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(
        event.event, "item_created",
        "no event for the archived child"
    );
    assert_eq!(event.short_code, probe.short_code.as_str());

    // =========================================================================
    // Clean disconnect / reconnect (timeout-bounded)
    // =========================================================================
    stream.close().await.expect("closing the helper stream");
    futures_util::SinkExt::close(&mut browser_socket)
        .await
        .expect("closing browser socket");
    drop(browser_socket);

    let mut stream = tokio::time::timeout(RECV_TIMEOUT, alice.connect_events())
        .await
        .expect("reconnect within the timeout")
        .expect("reconnect upgrade succeeds");
    let task5 = items::create_task(
        &mut acme,
        CreateTask {
            board_id: acme_delivery,
            column_id: None,
            title: "After reconnect",
            content: "…",
            task_type: TaskType::Task,
            work_class: kairos_db::models::enums::WorkClass::Planned,
            repository_id: None,
        },
        alice_id,
    )
    .expect("creating post-reconnect task");
    let (event, _) = recv_event(&mut stream).await;
    assert_eq!(event.event, "item_created");
    assert_eq!(
        event.short_code,
        task5.short_code.as_str(),
        "no replay: only events emitted after the reconnect arrive"
    );
    stream.close().await.expect("final close");

    // --- teardown ------------------------------------------------------------
    drop(conn);
    drop(acme);
    drop(widgets);
    drop(pool);
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}

/// The scratch database of [`ws_person_only_and_lagged`].
const LAGGED_DB: &str = "kairos_ws_lagged_t0621_test";

/// Send `NOTIFY kairos_events` as the services do, with a raw payload.
fn notify(conn: &mut PgConnection, payload: &Value) {
    sql_query(format!(
        "SELECT pg_notify('kairos_events', '{}')",
        payload.to_string().replace('\'', "''")
    ))
    .execute(conn)
    .expect("pg_notify");
}

/// KAIROS-T-0621: the filter `person_only` passes only the events of the
/// person. A board filter also passes them. A socket that falls behind
/// gets the frame `{"event":"lagged"}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_person_only_and_lagged() {
    let mut admin_conn = recreate_scratch_db(LAGGED_DB);
    let scratch_url = with_database(&common::admin_database_url(), LAGGED_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    let acme_org: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org row");

    let http = reqwest::Client::new();
    let alice_token = user_token(&http, "alice").await;
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
    let server = spawn_server(router).await;
    let alice = server.client(&alice_token, "acme");
    let _ = alice.whoami().await;
    let alice_id = user_id(&mut conn, "alice@kairos.test");
    diesel::insert_into(kairos_db::schema::organization_members::table)
        .values(NewOrganizationMember {
            organization_id: acme_org,
            user_id: alice_id,
            role: OrgRole::Member,
        })
        .execute(&mut conn)
        .expect("granting acme membership");
    let mut acme = tenant_connection(&scratch_url, "org_acme");
    let board = board_of_level(&mut acme, BoardLevel::Strategy);

    let item_event = |code: &str| {
        serde_json::json!({
            "tenant": "acme", "event": "item_updated", "entity_type": "strategy",
            "short_code": code, "board_id": board.to_string(),
            "actor": alice_id.to_string(), "occurred_at": "2026-10-18T12:00:00Z",
        })
    };
    let bell_event = |code: &str| {
        serde_json::json!({
            "tenant": "acme", "recipient": alice_id.to_string(),
            "event": "notification_created", "entity_type": "notification",
            "short_code": code, "board_id": null,
            "actor": alice_id.to_string(), "occurred_at": "2026-10-18T12:00:00Z",
        })
    };

    // =========================================================================
    // person_only: the events of the person, no event of the items
    // =========================================================================
    let mut bell = alice.connect_events().await.expect("bell socket");
    bell.subscribe_person()
        .await
        .expect("subscribe person_only");
    let mut by_board = alice.connect_events().await.expect("board socket");
    by_board
        .subscribe_board(&board.to_string())
        .await
        .expect("subscribe board");
    tokio::time::sleep(Duration::from_millis(300)).await;

    notify(&mut conn, &item_event("ACME-S-0001"));
    notify(&mut conn, &bell_event("ACME-S-0002"));
    let (event, raw) = recv_event(&mut bell).await;
    assert_eq!(
        event.event, "notification_created",
        "the item event is filtered out: {raw}"
    );
    assert_eq!(event.short_code, "ACME-S-0002");
    assert!(
        raw.get("recipient").is_none(),
        "recipient is stripped: {raw}"
    );

    let (first, _) = recv_event(&mut by_board).await;
    let (second, _) = recv_event(&mut by_board).await;
    assert_eq!(
        (first.event.as_str(), second.event.as_str()),
        ("item_updated", "notification_created"),
        "the board filter passes its board and the person"
    );
    bell.close().await.expect("close bell");
    by_board.close().await.expect("close board socket");

    // =========================================================================
    // lagged: a socket that does not read falls behind the buffer
    // =========================================================================
    let mut slow = alice.connect_events().await.expect("slow socket");
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Big unique payloads: the socket buffers fill, and the server stops
    // reading the channel of the tenant. 4000 events pass its buffer.
    sql_query(format!(
        "SELECT pg_notify('kairos_events', json_build_object(
             'tenant', 'acme', 'event', 'item_updated', 'entity_type', 'strategy',
             'short_code', 'ACME-S-' || g, 'board_id', '{board}',
             'actor', '{alice_id}', 'occurred_at', '2026-10-18T12:00:00Z',
             'pad', repeat('x', 7000))::text)
         FROM generate_series(1, 4000) AS g"
    ))
    .execute(&mut conn)
    .expect("pg_notify flood");
    // Let the listener pass the events before the socket reads again.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut frames = 0usize;
    let lagged = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let raw = slow.next_event_raw().await.expect("frame");
            frames += 1;
            if raw["event"] == "lagged" {
                return raw;
            }
        }
    })
    .await
    .expect("the lagged frame within the timeout");
    assert_eq!(lagged, serde_json::json!({ "event": "lagged" }));
    assert!(frames < 4000, "events were skipped: {frames} frames");

    // The socket stays open after the frame.
    notify(&mut conn, &item_event("ACME-S-9999"));
    let found = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let raw = slow.next_event_raw().await.expect("frame");
            if raw["short_code"] == "ACME-S-9999" {
                return;
            }
        }
    })
    .await;
    assert!(found.is_ok(), "the socket gets the events after the lag");
    slow.close().await.expect("close slow socket");

    drop(conn);
    drop(acme);
    drop(pool);
    drop_scratch_db(&mut admin_conn, LAGGED_DB);
}

/// The scratch database of [`ws_every_kind_through_the_outbox`].
const OUTBOX_DB: &str = "kairos_ws_outbox_t0632_test";

/// KAIROS-T-0632: each event kind goes through the outbox and the emitter
/// of a server, and reaches the socket. Two servers run on one database,
/// as two processes do: each socket gets each event once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_every_kind_through_the_outbox() {
    use kairos_db::events::{self, EventKind};

    let mut admin_conn = recreate_scratch_db(OUTBOX_DB);
    let scratch_url = with_database(&common::admin_database_url(), OUTBOX_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    let acme_org: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org row");

    let http = reqwest::Client::new();
    let alice_token = user_token(&http, "alice").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let mut servers = Vec::new();
    for _ in 0..2 {
        let router = app::router(app::state_with(
            base_config(&scratch_url),
            pool.clone(),
            Arc::clone(&auth),
        ));
        servers.push(spawn_server(router).await);
    }
    let alice = servers[0].client(&alice_token, "acme");
    let _ = alice.whoami().await;
    let alice_id = user_id(&mut conn, "alice@kairos.test");
    diesel::insert_into(kairos_db::schema::organization_members::table)
        .values(NewOrganizationMember {
            organization_id: acme_org,
            user_id: alice_id,
            role: OrgRole::Member,
        })
        .execute(&mut conn)
        .expect("granting acme membership");
    let mut acme = tenant_connection(&scratch_url, "org_acme");
    let board = board_of_level(&mut acme, BoardLevel::Strategy);

    let mut sockets = Vec::new();
    for server in &servers {
        let client = server.client(&alice_token, "acme");
        let mut socket = client.connect_events().await.expect("socket");
        socket.subscribe_all().await.expect("subscribe");
        sockets.push(socket);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Each kind of the items, the boards and the repositories, in its own
    // committed write.
    let mut expected: Vec<String> = Vec::new();
    for (n, kind) in EventKind::ALL.into_iter().enumerate() {
        let short_code = format!("ACME-K-{n:04}");
        events::emit_event(
            &mut acme,
            &events::ThinEvent {
                event: kind,
                entity_type: "strategy".into(),
                short_code: short_code.clone(),
                board_id: Some(board),
                column_id: None,
                actor: alice_id,
            },
        )
        .expect("emit");
        expected.push(format!("{}:{short_code}", kind.as_str()));
    }
    // The events of a person: a notification, a read, and one event for
    // 150 persons (two rows of the outbox; alice is in the second).
    let told = kairos_db::notifications::notify(
        &mut acme,
        &[alice_id],
        kairos_core::notifications::NotificationKind::P0Bug,
        None,
        "A critical bug",
        Uuid::nil(),
    )
    .expect("notify");
    assert_eq!(told, vec![alice_id]);
    expected.push(format!("{}:", events::NOTIFICATION_CREATED));
    events::emit_notification_event(
        &mut acme,
        events::NOTIFICATIONS_READ,
        &[alice_id],
        "",
        alice_id,
    )
    .expect("read");
    expected.push(format!("{}:", events::NOTIFICATIONS_READ));
    let mut many: Vec<Uuid> = (0..149).map(|_| Uuid::new_v4()).collect();
    many.insert(120, alice_id);
    events::emit_notification_event(
        &mut acme,
        events::NOTIFICATION_CREATED,
        &many,
        "ACME-K-MANY",
        Uuid::nil(),
    )
    .expect("many");
    expected.push(format!("{}:ACME-K-MANY", events::NOTIFICATION_CREATED));
    expected.sort();

    for (n, socket) in sockets.iter_mut().enumerate() {
        let mut got = Vec::new();
        while got.len() < expected.len() {
            let (event, raw) = recv_event(socket).await;
            for routing in ["tenant", "recipients", "recipient", "blocks_boards"] {
                assert!(
                    raw.get(routing).is_none(),
                    "{routing} reaches a client: {raw}"
                );
            }
            got.push(format!("{}:{}", event.event, event.short_code));
        }
        got.sort();
        assert_eq!(got, expected, "server {n}: each kind, each once");
    }
    // No event comes twice: after one more pass of each emitter, a marker
    // is the next frame of each socket.
    tokio::time::sleep(Duration::from_millis(500)).await;
    events::emit_event(
        &mut acme,
        &events::ThinEvent {
            event: EventKind::ItemUpdated,
            entity_type: "strategy".into(),
            short_code: "ACME-K-LAST".into(),
            board_id: Some(board),
            column_id: None,
            actor: alice_id,
        },
    )
    .expect("marker");
    for socket in &mut sockets {
        let (event, raw) = recv_event(socket).await;
        assert_eq!(
            event.short_code, "ACME-K-LAST",
            "no duplicate before it: {raw}"
        );
    }
    let unsent: i64 = diesel::select(diesel::dsl::sql::<diesel::sql_types::BigInt>(
        "(SELECT COUNT(*) FROM public.event_outbox WHERE sent_at IS NULL)",
    ))
    .get_result(&mut conn)
    .expect("unsent rows");
    assert_eq!(unsent, 0, "the emitters sent each row");

    for socket in sockets {
        socket.close().await.expect("close");
    }
    drop(conn);
    drop(acme);
    drop(pool);
    drop_scratch_db(&mut admin_conn, OUTBOX_DB);
}
