//! Service-account API key storage (KAIROS-A-0017 / KAIROS-T-0057).
//!
//! `api_keys` is a TENANT table (like `scim_tokens`): each org's keys live in
//! its own `org_{slug}` schema, so every query here runs on a connection whose
//! `search_path` is pinned to the target tenant.
//!
//! Only the hex SHA-256 of the full key is stored. Hashing and the
//! `kairos_sk_<slug>_<hex>` key format are owned by the server layer
//! (`kairos-server`); this module never sees a raw secret. The "is this key
//! currently valid" decision (expiry/revocation) is likewise the caller's — the
//! auth path keeps its failures uniform (KAIROS-T-0058).

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use crate::schema::api_keys;

/// An API key row (`api_keys`, tenant schema). The row never carries the raw
/// secret — only its hash and a display `prefix`.
#[derive(Debug, Clone, PartialEq, Eq, Queryable, QueryableByName, Selectable, Identifiable)]
#[diesel(table_name = api_keys)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ApiKey {
    pub id: Uuid,
    /// The principal (`public.users.id`): a service account, or a person
    /// for an agent key (KAIROS-T-0359).
    pub user_id: Uuid,
    /// Operator label ("ci-deploy", ...).
    pub name: String,
    /// Hex SHA-256 of the full key string; never the secret itself.
    pub token_hash: String,
    /// Display-only leading characters of the key (never the secret).
    pub prefix: String,
    /// The org admin (`public.users.id`) who minted the key.
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    /// Optional expiry; `None` = never expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Best-effort last-seen; `None` until first use.
    pub last_used_at: Option<DateTime<Utc>>,
    /// Set on revocation; the row is retained for audit.
    pub revoked_at: Option<DateTime<Utc>>,
    /// The key that a rotation made in place of this key (KAIROS-T-0370).
    pub replaced_by: Option<Uuid>,
}

impl ApiKey {
    /// Whether the key is usable at `now`: not revoked and not past expiry.
    /// The auth path uses this AFTER the hash match so every failure is a
    /// uniform 401 (KAIROS-T-0058).
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|exp| exp > now)
    }
}

