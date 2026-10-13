//! The provider settings of the code index of a tenant (KAIROS-T-0339,
//! COLLIERY-I-0611): `/api/org/code-index-settings`.
//!
//! 1. An org admin sets Ollama Cloud with a key; the read shows the
//!    provider, the URL, the model and the status of the secret, never the
//!    key. A member reads it too, and cannot write it.
//! 2. A provider that needs a value is refused with the field named; an
//!    unknown provider or field is refused and named.
//! 3. A write with the secret absent keeps the stored secret; an empty
//!    secret removes it, and the provider then refuses to stay hosted.
//! 4. The stored secret opens only with the key of the deployment and the
//!    AAD of its tenant and field.
//! 5. A server with no KAIROS_SECRETS_KEY refuses a secret and names the
//!    setting.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_code_index_settings_t0339_test`.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::Error;
use kairos_client::types_auth::Secret;
use kairos_client::types_code_index::{
    PutCodeIndexSettings, PutSummaryProvider, PutVectorProvider,
};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;
use kairos_server::secrets::SecretsKey;

const SCRATCH_DB: &str = "kairos_code_index_settings_t0339_test";
const KEY: &str = "ollama_t0339_API_SECRET_42";

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

fn ollama(secret: Option<&str>) -> PutCodeIndexSettings {
    PutCodeIndexSettings {
        summary: Some(PutSummaryProvider {
            provider: "ollama-cloud".into(),
            base_url: Some("https://ollama.com/v1".into()),
            model: Some("gemma4:31b".into()),
            region: None,
            secret: secret.map(Secret::new),
        }),
        vectors: Some(PutVectorProvider {
            provider: "embedded".into(),
            ..Default::default()
        }),
        concurrency: Some(8),
        default_summaries: None,
        version: None,
    }
}

fn field_of(err: &Error) -> Option<String> {
    match err {
        Error::Validation { field, .. } => field.clone(),
        _ => None,
    }
}

