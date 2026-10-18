//! An organization admin lists and revokes the agent key of any person,
//! and the removal of a person revokes their keys (KAIROS-T-0388).
//!
//! 1. The admin lists the agent keys of each person: the owner, the name,
//!    the prefix, the dates; no secret, and no key of a service account.
//!    The `user` filter takes an id or an email; a value that names no
//!    person is 422.
//! 2. A person who is not an org admin is refused (403, the role named).
//! 3. The admin revokes a key with no expiry: the key then gets 401, an
//!    activity row names the key, and the owner gets a notification that
//!    names the key. A second revoke is 409; an unknown id and the id of a
//!    service-account key are 404. The other key of the owner still works.
//! 4. `DELETE /api/members/{user_id}` revokes the keys of the person.
//! 5. The SCIM deprovision (`DELETE /scim/v2/Users/{id}`) revokes the keys
//!    of the person.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_org_agent_keys_t0388_test`.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::Error;
use kairos_client::types_service_accounts::{
    CreateApiKeyRequest, CreateServiceAccountRequest, OrgAgentKeysQuery,
};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_org_agent_keys_t0388_test";

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

fn key_request(name: &str, no_expiry: bool) -> CreateApiKeyRequest {
    CreateApiKeyRequest {
        name: name.into(),
        expires_at: None,
        no_expiry,
    }
}

