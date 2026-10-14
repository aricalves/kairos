//! The read token of a repository (COLLIERY-T-3105):
//! `/api/repositories/{slug}/credential`, the status on each read surface,
//! the encryption in the database, the access check, and the refusals.
//!
//! The fetch of the builder with the token is in `tests/code_index.rs`
//! (`a_private_repository_is_fetched_with_the_stored_read_token`).
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_repository_credentials_t3105_test`.
//!
//! Cast: `svc` org admin; `bob` member of `platform`, the owner team of the
//! repositories; `alice` member of `web` (no right on them).

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::types_auth::Secret;
use kairos_client::types_org::{AddTeamMemberRequest, CreateTeamRequest};
use kairos_client::types_repositories::CreateRepositoryRequest;
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;
use kairos_server::secrets::SecretsKey;

const SCRATCH_DB: &str = "kairos_repository_credentials_t3105_test";
const TOKEN: &str = "github_pat_t3105_API_SECRET_42";
/// The token of `missing-repo`. Its URL has it, so that the error of git
/// names it.
const OTHER_TOKEN: &str = "github_pat_t3105_OTHER_SECRET_77";

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("user {email} not provisioned: {e}"))
}

/// A git repository with one commit on `main`, at `dir`.
fn repository_with_a_commit(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@kairos.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@kairos.test")
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "private\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "first"]);
}

#[derive(QueryableByName)]
struct Text {
    #[diesel(sql_type = diesel::sql_types::Text)]
    text: String,
}

#[derive(QueryableByName)]
struct Bytes {
    #[diesel(sql_type = diesel::sql_types::Bytea)]
    bytes: Vec<u8>,
}

/// Each row of the credentials of the tenant, as JSON text.
fn credential_rows(conn: &mut PgConnection) -> Vec<String> {
    diesel::sql_query(
        "SELECT row_to_json(rc)::text AS text FROM org_acme.repository_credentials rc",
    )
    .load::<Text>(conn)
    .unwrap()
    .into_iter()
    .map(|r| r.text)
    .collect()
}

/// The activity rows of the read tokens: `actor details`.
fn credential_activity(conn: &mut PgConnection) -> Vec<String> {
    diesel::sql_query(
        "SELECT actor_id::text || ' ' || details AS text FROM org_acme.activity_log \
         WHERE details LIKE 'repository_credential%' ORDER BY occurred_at",
    )
    .load::<Text>(conn)
    .unwrap()
    .into_iter()
    .map(|r| r.text)
    .collect()
}

fn assert_no_token(what: &str, text: &str) {
    assert!(!text.contains(TOKEN), "{what} has the token: {text}");
}

/// A session of the MCP endpoint, for the tool `get_repository`.
struct Mcp {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Mcp {
    async fn open(base_url: &str, token: &str) -> Self {
        let http = reqwest::Client::new();
        let url = format!("{base_url}/mcp");
        let response = http
            .post(&url)
            .bearer_auth(token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "colliery-t3105-test", "version": "0.0.0"},
                },
            }))
            .send()
            .await
            .expect("initialize");
        assert_eq!(response.status(), 200);
        let mcp = Self {
            http,
            url,
            token: token.to_string(),
        };
        let response = mcp
            .post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert_eq!(response.status(), 202);
        mcp
    }

    async fn post(&self, body: Value) -> reqwest::Response {
        self.http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .expect("mcp request")
    }

    /// The text of a tool call.
    async fn call(&self, tool: &str, arguments: Value) -> String {
        let body = self
            .post(json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }))
            .await
            .text()
            .await
            .unwrap();
        let message: Value = serde_json::from_str::<Value>(&body)
            .ok()
            .filter(|v| v.get("jsonrpc").is_some())
            .or_else(|| {
                body.lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
                    .find(|v| v.get("jsonrpc").is_some())
            })
            .unwrap_or_else(|| panic!("no JSON-RPC message: {body:?}"));
        message["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text: {message}"))
            .to_string()
    }
}

#[tokio::test]
async fn the_read_token_of_a_repository() {
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

    // A private git repository behind basic auth, which takes TOKEN.
    let work = tempfile::tempdir().unwrap();
    repository_with_a_commit(&work.path().join("private"));
    let git_server =
        common::git_http::serve(work.path().to_path_buf(), "x-access-token", TOKEN).await;

    // 3 servers on one database: with a key, with no key, with a
    // different key.
    let key = SecretsKey::from_bytes([7u8; 32]);
    let other_key = SecretsKey::from_bytes([8u8; 32]);
    let http = reqwest::Client::new();
    let svc_token = user_token(&http, "svc").await;
    let alice_token = user_token(&http, "alice").await;
    let bob_token = user_token(&http, "bob").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let server_with = |secrets_key: Option<SecretsKey>| {
        let mut config = base_config(&scratch_url);
        config.secrets_key = secrets_key;
        app::router(app::state_with(config, pool.clone(), auth.clone()))
    };
    let keyed = spawn_server(server_with(Some(key.clone()))).await;
    let unkeyed = spawn_server(server_with(None)).await;
    let rekeyed = spawn_server(server_with(Some(other_key))).await;
    let svc = keyed.client(&svc_token, "acme");
    let alice = keyed.client(&alice_token, "acme");
    let bob = keyed.client(&bob_token, "acme");
    for client in [&svc, &alice, &bob] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
        ("bob@kairos.test", OrgRole::Member),
    ] {
        let user_id = user_id(&mut conn, email);
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }
    let bob_id = user_id(&mut conn, "bob@kairos.test");
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    for (name, slug, prefix, member) in [
        ("Platform", "platform", "PLATFORM", "bob@kairos.test"),
        ("Web", "web", "WEB", "alice@kairos.test"),
    ] {
        let team = svc
            .create_team(&CreateTeamRequest {
                name: name.into(),
                slug: slug.into(),
                code_prefix: prefix.into(),
                team_type: None,
            })
            .await
            .expect("team");
        svc.add_team_member(
            &team.id,
            &AddTeamMemberRequest {
                user_id: user_id(&mut conn, member).to_string(),
            },
        )
        .await
        .expect("team member");
    }
    let repository = |slug: &str, url: String| CreateRepositoryRequest {
        slug: Some(slug.into()),
        forge: "github".into(),
        repo_full_name: format!("acme/{slug}"),
        repo_url: url,
        default_branch: None,
        team: "platform".into(),
        description: None,
    };
    bob.create_repository(&repository(
        "private-repo",
        format!("{}/private/.git", git_server.base_url),
    ))
    .await
    .expect("private-repo");
    // A repository whose URL has its token in it: the error of git names
    // the URL, so the test can see that the token is removed from it.
    bob.create_repository(&repository(
        "missing-repo",
        format!("{}/missing-{OTHER_TOKEN}.git", git_server.base_url),
    ))
    .await
    .expect("missing-repo");
    let path = "/api/repositories/private-repo/credential";

    // =======================================================================
    // AC3: with no KAIROS_SECRETS_KEY, a write is refused, and the refusal
    // names the setting.
    // =======================================================================
    let (status, body) = unkeyed
        .client(&bob_token, "acme")
        .raw_request(Method::PUT, path, Some(&json!({ "token": TOKEN })))
        .await
        .unwrap();
    println!("AC3 PUT with no key: {status} {body}");
    assert_eq!(status, 501, "{body}");
    assert_eq!(body["error"]["code"], "SECRETS_NOT_CONFIGURED");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("KAIROS_SECRETS_KEY"),
        "{body}"
    );
    assert_no_token("the refusal", &body.to_string());
    assert!(credential_rows(&mut conn).is_empty());

    // =======================================================================
    // AC5: an unknown field of the body is refused and named.
    // =======================================================================
    let (status, body) = bob
        .raw_request(
            Method::PUT,
            path,
            Some(&json!({ "token": TOKEN, "username": "x-access-token" })),
        )
        .await
        .unwrap();
    println!("AC5 PUT with an unknown field: {status} {body}");
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["details"]["field"], "username", "{body}");
    assert_no_token("the refusal", &body.to_string());
    // A token that is not one word is refused, and named.
    let (status, body) = bob
        .raw_request(Method::PUT, path, Some(&json!({ "token": "two words" })))
        .await
        .unwrap();
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["details"]["field"], "token", "{body}");

    // =======================================================================
    // AC4: a member of a different team, with no admin role, cannot set,
    // remove or check the token. The status is open to read.
    // =======================================================================
    for (method, uri, body) in [
        (
            Method::PUT,
            path.to_string(),
            Some(json!({ "token": TOKEN })),
        ),
        (Method::DELETE, path.to_string(), None),
        (Method::POST, format!("{path}/check"), None),
    ] {
        let (status, response) = alice
            .raw_request(method.clone(), &uri, body.as_ref())
            .await
            .unwrap();
        println!("AC4 alice {method} {uri}: {status}");
        assert_eq!(status, 403, "{method} {uri}: {response}");
    }
    assert!(credential_rows(&mut conn).is_empty());
    let (status, body) = alice.raw_request(Method::GET, path, None).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body["set"], false);

    // =======================================================================
    // AC1: the owner team sets the token. The row has ciphertext only.
    // =======================================================================
    let set = bob
        .set_repository_credential("private-repo", &Secret::new(TOKEN))
        .await
        .expect("bob sets the token");
    assert!(set.set);
    assert_eq!(set.set_by.as_deref(), Some(bob_id.to_string().as_str()));
    let rows = credential_rows(&mut conn);
    println!("AC1 the row: {rows:?}");
    assert_eq!(rows.len(), 1);
    assert_no_token("the row", &rows[0]);
    let hex: String = TOKEN.bytes().map(|b| format!("{b:02x}")).collect();
    assert!(!rows[0].contains(&hex), "the row has the token in hex");
    let ciphertext =
        diesel::sql_query("SELECT ciphertext AS bytes FROM org_acme.repository_credentials")
            .get_result::<Bytes>(&mut conn)
            .unwrap()
            .bytes;
    assert!(
        !ciphertext
            .windows(TOKEN.len())
            .any(|w| w == TOKEN.as_bytes()),
        "the ciphertext has the token bytes"
    );
    assert_eq!(ciphertext.len(), TOKEN.len() + 16, "ciphertext and tag");

    // =======================================================================
    // AC2: each read shows set, set_by and set_at, and never the token.
    // =======================================================================
    let (_, credential) = alice.raw_request(Method::GET, path, None).await.unwrap();
    println!("AC2 GET credential: {credential}");
    assert_eq!(credential["set"], true);
    assert_eq!(credential["set_by"], bob_id.to_string());
    assert!(credential["set_at"].is_string());
    assert_no_token("GET credential", &credential.to_string());
    let (_, detail) = alice
        .raw_request(Method::GET, "/api/repositories/private-repo", None)
        .await
        .unwrap();
    assert_eq!(detail["credential"]["set"], true, "{detail}");
    assert_eq!(detail["credential"]["set_by"], bob_id.to_string());
    assert_no_token("GET repository", &detail.to_string());
    let (_, list) = alice
        .raw_request(Method::GET, "/api/repositories", None)
        .await
        .unwrap();
    assert_no_token("GET repositories", &list.to_string());
    let listed = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["slug"] == "private-repo")
        .unwrap();
    assert_eq!(listed["credential"]["set"], true);
    let mcp = Mcp::open(&keyed.base_url, &alice_token).await;
    let text = mcp
        .call("get_repository", json!({"repository": "private-repo"}))
        .await;
    println!("AC2 MCP get_repository: {text}");
    assert!(text.contains("- read token: set by "), "{text}");
    assert!(text.contains("; not checked"), "{text}");
    assert_no_token("get_repository", &text);
    let text = mcp
        .call("get_repository", json!({"repository": "missing-repo"}))
        .await;
    assert!(text.contains("- read token: not set"), "{text}");

    // A replacement is a second activity row; the check result is cleared.
    bob.set_repository_credential("private-repo", &Secret::new(TOKEN))
        .await
        .expect("bob replaces the token");

    // =======================================================================
    // The access check: git ls-remote with the token.
    // =======================================================================
    let checked = bob
        .check_repository_credential("private-repo")
        .await
        .expect("the check runs");
    println!("check: {checked:?}");
    assert_eq!(checked.last_check_ok, Some(true), "{checked:?}");
    assert!(checked.last_checked_at.is_some());
    let seen = git_server.seen();
    assert!(
        seen.iter()
            .any(|s| s.user.as_deref() == Some("x-access-token")),
        "{seen:?}"
    );
    assert!(seen.iter().all(|s| !s.uri.contains(TOKEN)), "{seen:?}");

    // =======================================================================
    // AC7: when git fails, the stored error has no token.
    // =======================================================================
    svc.set_repository_credential("missing-repo", &Secret::new(OTHER_TOKEN))
        .await
        .expect("svc sets a token");
    let failed = svc
        .check_repository_credential("missing-repo")
        .await
        .expect("the check runs");
    println!("AC7 a failed check: {failed:?}");
    assert_eq!(failed.last_check_ok, Some(false));
    let error = failed.last_check_error.clone().unwrap_or_default();
    assert!(error.contains("git ls-remote failed"), "{error}");
    assert!(error.contains("missing-[token].git"), "{error}");
    assert!(
        !error.contains(OTHER_TOKEN),
        "the stored error has the token: {error}"
    );
    for row in credential_rows(&mut conn) {
        assert_no_token("a row after a failed check", &row);
        assert!(!row.contains(OTHER_TOKEN), "{row}");
    }

    // =======================================================================
    // AC8: a ciphertext moved to a different repository does not decrypt.
    // A changed key gives the "set it again" error.
    // =======================================================================
    diesel::sql_query(
        "UPDATE org_acme.repository_credentials m SET ciphertext = p.ciphertext, \
         nonce = p.nonce, key_id = p.key_id \
         FROM org_acme.repository_credentials p, org_acme.repositories pr, \
              org_acme.repositories mr \
         WHERE pr.slug = 'private-repo' AND p.repository_id = pr.id \
           AND mr.slug = 'missing-repo' AND m.repository_id = mr.id",
    )
    .execute(&mut conn)
    .unwrap();
    let (status, body) = svc
        .raw_request(
            Method::POST,
            "/api/repositories/missing-repo/credential/check",
            None,
        )
        .await
        .unwrap();
    println!("AC8 a moved ciphertext: {status} {body}");
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "CREDENTIAL_UNREADABLE");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not decrypt"),
        "{body}"
    );
    let (status, body) = rekeyed
        .client(&bob_token, "acme")
        .raw_request(Method::POST, &format!("{path}/check"), None)
        .await
        .unwrap();
    println!("AC8 a changed key: {status} {body}");
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("different KAIROS_SECRETS_KEY. Set the token again."),
        "{body}"
    );
    let (status, body) = unkeyed
        .client(&bob_token, "acme")
        .raw_request(Method::POST, &format!("{path}/check"), None)
        .await
        .unwrap();
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no KAIROS_SECRETS_KEY"),
        "{body}"
    );

    // =======================================================================
    // KAIROS-T-0370: rotate-secrets. The keyring of the new key [8] with the
    // old key [7] encrypts each row again with [8]; then the server with
    // [8] opens the token, and the server with [7] alone does not.
    // =======================================================================
    let ring = SecretsKey::from_bytes([8u8; 32]).with_old_bytes([7u8; 32]);
    let report = kairos_server::secrets_rotation::rotate_tenant(&mut conn, "acme", &ring)
        .expect("rotate-secrets");
    println!("rotation: {report:?}");
    // private-repo opens; missing-repo has the ciphertext of private-repo
    // since AC8, so no key opens it, and the report names it.
    assert_eq!(report.resealed, 1, "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].contains("does not decrypt"), "{report:?}");
    let (status, body) = rekeyed
        .client(&bob_token, "acme")
        .raw_request(Method::POST, &format!("{path}/check"), None)
        .await
        .unwrap();
    assert_eq!(status, 200, "the new key opens the token: {body}");
    let (status, body) = bob
        .raw_request(Method::POST, &format!("{path}/check"), None)
        .await
        .unwrap();
    assert_eq!(status, 409, "the old key alone does not: {body}");
    // A second run has nothing to do. A key that is not in the ring is
    // named, and the row is not changed.
    let again = kairos_server::secrets_rotation::rotate_tenant(&mut conn, "acme", &ring)
        .expect("rotate-secrets again");
    assert_eq!((again.resealed, again.current), (0, 1), "{again:?}");
    let stray = kairos_server::secrets_rotation::rotate_tenant(
        &mut conn,
        "acme",
        &SecretsKey::from_bytes([9u8; 32]),
    )
    .expect("rotate-secrets with a key that opens nothing");
    assert_eq!(stray.failed.len(), 2, "{stray:?}");
    assert!(
        stray.failed[0].starts_with("repository_credentials ("),
        "{stray:?}"
    );

    // =======================================================================
    // AC9: remove deletes the row. A second remove, and a check, get 404.
    // =======================================================================
    let removed = bob
        .remove_repository_credential("private-repo")
        .await
        .expect("bob removes the token");
    assert!(!removed.set);
    let rows = credential_rows(&mut conn);
    assert_eq!(rows.len(), 1, "only the row of missing-repo: {rows:?}");
    for (method, uri) in [
        (Method::DELETE, path.to_string()),
        (Method::POST, format!("{path}/check")),
    ] {
        let (status, body) = bob.raw_request(method, &uri, None).await.unwrap();
        assert_eq!(status, 404, "{uri}: {body}");
    }

    // The activity rows: the actor, the slug, never the token.
    let activity = credential_activity(&mut conn);
    println!("activity: {activity:#?}");
    assert_eq!(
        activity,
        vec![
            format!("{bob_id} repository_credential_set:private-repo"),
            format!("{bob_id} repository_credential_replaced:private-repo"),
            format!("{svc_id} repository_credential_set:missing-repo"),
            format!("{bob_id} repository_credential_removed:private-repo"),
        ]
    );

    // A removed repository keeps no token.
    svc.delete_repository("missing-repo")
        .await
        .expect("svc removes the repository");
    assert!(credential_rows(&mut conn).is_empty());

    org_read_tokens(
        &mut conn,
        &keyed,
        &unkeyed,
        &svc_token,
        &alice_token,
        svc_id,
    )
    .await;
}

/// Each row of the read tokens of the organization, as JSON text.
fn org_token_rows(conn: &mut PgConnection) -> Vec<String> {
    diesel::sql_query("SELECT row_to_json(t)::text AS text FROM org_acme.org_read_tokens t")
        .load::<Text>(conn)
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect()
}

/// KAIROS-T-0431: the read token of the organization for each forge host,
/// over REST and MCP. Only an organization admin; the reads show the host,
/// who set it and when, never the token; each write has an activity row.
async fn org_read_tokens(
    conn: &mut PgConnection,
    keyed: &common::TestServer,
    unkeyed: &common::TestServer,
    svc_token: &str,
    alice_token: &str,
    svc_id: Uuid,
) {
    let svc = keyed.client(svc_token, "acme");
    let alice = keyed.client(alice_token, "acme");
    let list = "/api/org/read-tokens";
    let github = "/api/org/read-tokens/github.com";

    // A member who is not an admin can not list, set or remove.
    for (method, uri, body) in [
        (Method::GET, list, None),
        (Method::PUT, github, Some(json!({ "token": TOKEN }))),
        (Method::DELETE, github, None),
    ] {
        let (status, response) = alice
            .raw_request(method.clone(), uri, body.as_ref())
            .await
            .unwrap();
        println!("T-0431 alice {method} {uri}: {status}");
        assert_eq!(status, 403, "{method} {uri}: {response}");
    }
    // With no KAIROS_SECRETS_KEY, a set is refused and names the setting.
    let (status, body) = unkeyed
        .client(svc_token, "acme")
        .raw_request(Method::PUT, github, Some(&json!({ "token": TOKEN })))
        .await
        .unwrap();
    assert_eq!(status, 501, "{body}");
    assert_eq!(body["error"]["code"], "SECRETS_NOT_CONFIGURED");
    // A host with a path, an unknown field and a token with a space are
    // refused and named.
    for (uri, body, field) in [
        (
            "/api/org/read-tokens/github.com%2Facme",
            json!({ "token": TOKEN }),
            "host",
        ),
        (github, json!({ "token": TOKEN, "user": "x" }), "user"),
        (github, json!({ "token": "two words" }), "token"),
    ] {
        let (status, response) = svc
            .raw_request(Method::PUT, uri, Some(&body))
            .await
            .unwrap();
        println!("T-0431 refused {uri}: {status} {response}");
        assert_eq!(status, 422, "{uri}: {response}");
        assert_eq!(response["error"]["details"]["field"], field, "{response}");
        assert_no_token("the refusal", &response.to_string());
    }
    assert!(org_token_rows(conn).is_empty());

    // The admin sets the token of a host as a person writes it; Kairos
    // keeps the normalized host and the ciphertext only.
    let set = svc
        .set_org_read_token("GitHub.com", &Secret::new(TOKEN))
        .await
        .expect("svc sets the token of github.com");
    assert_eq!(set.host, "github.com");
    assert_eq!(set.set_by, svc_id.to_string());
    svc.set_org_read_token("github.com:443", &Secret::new(TOKEN))
        .await
        .expect("svc replaces the token of github.com");
    svc.set_org_read_token("git.example.org:8443", &Secret::new(OTHER_TOKEN))
        .await
        .expect("svc sets the token of a second host");
    let rows = org_token_rows(conn);
    println!("T-0431 rows: {rows:?}");
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_no_token("a row", row);
        assert!(!row.contains(OTHER_TOKEN), "{row}");
    }

    // The list: the host, who set it and when. Never the token.
    let (status, listed) = svc.raw_request(Method::GET, list, None).await.unwrap();
    println!("T-0431 GET {list}: {listed}");
    assert_eq!(status, 200);
    assert_no_token("the list", &listed.to_string());
    assert!(!listed.to_string().contains(OTHER_TOKEN));
    let hosts: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["host"].as_str().unwrap())
        .collect();
    assert_eq!(hosts, ["git.example.org:8443", "github.com"]);
    assert_eq!(listed[1]["set_by"], svc_id.to_string());
    assert!(listed[1]["set_at"].is_string());

    // MCP: the same, for an admin only.
    let mcp = Mcp::open(&keyed.base_url, svc_token).await;
    let text = mcp.call("list_org_read_tokens", json!({})).await;
    println!("T-0431 MCP list: {text}");
    assert!(text.contains("- github.com: set by "), "{text}");
    assert!(text.contains("- git.example.org:8443: set by "), "{text}");
    assert_no_token("list_org_read_tokens", &text);
    let text = mcp
        .call(
            "set_org_read_token",
            json!({ "host": "https://GitLab.example.org/", "token": "fake_mcp_token_t0431" }),
        )
        .await;
    assert!(
        text.contains("Set the read token of the organization for gitlab.example.org."),
        "{text}"
    );
    assert!(!text.contains("fake_mcp_token_t0431"), "{text}");
    let text = mcp
        .call(
            "remove_org_read_token",
            json!({ "host": "gitlab.example.org" }),
        )
        .await;
    assert!(
        text.contains("Removed the read token of the organization for gitlab.example.org."),
        "{text}"
    );
    let alice_mcp = Mcp::open(&keyed.base_url, alice_token).await;
    for (tool, arguments) in [
        ("list_org_read_tokens", json!({})),
        (
            "set_org_read_token",
            json!({ "host": "github.com", "token": "fake_x" }),
        ),
        ("remove_org_read_token", json!({ "host": "github.com" })),
    ] {
        let text = alice_mcp.call(tool, arguments).await;
        assert!(text.starts_with("FORBIDDEN"), "{tool}: {text}");
    }

    // The key rotation encrypts the tokens of the hosts again.
    let ring = SecretsKey::from_bytes([10u8; 32]).with_old_bytes([7u8; 32]);
    let report = kairos_server::secrets_rotation::rotate_tenant(conn, "acme", &ring)
        .expect("rotate-secrets");
    println!("T-0431 rotation: {report:?}");
    assert_eq!(report.resealed, 2, "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");

    // Remove: the list that stays. A second remove is a 404.
    let left = svc
        .remove_org_read_token("git.example.org:8443")
        .await
        .expect("svc removes a token");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].host, "github.com");
    let (status, body) = svc
        .raw_request(
            Method::DELETE,
            "/api/org/read-tokens/git.example.org:8443",
            None,
        )
        .await
        .unwrap();
    assert_eq!(status, 404, "{body}");

    // The activity rows: the actor and the host, never the token.
    let activity: Vec<String> = diesel::sql_query(
        "SELECT actor_id::text || ' ' || details AS text FROM org_acme.activity_log \
         WHERE details LIKE 'org_read_token%' ORDER BY occurred_at",
    )
    .load::<Text>(conn)
    .unwrap()
    .into_iter()
    .map(|r| r.text)
    .collect();
    println!("T-0431 activity: {activity:#?}");
    assert_eq!(
        activity,
        vec![
            format!("{svc_id} org_read_token_set:github.com"),
            format!("{svc_id} org_read_token_replaced:github.com"),
            format!("{svc_id} org_read_token_set:git.example.org:8443"),
            format!("{svc_id} org_read_token_set:gitlab.example.org"),
            format!("{svc_id} org_read_token_removed:gitlab.example.org"),
            format!("{svc_id} org_read_token_removed:git.example.org:8443"),
        ]
    );
}