#[tokio::test]
async fn the_provider_settings_of_a_tenant() {
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

    let key = SecretsKey::from_bytes([9u8; 32]);
    let http = reqwest::Client::new();
    let svc_token = user_token(&http, "svc").await;
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
    let svc = keyed.client(&svc_token, "acme");
    let bob = keyed.client(&bob_token, "acme");
    for client in [&svc, &bob] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
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
    let svc_id = user_id(&mut conn, "svc@kairos.test");

    // A tenant with no row: the defaults.
    let defaults = bob.code_index_settings().await.expect("the defaults");
    assert_eq!(defaults.summary.provider, "embedded");
    assert_eq!(defaults.vectors.provider, "embedded");
    assert_eq!(defaults.concurrency, 4);
    assert!(!defaults.summary.secret.set);
    assert_eq!(defaults.updated_by, None);

    // 1. The admin sets Ollama Cloud with a key; a member cannot.
    let err = bob
        .put_code_index_settings(&ollama(Some(KEY)))
        .await
        .expect_err("bob is a member");
    assert!(matches!(err, Error::Forbidden { .. }), "{err}");
    let set = svc
        .put_code_index_settings(&ollama(Some(KEY)))
        .await
        .expect("the admin sets the provider");
    assert_eq!(set.summary.provider, "ollama-cloud");
    assert_eq!(
        set.summary.base_url.as_deref(),
        Some("https://ollama.com/v1")
    );
    assert_eq!(set.summary.model.as_deref(), Some("gemma4:31b"));
    assert!(set.summary.secret.set);
    assert_eq!(
        set.summary.secret.set_by.as_deref(),
        Some(svc_id.to_string().as_str())
    );
    let set_at = set
        .summary
        .secret
        .set_at
        .clone()
        .expect("when the key was set");
    assert_eq!(set.concurrency, 8);
    assert_eq!(set.updated_by.as_deref(), Some(svc_id.to_string().as_str()));
    let shown = serde_json::to_string(&bob.code_index_settings().await.unwrap()).unwrap();
    assert!(!shown.contains(KEY), "the key is in a read: {shown}");

    // 2. Refusals that name the field.
    let mut no_url = ollama(None);
    no_url.summary.as_mut().unwrap().base_url = None;
    let err = svc
        .put_code_index_settings(&no_url)
        .await
        .expect_err("no URL");
    assert_eq!(field_of(&err).as_deref(), Some("summary.base_url"), "{err}");
    let mut no_model = ollama(None);
    no_model.summary.as_mut().unwrap().model = None;
    let err = svc
        .put_code_index_settings(&no_model)
        .await
        .expect_err("no model");
    assert_eq!(field_of(&err).as_deref(), Some("summary.model"), "{err}");
    let mut unknown = ollama(None);
    unknown.summary.as_mut().unwrap().provider = "openai".into();
    let err = svc
        .put_code_index_settings(&unknown)
        .await
        .expect_err("unknown provider");
    assert_eq!(field_of(&err).as_deref(), Some("summary.provider"), "{err}");
    let mut bad_concurrency = ollama(None);
    bad_concurrency.concurrency = Some(0);
    let err = svc
        .put_code_index_settings(&bad_concurrency)
        .await
        .expect_err("concurrency 0");
    assert_eq!(field_of(&err).as_deref(), Some("concurrency"), "{err}");
    let mut bedrock = ollama(None);
    bedrock.summary.as_mut().unwrap().provider = "bedrock".into();
    bedrock.summary.as_mut().unwrap().base_url = None;
    bedrock.summary.as_mut().unwrap().model =
        Some("anthropic.claude-3-5-haiku-20241022-v1:0".into());
    let err = svc
        .put_code_index_settings(&bedrock)
        .await
        .expect_err("no region");
    assert_eq!(field_of(&err).as_deref(), Some("summary.region"), "{err}");
    bedrock.summary.as_mut().unwrap().region = Some("us-east-1".into());
    bedrock.summary.as_mut().unwrap().secret = Some(Secret::new("AKIA-only"));
    let err = svc
        .put_code_index_settings(&bedrock)
        .await
        .expect_err("the credentials have 2 or 3 parts");
    assert_eq!(field_of(&err).as_deref(), Some("summary.secret"), "{err}");
    // An unknown field (the body is JSON with a field the DTO does not have).
    let raw = http
        .put(format!("{}/api/org/code-index-settings", keyed.base_url))
        .bearer_auth(&svc_token)
        .header("X-Tenant", "acme")
        .json(&serde_json::json!({
            "summary": {"provider": "embedded", "cost_cap": 10},
            "vectors": {"provider": "embedded"},
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(raw.status(), 422);
    let text = raw.text().await.unwrap();
    assert!(text.contains("cost_cap"), "{text}");

    // 3. The secret stays when the body does not name it, and goes when the
    //    body sends an empty one.
    let kept = svc
        .put_code_index_settings(&ollama(None))
        .await
        .expect("a write with no secret keeps it");
    assert!(kept.summary.secret.set);
    assert_eq!(kept.summary.secret.set_at.as_deref(), Some(set_at.as_str()));
    let err = svc
        .put_code_index_settings(&ollama(Some("")))
        .await
        .expect_err("ollama-cloud with no key");
    assert_eq!(field_of(&err).as_deref(), Some("summary.secret"), "{err}");
    let mut back = ollama(Some(""));
    back.summary.as_mut().unwrap().provider = "embedded".into();
    back.summary.as_mut().unwrap().base_url = None;
    back.summary.as_mut().unwrap().model = None;
    let removed = svc
        .put_code_index_settings(&back)
        .await
        .expect("back to embedded, the key removed");
    assert_eq!(removed.summary.provider, "embedded");
    assert!(!removed.summary.secret.set);

    // 4. The sealed secret opens with the key and the AAD of its field, and
    //    with nothing else.
    svc.put_code_index_settings(&ollama(Some(KEY)))
        .await
        .expect("the key again");
    diesel::sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .unwrap();
    let row = kairos_db::code_index_settings::load(&mut conn)
        .expect("read")
        .expect("a row");
    let sealed = row.summary_secret().expect("a sealed secret");
    let aad = kairos_server::api::org::code_index_settings::settings_aad("acme", "summary.secret");
    let opened = key
        .open(&aad, &sealed.key_id, &sealed.nonce, &sealed.ciphertext)
        .expect("opens with the key and the AAD");
    assert_eq!(opened, KEY.as_bytes());
    let other_aad =
        kairos_server::api::org::code_index_settings::settings_aad("acme", "vectors.secret");
    assert!(
        key.open(
            &other_aad,
            &sealed.key_id,
            &sealed.nonce,
            &sealed.ciphertext
        )
        .is_err(),
        "the AAD of another field opens nothing"
    );
    let other_key = SecretsKey::from_bytes([10u8; 32]);
    assert!(
        other_key
            .open(&aad, &sealed.key_id, &sealed.nonce, &sealed.ciphertext)
            .is_err(),
        "another key opens nothing"
    );

    // KAIROS-T-0370: rotate-secrets seals the secret again with the new key
    // [10]; the old key [9] is in the ring and opens it.
    let ring = SecretsKey::from_bytes([10u8; 32]).with_old_bytes([9u8; 32]);
    let report = kairos_server::secrets_rotation::rotate_tenant(&mut conn, "acme", &ring)
        .expect("rotate-secrets");
    assert_eq!(report.resealed, 1, "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    let row = kairos_db::code_index_settings::load(&mut conn)
        .expect("read")
        .expect("a row");
    let resealed = row.summary_secret().expect("a sealed secret");
    assert_eq!(resealed.key_id, other_key.id());
    let opened = other_key
        .open(
            &aad,
            &resealed.key_id,
            &resealed.nonce,
            &resealed.ciphertext,
        )
        .expect("the new key alone opens it");
    assert_eq!(opened, KEY.as_bytes());
    // Back to the key of the servers of this test.
    let back = SecretsKey::from_bytes([9u8; 32]).with_old_bytes([10u8; 32]);
    let report = kairos_server::secrets_rotation::rotate_tenant(&mut conn, "acme", &back)
        .expect("rotate-secrets back");
    assert_eq!(report.resealed, 1, "{report:?}");

    // 5. No KAIROS_SECRETS_KEY: a secret is refused with the setting named;
    //    a write with no secret still goes.
    let svc_unkeyed = unkeyed.client(&svc_token, "acme");
    let err = svc_unkeyed
        .put_code_index_settings(&ollama(Some(KEY)))
        .await
        .expect_err("no key on this server");
    match &err {
        Error::Other {
            status,
            code,
            details,
            ..
        } => {
            assert_eq!(*status, 501);
            assert_eq!(code, "SECRETS_NOT_CONFIGURED");
            assert_eq!(details["setting"], "KAIROS_SECRETS_KEY");
        }
        other => panic!("{other}"),
    }
    svc_unkeyed
        .put_code_index_settings(&ollama(None))
        .await
        .expect("a write that keeps the stored secret needs no key");
}

/// KAIROS-T-0415: the settings cannot be overwritten by mistake.
///
/// 1. Each write adds 1 to the version, and the read names who wrote.
/// 2. A write with an older version is refused (409) with the current
///    settings, and nothing changes.
/// 3. A write of one section leaves the other sections as stored, and a
///    write that does not give the concurrency keeps it.
/// 4. A change of the provider with no new secret: a hosted summary
///    provider is refused (its secret is needed); the vectors lose the old
///    secret.
/// 5. The read lists the repositories that use the hosted summarizer.
#[tokio::test]
async fn the_settings_have_a_version_and_a_write_of_one_section() {
    const DB: &str = "kairos_code_index_settings_t0415_test";
    let _admin_conn = recreate_scratch_db(DB);
    let scratch_url = with_database(&common::admin_database_url(), DB);
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
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let mut config = base_config(&scratch_url);
    config.secrets_key = Some(SecretsKey::from_bytes([9u8; 32]));
    let server = spawn_server(app::router(app::state_with(config, pool, auth))).await;
    let svc = server.client(&svc_token, "acme");
    let _ = svc.whoami().await;
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: svc_id,
            role: OrgRole::Admin,
        })
        .execute(&mut conn)
        .expect("granting membership");

    // 1. The version and the name.
    let defaults = svc.code_index_settings().await.expect("defaults");
    assert_eq!(defaults.version, 0);
    let mut first = ollama(Some(KEY));
    first.version = Some(0);
    let set = svc
        .put_code_index_settings(&first)
        .await
        .expect("first write");
    assert_eq!(set.version, 1);
    assert!(set.updated_by_name.is_some(), "{set:?}");
    assert!(set.summary.secret.set_by_name.is_some(), "{set:?}");

    // 2. A write based on the defaults (version 0) is now stale.
    let mut stale = ollama(None);
    stale.summary.as_mut().unwrap().model = Some("other-model".into());
    stale.version = Some(0);
    let err = svc
        .put_code_index_settings(&stale)
        .await
        .expect_err("a stale write");
    match &err {
        Error::Conflict {
            message, current, ..
        } => {
            assert!(message.contains("changed after you read them"), "{message}");
            assert_eq!(current["version"], 1, "{current}");
            assert_eq!(current["summary"]["model"], "gemma4:31b");
        }
        other => panic!("{other}"),
    }
    let now = svc.code_index_settings().await.expect("read");
    assert_eq!(
        now.summary.model.as_deref(),
        Some("gemma4:31b"),
        "nothing changed"
    );
    assert_eq!(now.version, 1);

    // 3. A write of the builder only: the summaries and the vectors stay.
    let builder_only = PutCodeIndexSettings {
        concurrency: Some(12),
        version: Some(1),
        ..Default::default()
    };
    let set = svc
        .put_code_index_settings(&builder_only)
        .await
        .expect("the builder only");
    assert_eq!(set.version, 2);
    assert_eq!(set.concurrency, 12);
    assert_eq!(set.summary.provider, "ollama-cloud");
    assert_eq!(set.summary.model.as_deref(), Some("gemma4:31b"));
    assert!(set.summary.secret.set, "the secret stays");
    //    A write with no concurrency keeps it.
    let vectors_only = PutCodeIndexSettings {
        vectors: Some(PutVectorProvider {
            provider: "remote".into(),
            base_url: Some("http://ollama:11434/v1".into()),
            model: Some("nomic-embed-text".into()),
            secret: Some(Secret::new("vector-key")),
        }),
        version: Some(2),
        ..Default::default()
    };
    let set = svc
        .put_code_index_settings(&vectors_only)
        .await
        .expect("the vectors only");
    assert_eq!(set.concurrency, 12, "not reset to 4");
    assert_eq!(set.summary.model.as_deref(), Some("gemma4:31b"));
    assert!(set.vectors.secret.set);

    // 4. A change of provider with no new secret.
    let to_bedrock = PutCodeIndexSettings {
        summary: Some(PutSummaryProvider {
            provider: "bedrock".into(),
            region: Some("us-east-1".into()),
            model: Some("anthropic.claude-3-5-haiku-20241022-v1:0".into()),
            ..Default::default()
        }),
        version: Some(3),
        ..Default::default()
    };
    let err = svc
        .put_code_index_settings(&to_bedrock)
        .await
        .expect_err("bedrock needs its own secret");
    assert_eq!(field_of(&err).as_deref(), Some("summary.secret"), "{err}");
    let back_to_embedded = PutCodeIndexSettings {
        vectors: Some(PutVectorProvider {
            provider: "embedded".into(),
            ..Default::default()
        }),
        version: Some(3),
        ..Default::default()
    };
    let set = svc
        .put_code_index_settings(&back_to_embedded)
        .await
        .expect("vectors back to embedded");
    assert!(!set.vectors.secret.set, "the key of the old provider goes");
    assert!(set.summary.secret.set, "the other section keeps its secret");

    // 5. The repositories that use the hosted summarizer.
    let team = svc
        .create_team(&kairos_client::types_org::CreateTeamRequest {
            name: "Platform".into(),
            slug: "platform".into(),
            code_prefix: "PLAT".into(),
            team_type: None,
        })
        .await
        .expect("team");
    let _ = team;
    for (slug, summaries) in [("hosted-one", "hosted"), ("local-one", "embedded")] {
        let created = svc
            .create_repository(
                &kairos_client::types_repositories::CreateRepositoryRequest {
                    slug: Some(slug.into()),
                    forge: "other".into(),
                    repo_full_name: slug.into(),
                    repo_url: format!("https://example.com/{slug}"),
                    default_branch: None,
                    team: "platform".into(),
                    description: None,
                },
            )
            .await
            .expect("repository");
        let _ = created;
        svc.update_repository(
            slug,
            &kairos_client::types_repositories::UpdateRepositoryRequest {
                code_index_summaries: kairos_client::types_repositories::CodeIndexSummaries::parse(
                    summaries,
                ),
                ..Default::default()
            },
        )
        .await
        .expect("summaries");
    }
    let read = svc.code_index_settings().await.expect("read");
    assert_eq!(read.hosted_repositories, vec!["hosted-one".to_string()]);
}
