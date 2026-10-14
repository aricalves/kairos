//! The rotation of `KAIROS_SECRETS_KEY` (KAIROS-T-0370): encrypt each
//! sealed row of a tenant again with the new key.
//!
//! The sealed columns (each with a `key_id`):
//!
//! - `repository_credentials`: the read token of a repository
//!   (`ciphertext`, `nonce`, `key_id`; associated data
//!   [`crate::secrets::credential_aad`]);
//! - `code_index_settings`: the secret of the hosted summary model
//!   (`summary_*`) and of the hosted vector model (`vector_*`; associated
//!   data [`crate::api::org::code_index_settings::settings_aad`]);
//! - `webhook_targets`: the secret of a webhook target (`secret_*`;
//!   associated data [`crate::secrets::webhook_target_aad`], KAIROS-T-0384);
//! - `repository_write_tokens`: the write token of a repository (associated
//!   data [`crate::secrets::write_token_aad`], KAIROS-T-0386);
//! - `org_read_tokens`: the read token of the organization for a forge host
//!   (associated data [`crate::secrets::org_read_token_aad`], KAIROS-T-0431).
//!
//! The webhook secrets of the forge connections are not stored: Kairos
//! derives them from `KAIROS_WEBHOOK_SIGNING_KEY`, so they have nothing to
//! encrypt again. The API keys are hashes, not sealed secrets.
//!
//! A row that the new key sealed is left as it is. A row that no key of
//! the keyring opens is not changed, and the report names it: the operator
//! sets that secret again.

use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use crate::secrets::{SecretsKey, credential_aad};

/// What [`rotate_tenant`] did in one tenant.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotationReport {
    /// The rows that the new key seals now.
    pub resealed: usize,
    /// The rows that the new key sealed already.
    pub current: usize,
    /// The rows that no key of the keyring opens: `table.column (row)`.
    pub failed: Vec<String>,
}

/// Encrypt each sealed row of the tenant `slug` again with the key that
/// encrypts. One transaction for the tenant.
pub fn rotate_tenant(
    conn: &mut PgConnection,
    slug: &str,
    key: &SecretsKey,
) -> Result<RotationReport, diesel::result::Error> {
    let schema = kairos_db::tenant::tenant_schema_name(slug);
    conn.transaction(|conn| {
        diesel::sql_query(format!("SET LOCAL search_path TO \"{schema}\", public"))
            .execute(conn)?;
        let mut report = RotationReport::default();
        rotate_credentials(conn, slug, key, &mut report)?;
        rotate_code_index_settings(conn, slug, key, &mut report)?;
        rotate_webhook_targets(conn, slug, key, &mut report)?;
        rotate_write_tokens(conn, slug, key, &mut report)?;
        rotate_org_read_tokens(conn, slug, key, &mut report)?;
        Ok(report)
    })
}

fn rotate_credentials(
    conn: &mut PgConnection,
    slug: &str,
    key: &SecretsKey,
    report: &mut RotationReport,
) -> Result<(), diesel::result::Error> {
    use kairos_db::schema::repository_credentials as rc;
    let rows: Vec<(Uuid, Vec<u8>, Vec<u8>, String)> = rc::table
        .select((rc::repository_id, rc::ciphertext, rc::nonce, rc::key_id))
        .for_update()
        .load(conn)?;
    for (repository_id, ciphertext, nonce, key_id) in rows {
        if !key.needs_reseal(&key_id) {
            report.current += 1;
            continue;
        }
        let aad = credential_aad(slug, repository_id);
        match key.open(&aad, &key_id, &nonce, &ciphertext) {
            Ok(plain) => {
                let sealed = key.seal(&aad, &plain);
                diesel::update(rc::table.filter(rc::repository_id.eq(repository_id)))
                    .set((
                        rc::ciphertext.eq(sealed.ciphertext),
                        rc::nonce.eq(sealed.nonce),
                        rc::key_id.eq(sealed.key_id),
                    ))
                    .execute(conn)?;
                report.resealed += 1;
            }
            Err(e) => report
                .failed
                .push(format!("repository_credentials ({repository_id}): {e}")),
        }
    }
    Ok(())
}

/// KAIROS-T-0386: the write tokens of the repositories.
fn rotate_write_tokens(
    conn: &mut PgConnection,
    slug: &str,
    key: &SecretsKey,
    report: &mut RotationReport,
) -> Result<(), diesel::result::Error> {
    for token in kairos_db::writeback::tokens_for_update(conn)? {
        if !key.needs_reseal(&token.key_id) {
            report.current += 1;
            continue;
        }
        let aad = crate::secrets::write_token_aad(slug, token.repository_id);
        match key.open(&aad, &token.key_id, &token.nonce, &token.ciphertext) {
            Ok(plain) => {
                let again = key.seal(&aad, &plain);
                kairos_db::writeback::reseal_token(
                    conn,
                    &kairos_db::writeback::StoredWriteToken {
                        repository_id: token.repository_id,
                        ciphertext: again.ciphertext,
                        nonce: again.nonce,
                        key_id: again.key_id,
                    },
                )?;
                report.resealed += 1;
            }
            Err(e) => report.failed.push(format!(
                "repository_write_tokens ({}): {e}",
                token.repository_id
            )),
        }
    }
    Ok(())
}

