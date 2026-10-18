//! A key that expires within 7 days gets one `key_expiring` notification
//! (KAIROS-T-0540, COLLIERY-I-0642):
//!
//! 1. The periodic pass `key-expiring` runs as a job. The person of an
//!    agent key that expires within 7 days is told. The text names the key
//!    and `kairos keys rotate`.
//! 2. For a key of a service account, each organization admin is told, not
//!    a member.
//! 3. Not a key with no expiry, not a key that expires later, not a revoked
//!    key, not a key that expired, not a key that a rotation replaced. The
//!    new key of a rotation is told in its turn.
//! 4. A second pass tells nobody: each key is told once.
//! 5. A person who turned the kind off is not told.
//! 6. The notification rides along on an MCP answer.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_key_expiring_t0540_test`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_types::{Text, Uuid as SqlUuid};
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::types_notifications::UpdateNotificationSettingsRequest;
use kairos_client::types_service_accounts::{CreateApiKeyRequest, CreateServiceAccountRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::blocking::BlockingTenantPool;
use kairos_server::jobs::{JobSettings, Worker};
use kairos_server::middleware::auth::Authenticator;
use kairos_server::periodic;

const SCRATCH_DB: &str = "kairos_key_expiring_t0540_test";

#[derive(QueryableByName, Debug)]
struct Note {
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = Text)]
    text: String,
}