/// Insert for [`ApiKey`]; `id`/`created_at` come from column defaults and
/// `last_used_at`/`revoked_at` start NULL.
#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = api_keys)]
pub struct NewApiKey {
    pub user_id: Uuid,
    pub name: String,
    pub token_hash: String,
    pub prefix: String,
    pub created_by: Uuid,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Insert a new key row (connection must be tenant-pinned).
///
/// A system actor (KAIROS-T-0481) holds no key: the insert is refused.
pub fn create_key(conn: &mut PgConnection, new: NewApiKey) -> QueryResult<ApiKey> {
    if crate::forge_actor::is_system_user(conn, new.user_id)? {
        return Err(diesel::result::Error::QueryBuilderError(
            "a system actor holds no API key".into(),
        ));
    }
    diesel::insert_into(api_keys::table)
        .values(new)
        .returning(ApiKey::as_returning())
        .get_result(conn)
}

/// The key row with this hash, if any. Validity (expiry/revocation) is the
/// caller's check — this returns revoked/expired rows too.
pub fn find_by_hash(conn: &mut PgConnection, token_hash: &str) -> QueryResult<Option<ApiKey>> {
    api_keys::table
        .filter(api_keys::token_hash.eq(token_hash))
        .select(ApiKey::as_select())
        .first(conn)
        .optional()
}

/// All key rows for one service account, newest first (revoked included — the
/// listing is an audit surface).
pub fn list_keys(conn: &mut PgConnection, user_id: Uuid) -> QueryResult<Vec<ApiKey>> {
    api_keys::table
        .filter(api_keys::user_id.eq(user_id))
        .order(api_keys::created_at.desc())
        .select(ApiKey::as_select())
        .load(conn)
}

/// The key row with this id, if any.
pub fn find_key(conn: &mut PgConnection, id: Uuid) -> QueryResult<Option<ApiKey>> {
    api_keys::table
        .filter(api_keys::id.eq(id))
        .select(ApiKey::as_select())
        .first(conn)
        .optional()
}

/// Mark a key revoked (idempotence is the caller's policy).
pub fn revoke_key(conn: &mut PgConnection, id: Uuid) -> QueryResult<ApiKey> {
    diesel::update(api_keys::table.filter(api_keys::id.eq(id)))
        .set(api_keys::revoked_at.eq(diesel::dsl::now))
        .returning(ApiKey::as_returning())
        .get_result(conn)
}

/// Record that a key was just used, when its last stamp is older than
/// `older_than` or missing. Best-effort: the auth path ignores the result
/// so a write failure never blocks a request (KAIROS-T-0058). The stamp is
/// a coarse "when was this key last seen", so a request within the window
/// writes nothing: a read no longer writes on every call (KAIROS-T-0610).
/// Returns the rows written: 1, or 0 when the stamp was fresh.
pub fn touch_last_used(
    conn: &mut PgConnection,
    id: Uuid,
    older_than: DateTime<Utc>,
) -> QueryResult<usize> {
    diesel::update(
        api_keys::table.filter(api_keys::id.eq(id)).filter(
            api_keys::last_used_at
                .is_null()
                .or(api_keys::last_used_at.lt(older_than)),
        ),
    )
    .set(api_keys::last_used_at.eq(diesel::dsl::now))
    .execute(conn)
}

/// Rotate a key (KAIROS-T-0370): insert `new`, and end the old key at
/// `old_ends_at` (or at its own expiry when that is sooner), with a link to
/// the new key. One transaction; the old row is locked, so two rotations
/// of one key do not both succeed. `Ok(None)` when the old key was revoked
/// or rotated already, or when it does not exist.
pub fn rotate_key(
    conn: &mut PgConnection,
    old_id: Uuid,
    new: NewApiKey,
    old_ends_at: DateTime<Utc>,
) -> QueryResult<Option<(ApiKey, ApiKey)>> {
    conn.transaction(|conn| {
        let Some(old) = api_keys::table
            .filter(api_keys::id.eq(old_id))
            .select(ApiKey::as_select())
            .for_update()
            .first(conn)
            .optional()?
        else {
            return Ok(None);
        };
        if old.revoked_at.is_some() || old.replaced_by.is_some() {
            return Ok(None);
        }
        let created = create_key(conn, new)?;
        let ends = old.expires_at.map_or(old_ends_at, |e| e.min(old_ends_at));
        let old = diesel::update(api_keys::table.filter(api_keys::id.eq(old_id)))
            .set((
                api_keys::expires_at.eq(Some(ends)),
                api_keys::replaced_by.eq(Some(created.id)),
            ))
            .returning(ApiKey::as_returning())
            .get_result(conn)?;
        Ok(Some((created, old)))
    })
}

/// An agent key with its owner (KAIROS-T-0388): a key whose principal is a
/// person (`users.kind = 'human'`), not a service account.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct PersonKey {
    #[diesel(embed)]
    pub key: ApiKey,
    /// The email of the owner.
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub owner_email: String,
    /// The display name of the owner.
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub owner_name: String,
}

/// The select of [`PersonKey`]: the keys of persons only.
const SELECT_PERSON_KEY: &str = "SELECT k.*, u.email AS owner_email, u.display_name AS owner_name \
     FROM api_keys k JOIN public.users u ON u.id = k.user_id WHERE u.kind = 'human'";

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