/// KAIROS-T-0431: the read tokens of the organization by host.
fn rotate_org_read_tokens(
    conn: &mut PgConnection,
    slug: &str,
    key: &SecretsKey,
    report: &mut RotationReport,
) -> Result<(), diesel::result::Error> {
    for token in kairos_db::org_read_tokens::all_for_update(conn)? {
        if !key.needs_reseal(&token.key_id) {
            report.current += 1;
            continue;
        }
        let aad = crate::secrets::org_read_token_aad(slug, &token.host);
        match key.open(&aad, &token.key_id, &token.nonce, &token.ciphertext) {
            Ok(plain) => {
                let again = key.seal(&aad, &plain);
                kairos_db::org_read_tokens::reseal(
                    conn,
                    &kairos_db::org_read_tokens::StoredOrgToken {
                        host: token.host.clone(),
                        ciphertext: again.ciphertext,
                        nonce: again.nonce,
                        key_id: again.key_id,
                    },
                )?;
                report.resealed += 1;
            }
            Err(e) => report
                .failed
                .push(format!("org_read_tokens ({}): {e}", token.host)),
        }
    }
    Ok(())
}

/// KAIROS-T-0384: the secrets of the webhook targets.
fn rotate_webhook_targets(
    conn: &mut PgConnection,
    slug: &str,
    key: &SecretsKey,
    report: &mut RotationReport,
) -> Result<(), diesel::result::Error> {
    for (id, sealed) in kairos_db::webhooks::secrets_for_update(conn)? {
        if !key.needs_reseal(&sealed.key_id) {
            report.current += 1;
            continue;
        }
        let aad = crate::secrets::webhook_target_aad(slug, id);
        match key.open(&aad, &sealed.key_id, &sealed.nonce, &sealed.ciphertext) {
            Ok(plain) => {
                let again = key.seal(&aad, &plain);
                kairos_db::webhooks::set_secret(
                    conn,
                    id,
                    &kairos_db::webhooks::SealedSecret {
                        ciphertext: again.ciphertext,
                        nonce: again.nonce,
                        key_id: again.key_id,
                    },
                )?;
                report.resealed += 1;
            }
            Err(e) => report
                .failed
                .push(format!("webhook_targets.secret ({id}): {e}")),
        }
    }
    Ok(())
}

/// The sealed secret of one field of `code_index_settings`.
type SealedColumns = (Option<Vec<u8>>, Option<Vec<u8>>, Option<String>);

fn rotate_code_index_settings(
    conn: &mut PgConnection,
    slug: &str,
    key: &SecretsKey,
    report: &mut RotationReport,
) -> Result<(), diesel::result::Error> {
    use kairos_db::schema::code_index_settings as cs;
    let rows: Vec<(i32, SealedColumns, SealedColumns)> = cs::table
        .select((
            cs::id,
            (
                cs::summary_ciphertext,
                cs::summary_nonce,
                cs::summary_key_id,
            ),
            (cs::vector_ciphertext, cs::vector_nonce, cs::vector_key_id),
        ))
        .for_update()
        .load(conn)?;
    for (id, summary, vector) in rows {
        for (field, sealed) in [("summary.secret", summary), ("vectors.secret", vector)] {
            let (Some(ciphertext), Some(nonce), Some(key_id)) = sealed else {
                continue;
            };
            if !key.needs_reseal(&key_id) {
                report.current += 1;
                continue;
            }
            let aad = crate::api::org::code_index_settings::settings_aad(slug, field);
            let plain = match key.open(&aad, &key_id, &nonce, &ciphertext) {
                Ok(plain) => plain,
                Err(e) => {
                    report
                        .failed
                        .push(format!("code_index_settings {field} ({id}): {e}"));
                    continue;
                }
            };
            let sealed = key.seal(&aad, &plain);
            let target = cs::table.filter(cs::id.eq(id));
            if field == "summary.secret" {
                diesel::update(target)
                    .set((
                        cs::summary_ciphertext.eq(Some(sealed.ciphertext)),
                        cs::summary_nonce.eq(Some(sealed.nonce)),
                        cs::summary_key_id.eq(Some(sealed.key_id)),
                    ))
                    .execute(conn)?;
            } else {
                diesel::update(target)
                    .set((
                        cs::vector_ciphertext.eq(Some(sealed.ciphertext)),
                        cs::vector_nonce.eq(Some(sealed.nonce)),
                        cs::vector_key_id.eq(Some(sealed.key_id)),
                    ))
                    .execute(conn)?;
            }
            report.resealed += 1;
        }
    }
    Ok(())
}