/// The `key_expiring` notifications, as (person, text), sorted.
fn notes(conn: &mut PgConnection) -> Vec<(Uuid, String)> {
    let rows: Vec<Note> = diesel::sql_query(
        "SELECT user_id, text FROM org_acme.notifications WHERE kind = 'key_expiring'",
    )
    .load(conn)
    .expect("notifications");
    let mut out: Vec<(Uuid, String)> = rows.into_iter().map(|n| (n.user_id, n.text)).collect();
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

/// A key request that expires `days` days from now, or never.
fn key_request(name: &str, days: Option<i64>) -> CreateApiKeyRequest {
    CreateApiKeyRequest {
        name: name.into(),
        expires_at: days.map(|d| {
            (Utc::now() + chrono::Duration::days(d)).to_rfc3339_opts(SecondsFormat::Secs, true)
        }),
        no_expiry: days.is_none(),
    }
}

/// Run the pass `key-expiring` once as a periodic job.
async fn run_pass_as_job(url: &str) {
    let worker = Arc::new(Worker::new(
        BlockingTenantPool::new(url, 8),
        Uuid::new_v4(),
        JobSettings {
            heartbeat: Duration::from_millis(100),
            lease: Duration::from_millis(600),
            max_tries: 3,
            poll: Duration::from_millis(20),
            retry_after: Duration::ZERO,
            ..JobSettings::default()
        },
    ));
    worker
        .add_periodic(
            periodic::KEY_EXPIRING,
            periodic::KEY_EXPIRING_INTERVAL,
            periodic::key_expiring_pass(url.to_string()),
        )
        .await
        .expect("add")
        .expect("the first run");
    worker.drain().await;
}

/// One JSON-RPC message of an MCP response: plain JSON or SSE `data:`.
fn rpc_message(body: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && value.get("jsonrpc").is_some()
    {
        return value;
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|value| value.get("jsonrpc").is_some())
        .unwrap_or_else(|| panic!("no JSON-RPC message in {body:?}"))
}

/// The text of the answer to `whoami` over MCP with `token`: each text
/// block, joined.
async fn mcp_whoami(base_url: &str, token: &str) -> String {
    let http = reqwest::Client::new();
    let mut session: Option<String> = None;
    let mut last = String::new();
    for body in [
        json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "kairos-t0540", "version": "0.0.0"},
            },
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "whoami", "arguments": {}},
        }),
    ] {
        let mut request = http
            .post(format!("{base_url}/mcp"))
            .bearer_auth(token)
            .header("x-tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(id) = &session {
            request = request.header("mcp-session-id", id);
        }
        let response = request.send().await.expect("MCP request");
        assert!(response.status().is_success(), "MCP {body}");
        if let Some(id) = response.headers().get("mcp-session-id") {
            session = Some(id.to_str().expect("session id").to_string());
        }
        last = response.text().await.expect("MCP body");
    }
    let result = rpc_message(&last)["result"].clone();
    result["content"]
        .as_array()
        .unwrap_or_else(|| panic!("whoami gave no content: {result}"))
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_that_expires_within_7_days_is_told_once() {
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
    let state = app::state_with(base_config(&scratch_url), pool, auth);
    let server = spawn_server(app::router(state)).await;
    let admin = server.client(&svc_token, "acme");
    let alice = server.client(&alice_token, "acme");
    let bob = server.client(&bob_token, "acme");
    let carol = server.client(&carol_token, "acme");
    for client in [&admin, &alice, &bob, &carol] {
        let _ = client.whoami().await;
    }
    // svc and bob are admins; alice and carol are members.
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
        ("bob@kairos.test", OrgRole::Admin),
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
    let svc_id = user(&mut conn, "svc@kairos.test");
    let alice_id = user(&mut conn, "alice@kairos.test");

    // The agent keys of alice.
    let soon = alice
        .create_agent_key(&key_request("soon", Some(3)))
        .await
        .expect("soon");
    let later = alice
        .create_agent_key(&key_request("later", Some(30)))
        .await
        .expect("later");
    let forever = alice
        .create_agent_key(&key_request("forever", None))
        .await
        .expect("forever");
    assert_eq!(forever.expires_at, None);
    let revoked = alice
        .create_agent_key(&key_request("revoked", Some(3)))
        .await
        .expect("revoked");
    alice.revoke_agent_key(&revoked.id).await.expect("revoke");
    let rotated = alice
        .create_agent_key(&key_request("rotated", Some(5)))
        .await
        .expect("rotated");
    let rotation = alice.rotate_agent_key(&rotated.id).await.expect("rotate");
    assert_eq!(rotation.replaced_key_id, rotated.id);
    let expired = alice
        .create_agent_key(&key_request("expired", Some(30)))
        .await
        .expect("expired");
    diesel::sql_query(
        "UPDATE org_acme.api_keys SET expires_at = now() - interval '1 hour' WHERE id = $1",
    )
    .bind::<SqlUuid, _>(Uuid::parse_str(&expired.id).expect("id"))
    .execute(&mut conn)
    .expect("expire the key");
    // carol turned the kind off.
    let off = UpdateNotificationSettingsRequest {
        key_expiring: Some(false),
        ..Default::default()
    };
    let settings = carol
        .update_notification_settings(&off)
        .await
        .expect("carol off");
    assert!(!settings.key_expiring && settings.claim_stale);
    carol
        .create_agent_key(&key_request("carol-soon", Some(2)))
        .await
        .expect("carol key");
    // A service account with a key that expires soon, and one that does not.
    let account = admin
        .create_service_account(&CreateServiceAccountRequest { name: "ci".into() })
        .await
        .expect("service account");
    let sa_key = admin
        .create_api_key(&account.id, &key_request("deploy", Some(6)))
        .await
        .expect("sa key");
    admin
        .create_api_key(&account.id, &key_request("nightly", Some(20)))
        .await
        .expect("sa key later");
    assert!(notes(&mut conn).is_empty(), "nothing before the pass");

    // --- 1, 2, 3. The pass as a job -------------------------------------------
    run_pass_as_job(&scratch_url).await;
    let told = notes(&mut conn);
    let alice_notes: Vec<&String> = told
        .iter()
        .filter(|(u, _)| *u == alice_id)
        .map(|(_, t)| t)
        .collect();
    assert_eq!(
        alice_notes.len(),
        2,
        "soon and the new rotated key: {told:?}"
    );
    let soon_note = alice_notes
        .iter()
        .find(|t| t.contains(" soon "))
        .unwrap_or_else(|| panic!("the key soon: {alice_notes:?}"));
    assert!(
        soon_note.starts_with("Your agent key soon ("),
        "{soon_note}"
    );
    assert!(soon_note.contains(&soon.prefix), "{soon_note}");
    assert!(
        soon_note.contains(&format!("kairos keys rotate {}", soon.id)),
        "{soon_note}"
    );
    let rotated_note = alice_notes
        .iter()
        .find(|t| t.contains(" rotated "))
        .unwrap_or_else(|| panic!("the new key of the rotation: {alice_notes:?}"));
    assert!(
        rotated_note.contains(&format!("kairos keys rotate {}", rotation.id)),
        "the new key, not the old one: {rotated_note}"
    );
    for (key, name) in [
        (&later.id, "later"),
        (&forever.id, "forever"),
        (&revoked.id, "revoked"),
        (&rotated.id, "replaced"),
        (&expired.id, "expired"),
    ] {
        assert!(
            told.iter().all(|(_, t)| !t.contains(key.as_str())),
            "the key {name} is not told: {told:?}"
        );
    }
    // The key of the service account: svc is told; bob is an admin too,
    // and is told; alice and carol are members, not told.
    let sa_notes: Vec<&(Uuid, String)> = told
        .iter()
        .filter(|(_, t)| t.contains("service account ci"))
        .collect();
    let bob_id = user(&mut conn, "bob@kairos.test");
    let mut sa_told: Vec<Uuid> = sa_notes.iter().map(|(u, _)| *u).collect();
    sa_told.sort();
    let mut admins = vec![svc_id, bob_id];
    admins.sort();
    assert_eq!(sa_told, admins, "the admins: {told:?}");
    let sa_text = &sa_notes[0].1;
    assert!(sa_text.contains("The key deploy ("), "{sa_text}");
    assert!(
        sa_text.contains(&format!(
            "kairos keys rotate {} --service-account {}",
            sa_key.id, account.id
        )),
        "{sa_text}"
    );
    assert!(told.iter().all(|(_, t)| !t.contains("nightly")), "{told:?}");
    // --- 5. carol turned the kind off ------------------------------------------
    let carol_id = user(&mut conn, "carol@kairos.test");
    assert!(told.iter().all(|(u, _)| *u != carol_id), "{told:?}");
    assert_eq!(told.len(), 4, "{told:?}");
    // carol's key is marked, so turning the kind on does not tell it later.
    let marked: i64 = diesel::sql_query(
        "SELECT count(*) AS n FROM org_acme.api_keys WHERE expiring_notified_at IS NOT NULL",
    )
    .get_result::<Count>(&mut conn)
    .expect("count")
    .n;
    assert_eq!(marked, 4, "soon, the new rotated key, carol-soon, deploy");

    // --- 4. Once for each key ---------------------------------------------------
    let again =
        kairos_db::api_keys::key_expiring_pass(&mut conn, Utc::now(), &mut || {}).expect("pass");
    // The next run of the job is due one interval later, so the second
    // pass runs on the connection.
    assert_eq!(again, 0);
    assert_eq!(notes(&mut conn), told, "no second notification");

    // --- 6. It rides along on MCP, and REST lists it ----------------------------
    let list = alice.notifications().await.expect("REST list");
    assert_eq!(
        list.items
            .iter()
            .filter(|n| n.kind == "key_expiring")
            .count(),
        2
    );
    assert!(
        list.items
            .iter()
            .filter(|n| n.kind == "key_expiring")
            .all(|n| n.short_code.is_none() && n.actor_id.is_none())
    );
    let text = mcp_whoami(&server.base_url, &forever.key).await;
    assert!(text.contains("## Your notifications"), "{text}");
    assert!(text.contains("key_expiring"), "{text}");
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}
