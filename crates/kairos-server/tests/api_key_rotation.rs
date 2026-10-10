//! The expiry and the rotation of API keys and agent keys (KAIROS-T-0370).
//!
//! 1. A new agent key gets the default expiry (`KAIROS_API_KEY_TTL_DAYS`,
//!    90 days); `no_expiry: true` makes a key with no expiry; the two
//!    together are a 422; `expires_at` sets the expiry.
//! 2. alice rotates her agent key: the new key works, the old key works
//!    until the end of the grace time, and the list links the old key to
//!    the new one. A second rotation of the old key is 409. A request with
//!    an agent key cannot rotate (403).
//! 3. With a grace time of 0, the old key gets 401 at once.
//! 4. An org admin rotates the key of a service account.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_api_key_rotation_t0370_test`.

mod common;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::Error;
use kairos_client::types_service_accounts::{CreateApiKeyRequest, CreateServiceAccountRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_api_key_rotation_t0370_test";

fn request(name: &str) -> CreateApiKeyRequest {
    CreateApiKeyRequest {
        name: name.into(),
        expires_at: None,
        no_expiry: false,
    }
}

fn at(text: &str) -> DateTime<Utc> {
    text.parse().expect("an RFC 3339 time")
}

fn near(actual: DateTime<Utc>, expected: DateTime<Utc>) -> bool {
    (actual - expected).num_seconds().abs() < 120
}

#[tokio::test]
async fn keys_expire_by_default_and_rotate_with_a_grace_time() {
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
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    // The default server: 90 days of life, a grace time of 1 day.
    let server = spawn_server(app::router(app::state_with(
        base_config(&scratch_url),
        TenantPool::new(&scratch_url, 4).await.expect("pool"),
        auth.clone(),
    )))
    .await;
    // A server with a grace time of 0.
    let mut no_grace = base_config(&scratch_url);
    no_grace.api_key_rotation_grace_secs = 0;
    let strict = spawn_server(app::router(app::state_with(
        no_grace,
        TenantPool::new(&scratch_url, 4).await.expect("pool"),
        auth,
    )))
    .await;

    let admin = server.client(&svc_token, "acme");
    let alice = server.client(&alice_token, "acme");
    let _ = admin.whoami().await;
    let _ = alice.whoami().await;
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
    ] {
        let user_id: Uuid = users::table
            .filter(users::email.eq(email))
            .select(users::id)
            .first(&mut conn)
            .expect("user row");
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }

    // 1. The default expiry, no expiry, and a refusal of the two together.
    let now = Utc::now();
    let laptop = alice
        .create_agent_key(&request("laptop"))
        .await
        .expect("a key with the default expiry");
    let expiry = at(laptop.expires_at.as_deref().expect("a default expiry"));
    assert!(near(expiry, now + chrono::Duration::days(90)), "{expiry}");
    let forever = alice
        .create_agent_key(&CreateApiKeyRequest {
            no_expiry: true,
            ..request("forever")
        })
        .await
        .expect("a key with no expiry");
    assert_eq!(forever.expires_at, None);
    let both = alice
        .create_agent_key(&CreateApiKeyRequest {
            name: "both".into(),
            expires_at: Some("2030-01-01T00:00:00Z".into()),
            no_expiry: true,
        })
        .await;
    match both {
        Err(Error::Validation { message, .. }) => {
            assert!(message.contains("no_expiry"), "{message}")
        }
        other => panic!("expected 422, got {other:?}"),
    }
    let dated = alice
        .create_agent_key(&CreateApiKeyRequest {
            expires_at: Some("2030-01-01T00:00:00Z".into()),
            ..request("dated")
        })
        .await
        .expect("a key with its own expiry");
    assert_eq!(
        at(dated.expires_at.as_deref().expect("expiry")),
        at("2030-01-01T00:00:00Z")
    );

    // 2. alice rotates the laptop key.
    let rotated = alice
        .rotate_agent_key(&laptop.id)
        .await
        .expect("alice rotates her key");
    assert_eq!(rotated.replaced_key_id, laptop.id);
    assert_eq!(rotated.name, "laptop");
    assert_ne!(rotated.key, laptop.key);
    // The new key has the life of the old key, from now.
    let new_expiry = at(rotated.expires_at.as_deref().expect("an expiry"));
    assert!(
        near(new_expiry, Utc::now() + chrono::Duration::days(90)),
        "{new_expiry}"
    );
    // The old secret ends after the grace time.
    let old_end = at(&rotated.replaced_key_expires_at);
    assert!(
        near(old_end, Utc::now() + chrono::Duration::days(1)),
        "{old_end}"
    );
    let new_agent = server.client(&rotated.key, "acme");
    let old_agent = server.client(&laptop.key, "acme");
    new_agent.whoami().await.expect("the new key works");
    old_agent
        .whoami()
        .await
        .expect("the old key works in the grace time");
    let listed = alice.list_agent_keys().await.expect("list");
    let old_row = listed
        .items
        .iter()
        .find(|k| k.id == laptop.id)
        .expect("the old key stays in the list");
    assert_eq!(old_row.replaced_by.as_deref(), Some(rotated.id.as_str()));
    assert!(old_row.revoked_at.is_none());
    match alice.rotate_agent_key(&laptop.id).await {
        Err(Error::Conflict { message, .. }) => {
            assert!(message.contains(&rotated.id), "{message}")
        }
        other => panic!("expected 409, got {other:?}"),
    }
    match new_agent.rotate_agent_key(&rotated.id).await {
        Err(Error::Forbidden { .. }) => {}
        other => panic!("expected 403, got {other:?}"),
    }
    // A key with no expiry gives a key with no expiry.
    let forever_rotated = alice
        .rotate_agent_key(&forever.id)
        .await
        .expect("rotate the key with no expiry");
    assert_eq!(forever_rotated.expires_at, None);

    // 3. A grace time of 0 ends the old secret at once.
    let alice_strict = strict.client(&alice_token, "acme");
    let quick = alice_strict
        .rotate_agent_key(&dated.id)
        .await
        .expect("rotate with no grace time");
    match strict.client(&dated.key, "acme").whoami().await {
        Err(Error::Unauthorized { .. }) => {}
        other => panic!("expected 401, got {other:?}"),
    }
    strict
        .client(&quick.key, "acme")
        .whoami()
        .await
        .expect("the new key works");

    // 4. An org admin rotates the key of a service account.
    let sa = admin
        .create_service_account(&CreateServiceAccountRequest { name: "ci".into() })
        .await
        .expect("service account");
    let ci = admin
        .create_api_key(&sa.id, &request("ci-main"))
        .await
        .expect("service account key");
    assert!(
        ci.expires_at.is_some(),
        "a service account key gets the default expiry"
    );
    let ci_rotated = admin
        .rotate_api_key(&sa.id, &ci.id)
        .await
        .expect("the admin rotates the key");
    assert_eq!(ci_rotated.replaced_key_id, ci.id);
    server
        .client(&ci_rotated.key, "acme")
        .whoami()
        .await
        .expect("the new service account key works");
    match alice.rotate_api_key(&sa.id, &ci_rotated.id).await {
        Err(Error::Forbidden { .. }) => {}
        other => panic!("expected 403, got {other:?}"),
    }
}