/// One page of the agent keys of the persons of the tenant, newest first
/// (KAIROS-T-0388), and the count of all of them. `owner`: the keys of
/// one person only. Revoked keys are in the list.
pub fn list_person_keys(
    conn: &mut PgConnection,
    owner: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> QueryResult<(i64, Vec<PersonKey>)> {
    use diesel::sql_types::{BigInt, Nullable, Uuid as SqlUuid};
    let filter = " AND ($1::uuid IS NULL OR k.user_id = $1)";
    let total = diesel::sql_query(format!(
        "SELECT count(*) AS count FROM api_keys k JOIN public.users u ON u.id = k.user_id \
         WHERE u.kind = 'human'{filter}"
    ))
    .bind::<Nullable<SqlUuid>, _>(owner)
    .get_result::<Count>(conn)?
    .count;
    let rows = diesel::sql_query(format!(
        "{SELECT_PERSON_KEY}{filter} ORDER BY k.created_at DESC, k.id LIMIT $2 OFFSET $3"
    ))
    .bind::<Nullable<SqlUuid>, _>(owner)
    .bind::<BigInt, _>(limit)
    .bind::<BigInt, _>(offset)
    .load(conn)?;
    Ok((total, rows))
}

/// The agent key with this id, with its owner. `None` when no key has
/// the id, or when the key is the key of a service account.
pub fn find_person_key(conn: &mut PgConnection, id: Uuid) -> QueryResult<Option<PersonKey>> {
    diesel::sql_query(format!("{SELECT_PERSON_KEY} AND k.id = $1"))
        .bind::<diesel::sql_types::Uuid, _>(id)
        .get_result(conn)
        .optional()
}

/// Revoke each key of the person `user_id` that is not revoked
/// (KAIROS-T-0388: the person leaves the organization). Nothing for a
/// service account. Returns the keys revoked now, oldest first.
pub fn revoke_person_keys(conn: &mut PgConnection, user_id: Uuid) -> QueryResult<Vec<ApiKey>> {
    let mut rows: Vec<ApiKey> = diesel::sql_query(
        "UPDATE api_keys SET revoked_at = now() \
          WHERE user_id = $1 AND revoked_at IS NULL \
            AND EXISTS (SELECT 1 FROM public.users u WHERE u.id = $1 AND u.kind = 'human') \
         RETURNING *",
    )
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .load(conn)?;
    rows.sort_by_key(|k| (k.created_at, k.id));
    Ok(rows)
}

/// One key that [`notify_expiring_keys`] reports (KAIROS-T-0540).
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct ExpiringKey {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub id: Uuid,
    /// The principal: a person (an agent key) or a service account.
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub user_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub prefix: String,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    pub expires_at: DateTime<Utc>,
    /// `users.kind` of the principal.
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub principal_kind: String,
    /// The display name of the principal.
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub principal_name: String,
}

impl ExpiringKey {
    /// True for an agent key: the principal is a person.
    pub fn is_agent_key(&self) -> bool {
        self.principal_kind == crate::models::public::USER_KIND_HUMAN
    }
}

/// Report each key of the tenant of the current `search_path` that expires
/// within [`KEY_EXPIRING_DAYS`] days after `now` (KAIROS-T-0540): the
/// notification `key_expiring`, as Kairos (no actor). The person of an
/// agent key is told; for a key of a service account, the organization
/// admins are told ([`crate::notifications::on_key_expiring`]).
///
/// Each key is told once: the update sets `expiring_notified_at`, and a key
/// with the column set is not told again. Not a revoked key, not a key that
/// a rotation replaced (its new key is told in its turn), not a key with no
/// expiry, not a key that expired already. Returns the keys reported, by
/// expiry.
///
/// [`KEY_EXPIRING_DAYS`]: kairos_core::notifications::KEY_EXPIRING_DAYS
pub fn notify_expiring_keys(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
) -> QueryResult<Vec<ExpiringKey>> {
    use diesel::sql_types::Timestamptz;
    let until = now + chrono::Duration::days(kairos_core::notifications::KEY_EXPIRING_DAYS);
    conn.transaction(|conn| {
        let mut keys: Vec<ExpiringKey> = diesel::sql_query(
            "UPDATE api_keys k SET expiring_notified_at = $1 \
               FROM public.users u \
              WHERE u.id = k.user_id \
                AND k.expiring_notified_at IS NULL \
                AND k.revoked_at IS NULL \
                AND k.replaced_by IS NULL \
                AND k.expires_at > $1 \
                AND k.expires_at <= $2 \
             RETURNING k.id, k.user_id, k.name, k.prefix, k.expires_at, \
                       u.kind AS principal_kind, u.display_name AS principal_name",
        )
        .bind::<Timestamptz, _>(now)
        .bind::<Timestamptz, _>(until)
        .load(conn)?;
        keys.sort_by_key(|k| (k.expires_at, k.id));
        for key in &keys {
            crate::notifications::on_key_expiring(conn, key)?;
        }
        Ok(keys)
    })
}

/// [`notify_expiring_keys`] in each tenant (KAIROS-T-0540): the work of the
/// periodic pass `key-expiring`. `progress` is called before each tenant.
/// Returns the count of the keys reported.
pub fn key_expiring_pass(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    progress: &mut dyn FnMut(),
) -> Result<u64, crate::tenant::TenantError> {
    let mut reported = 0u64;
    for tenant_info in crate::tenant::list_tenants(conn)? {
        progress();
        if !tenant_info.schema_exists {
            continue;
        }
        let schema = crate::tenant::tenant_schema_name(&tenant_info.slug);
        diesel::sql_query(format!("SET search_path TO \"{schema}\"")).execute(conn)?;
        let result = notify_expiring_keys(conn, now);
        let reset = diesel::sql_query("SET search_path TO DEFAULT").execute(conn);
        reported += result?.len() as u64;
        reset?;
    }
    Ok(reported)
}