fn by_user(user: &str) -> OrgAgentKeysQuery {
    OrgAgentKeysQuery {
        user: Some(user.into()),
        ..Default::default()
    }
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

/// The acme activity rows on an api key with an actor and a details pattern.
fn key_activity(conn: &mut PgConnection, actor: Uuid, details_like: &str) -> i64 {
    sql_query(
        "SELECT count(*) AS count FROM org_acme.activity_log \
         WHERE entity_type = 'api_key' AND action = 'delete' AND actor_id = $1 \
           AND details LIKE $2",
    )
    .bind::<diesel::sql_types::Uuid, _>(actor)
    .bind::<diesel::sql_types::Text, _>(details_like)
    .get_result::<CountRow>(conn)
    .expect("activity query")
    .count
}

/// Whether `whoami` with `key` is refused with 401.
async fn is_unauthorized(server: &common::TestServer, key: &str) -> bool {
    matches!(
        server.client(key, "acme").whoami().await,
        Err(Error::Unauthorized { .. })
    )
}

#[tokio::test]
async fn an_org_admin_revokes_any_agent_key_and_a_removal_revokes_the_keys() {
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
    let server = spawn_server(app::router(app::state_with(
        base_config(&scratch_url),
        pool,
        auth,
    )))
    .await;
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
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    let alice_id = user_id(&mut conn, "alice@kairos.test");
    let bob_id = user_id(&mut conn, "bob@kairos.test");
    let carol_id = user_id(&mut conn, "carol@kairos.test");

    let laptop = alice
        .create_agent_key(&key_request("laptop", true))
        .await
        .expect("alice makes a key with no expiry");
    let ci = alice
        .create_agent_key(&key_request("ci", false))
        .await
        .expect("alice makes a second key");
    let desk = bob
        .create_agent_key(&key_request("desk", false))
        .await
        .expect("bob makes a key");
    let phone = carol
        .create_agent_key(&key_request("phone", false))
        .await
        .expect("carol makes a key");
    let sa = admin
        .create_service_account(&CreateServiceAccountRequest { name: "ci".into() })
        .await
        .expect("service account");
    let sa_key = admin
        .create_api_key(&sa.id, &key_request("ci-main", false))
        .await
        .expect("service-account key");

    // 1. The admin lists the keys of each person.
    let all = admin
        .list_org_agent_keys(&OrgAgentKeysQuery::default())
        .await
        .expect("the admin lists the agent keys");
    assert_eq!(all.total, 4, "{all:?}");
    assert_eq!(all.items.len(), 4);
    assert!(
        all.items.iter().all(|k| k.id != sa_key.id),
        "no key of a service account"
    );
    let row = all
        .items
        .iter()
        .find(|k| k.id == laptop.id)
        .expect("the key laptop is listed");
    assert_eq!(row.owner_id, alice_id.to_string());
    assert_eq!(row.owner_email, "alice@kairos.test");
    assert_eq!(row.name, "laptop");
    assert_eq!(row.prefix, laptop.prefix);
    assert_eq!(row.expires_at, None, "no expiry");
    assert!(row.revoked_at.is_none());
    let raw = serde_json::to_string(&all).expect("serialize");
    for secret in [&laptop.key, &ci.key, &desk.key, &phone.key] {
        assert!(!raw.contains(secret.as_str()), "the list shows no secret");
    }
    let page = admin
        .list_org_agent_keys(&OrgAgentKeysQuery {
            limit: Some(1),
            offset: Some(1),
            ..Default::default()
        })
        .await
        .expect("a page");
    assert_eq!(
        (page.total, page.items.len(), page.limit, page.offset),
        (4, 1, 1, 1)
    );
    // KAIROS-T-0589: the filter takes the user name too.
    let alice_user_name: String = users::table
        .filter(users::id.eq(alice_id))
        .select(users::user_name)
        .first(&mut conn)
        .expect("alice's user name");
    for filter in [
        "alice@kairos.test".to_string(),
        alice_id.to_string(),
        alice_user_name,
    ] {
        let mine = admin
            .list_org_agent_keys(&by_user(&filter))
            .await
            .expect("the keys of alice");
        assert_eq!(mine.total, 2, "{filter}");
        assert!(
            mine.items
                .iter()
                .all(|k| k.owner_id == alice_id.to_string())
        );
    }
    for unknown in ["nobody@kairos.test".to_string(), Uuid::new_v4().to_string()] {
        match admin.list_org_agent_keys(&by_user(&unknown)).await {
            Err(Error::Validation { message, .. }) => {
                assert!(message.contains("No person has"), "{message}");
            }
            other => panic!("expected 422 for {unknown}, got {other:?}"),
        }
    }

    // 2. A person who is not an org admin is refused.
    let refusals = [
        bob.list_org_agent_keys(&OrgAgentKeysQuery::default())
            .await
            .map(|_| ()),
        bob.revoke_org_agent_key(&laptop.id).await.map(|_| ()),
    ];
    for result in refusals {
        match result {
            Err(Error::Forbidden { message, .. }) => {
                assert!(message.contains("organization admin role"), "{message}");
            }
            other => panic!("expected 403, got {other:?}"),
        }
    }
    assert!(
        !is_unauthorized(&server, &laptop.key).await,
        "bob changed nothing"
    );

    // 3. The admin revokes the key with no expiry.
    let revoked = admin
        .revoke_org_agent_key(&laptop.id)
        .await
        .expect("the admin revokes the key of alice");
    assert!(revoked.deleted);
    assert_eq!(revoked.id, laptop.id);
    assert!(
        is_unauthorized(&server, &laptop.key).await,
        "the key gets 401"
    );
    assert!(
        !is_unauthorized(&server, &ci.key).await,
        "the other key of alice works"
    );
    assert_eq!(
        key_activity(
            &mut conn,
            svc_id,
            "agent_key_admin_revoke:laptop owner:alice@kairos.test"
        ),
        1,
        "one activity row names the key and the owner"
    );
    let inbox = alice.notifications().await.expect("alice reads her inbox");
    let told: Vec<_> = inbox
        .items
        .iter()
        .filter(|n| n.kind == "agent_key_revoked")
        .collect();
    assert_eq!(told.len(), 1, "{inbox:?}");
    assert!(told[0].text.contains("laptop"), "{}", told[0].text);
    assert!(told[0].text.contains(&laptop.prefix), "{}", told[0].text);
    assert_eq!(
        told[0].actor_id.as_deref(),
        Some(svc_id.to_string().as_str())
    );
    let listed = alice.list_agent_keys().await.expect("alice lists her keys");
    let own = listed
        .items
        .iter()
        .find(|k| k.id == laptop.id)
        .expect("own");
    assert!(own.revoked_at.is_some(), "alice sees the revoke");
    match admin.revoke_org_agent_key(&laptop.id).await {
        Err(Error::Conflict { .. }) => {}
        other => panic!("expected 409, got {other:?}"),
    }
    for unknown in [Uuid::new_v4().to_string(), sa_key.id.clone()] {
        match admin.revoke_org_agent_key(&unknown).await {
            Err(Error::NotFound { .. }) => {}
            other => panic!("expected 404 for {unknown}, got {other:?}"),
        }
    }
    assert!(
        !is_unauthorized(&server, &sa_key.key).await,
        "the service-account key works"
    );

    // 4. The removal of bob revokes his key.
    assert!(!is_unauthorized(&server, &desk.key).await);
    admin
        .remove_org_member(&bob_id.to_string())
        .await
        .expect("the admin removes bob");
    let bobs = admin
        .list_org_agent_keys(&by_user("bob@kairos.test"))
        .await
        .expect("the keys of bob");
    assert_eq!(bobs.total, 1);
    assert!(
        bobs.items[0].revoked_at.is_some(),
        "the removal revoked desk"
    );
    assert!(is_unauthorized(&server, &desk.key).await, "desk gets 401");
    assert_eq!(
        key_activity(&mut conn, svc_id, "agent_key_revoke_on_remove:desk"),
        1
    );

    // 5. The SCIM deprovision of carol revokes her key.
    let response = http
        .post(format!("{}/api/scim-tokens", server.base_url))
        .bearer_auth(&svc_token)
        .header("x-tenant", "acme")
        .json(&json!({"name": "idp"}))
        .send()
        .await
        .expect("scim token request");
    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.expect("scim token body");
    let scim_token = body["token"].as_str().expect("token").to_string();
    assert!(!is_unauthorized(&server, &phone.key).await);
    let response = http
        .delete(format!("{}/scim/v2/Users/{carol_id}", server.base_url))
        .bearer_auth(&scim_token)
        .send()
        .await
        .expect("scim deprovision");
    assert_eq!(response.status(), 204);
    let carols = admin
        .list_org_agent_keys(&by_user("carol@kairos.test"))
        .await
        .expect("the keys of carol");
    assert!(carols.items[0].revoked_at.is_some(), "SCIM revoked phone");
    assert!(is_unauthorized(&server, &phone.key).await, "phone gets 401");
    let scim_actor: Uuid = sql_query(
        "SELECT actor_id FROM org_acme.activity_log \
         WHERE entity_type = 'api_key' AND details LIKE '%agent_key_revoke_on_remove:phone'",
    )
    .get_result::<ActorRow>(&mut conn)
    .expect("the SCIM activity row of the key")
    .actor_id;
    assert_eq!(
        key_activity(&mut conn, scim_actor, "%agent_key_revoke_on_remove:phone"),
        1
    );

    // The keys of alice are as before: laptop revoked, ci active.
    let alices = admin
        .list_org_agent_keys(&by_user("alice@kairos.test"))
        .await
        .expect("the keys of alice");
    let ci_row = alices.items.iter().find(|k| k.id == ci.id).expect("ci");
    assert!(ci_row.revoked_at.is_none());
}

#[derive(QueryableByName)]
struct ActorRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    actor_id: Uuid,
}
