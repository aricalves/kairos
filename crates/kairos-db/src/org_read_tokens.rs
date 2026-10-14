//! The read tokens of the organization (KAIROS-T-0431): one token for each
//! forge host. The builder of the code index uses the token of the host of
//! a repository before the token of the repository
//! ([`crate::repository_credentials`]).
//!
//! This module stores and reads the ENCRYPTED form only. The encryption and
//! the key (`KAIROS_SECRETS_KEY`) are in the server. One row for each host
//! (`org_read_tokens`).
//!
//! - [`put`] writes the row (a new one, or a replacement).
//! - [`statuses`] gives what a read shows: the host, who set the token and
//!   when. It never gives the ciphertext.
//! - [`load`] gives the ciphertext, for the server to decrypt.
//! - [`delete`] removes the row.
//!
//! Each write writes an activity row with the actor. The row holds the
//! host, never the token.
//!
//! The connection must be pinned to a tenant schema. This module does not
//! check rights: the caller does.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use crate::models::enums::ActivityAction;
use crate::models::graph::NewActivityLogEntry;
use crate::repository_credentials::PutOutcome;
use crate::schema::org_read_tokens;

/// The encrypted token of a host, to write.
#[derive(Clone, PartialEq, Eq, Insertable)]
#[diesel(table_name = org_read_tokens)]
pub struct NewOrgToken {
    /// The normalized host (see `kairos_core::repositories::forge_host`).
    pub host: String,
    /// The AES-256-GCM ciphertext with its tag.
    pub ciphertext: Vec<u8>,
    /// 12 random bytes.
    pub nonce: Vec<u8>,
    /// The fingerprint of the key that encrypted the token.
    pub key_id: String,
    pub set_by: Uuid,
}

impl std::fmt::Debug for NewOrgToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewOrgToken")
            .field("host", &self.host)
            .field("key_id", &self.key_id)
            .field("set_by", &self.set_by)
            .finish_non_exhaustive()
    }
}

/// The stored, encrypted token of a host.
#[derive(Clone, PartialEq, Eq, Queryable, Selectable)]
#[diesel(table_name = org_read_tokens)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct StoredOrgToken {
    pub host: String,
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub key_id: String,
}

impl std::fmt::Debug for StoredOrgToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredOrgToken")
            .field("host", &self.host)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// What a read of the token of a host shows. It has no ciphertext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgTokenStatus {
    pub host: String,
    pub set_by: Uuid,
    /// The display name of `set_by`, else the email. `None` when the user
    /// is not in `public.users`.
    pub set_by_name: Option<String>,
    pub set_at: DateTime<Utc>,
}

/// Write the encrypted token of a host, and an activity row for
/// `token.set_by` (`org_read_token_set:<host>` or
/// `org_read_token_replaced:<host>`).
pub fn put(
    conn: &mut PgConnection,
    token: NewOrgToken,
) -> Result<PutOutcome, diesel::result::Error> {
    use org_read_tokens::dsl;
    conn.transaction(|conn| {
        let existed: bool = diesel::select(diesel::dsl::exists(
            dsl::org_read_tokens.filter(dsl::host.eq(&token.host)),
        ))
        .get_result(conn)?;
        diesel::insert_into(dsl::org_read_tokens)
            .values(&token)
            .on_conflict(dsl::host)
            .do_update()
            .set((
                dsl::ciphertext.eq(&token.ciphertext),
                dsl::nonce.eq(&token.nonce),
                dsl::key_id.eq(&token.key_id),
                dsl::set_by.eq(token.set_by),
                dsl::set_at.eq(Utc::now()),
            ))
            .execute(conn)?;
        let (outcome, verb) = if existed {
            (PutOutcome::Replaced, "replaced")
        } else {
            (PutOutcome::Set, "set")
        };
        log_activity(
            conn,
            token.set_by,
            format!("org_read_token_{verb}:{}", token.host),
        )?;
        Ok(outcome)
    })
}

/// The encrypted token of `host`, if it has one.
pub fn load(
    conn: &mut PgConnection,
    host: &str,
) -> Result<Option<StoredOrgToken>, diesel::result::Error> {
    use org_read_tokens::dsl;
    dsl::org_read_tokens
        .filter(dsl::host.eq(host))
        .select(StoredOrgToken::as_select())
        .first(conn)
        .optional()
}

/// Each stored token, for the rotation of the key. The rows are locked
/// until the end of the transaction.
pub fn all_for_update(
    conn: &mut PgConnection,
) -> Result<Vec<StoredOrgToken>, diesel::result::Error> {
    use org_read_tokens::dsl;
    dsl::org_read_tokens
        .select(StoredOrgToken::as_select())
        .for_update()
        .load(conn)
}

/// Write a token that a new key encrypted again. The status does not
/// change.
pub fn reseal(
    conn: &mut PgConnection,
    token: &StoredOrgToken,
) -> Result<(), diesel::result::Error> {
    use org_read_tokens::dsl;
    diesel::update(dsl::org_read_tokens.filter(dsl::host.eq(&token.host)))
        .set((
            dsl::ciphertext.eq(&token.ciphertext),
            dsl::nonce.eq(&token.nonce),
            dsl::key_id.eq(&token.key_id),
        ))
        .execute(conn)?;
    Ok(())
}

/// The status of the token of each host, by host.
pub fn statuses(conn: &mut PgConnection) -> Result<Vec<OrgTokenStatus>, diesel::result::Error> {
    use org_read_tokens::dsl;
    let rows: Vec<(String, Uuid, DateTime<Utc>)> = dsl::org_read_tokens
        .select((dsl::host, dsl::set_by, dsl::set_at))
        .order(dsl::host.asc())
        .load(conn)?;
    let mut actors: Vec<Uuid> = rows.iter().map(|r| r.1).collect();
    actors.sort_unstable();
    actors.dedup();
    let names: HashMap<Uuid, String> = {
        use crate::schema::users;
        users::table
            .filter(users::id.eq_any(&actors))
            .select((users::id, users::display_name, users::email))
            .load::<(Uuid, String, String)>(conn)?
            .into_iter()
            .map(|(id, display_name, email)| {
                let name = if display_name.trim().is_empty() {
                    email
                } else {
                    display_name
                };
                (id, name)
            })
            .collect()
    };
    Ok(rows
        .into_iter()
        .map(|(host, set_by, set_at)| OrgTokenStatus {
            host,
            set_by,
            set_by_name: names.get(&set_by).cloned(),
            set_at,
        })
        .collect())
}

/// Remove the token of `host`, and write an activity row
/// (`org_read_token_removed:<host>`) for `actor`. False: the host had no
/// token, and nothing is written.
pub fn delete(
    conn: &mut PgConnection,
    host: &str,
    actor: Uuid,
) -> Result<bool, diesel::result::Error> {
    use org_read_tokens::dsl;
    conn.transaction(|conn| {
        let removed =
            diesel::delete(dsl::org_read_tokens.filter(dsl::host.eq(host))).execute(conn)?;
        if removed == 0 {
            return Ok(false);
        }
        log_activity(conn, actor, format!("org_read_token_removed:{host}"))?;
        Ok(true)
    })
}

fn log_activity(
    conn: &mut PgConnection,
    actor_id: Uuid,
    details: String,
) -> Result<(), diesel::result::Error> {
    diesel::insert_into(crate::schema::activity_log::table)
        .values(NewActivityLogEntry {
            actor_id,
            action: ActivityAction::Update,
            entity_id: None,
            entity_type: Some("org_read_token".to_string()),
            details,
        })
        .execute(conn)?;
    Ok(())
}
