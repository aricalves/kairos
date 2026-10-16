//! The external references (KAIROS-T-0508, COLLIERY-I-0630): an item
//! links to a URI with a label. The API and the tools show the link as
//! the relationship `external`.
//!
//! # Why a table of its own
//!
//! The rows are in `item_external_refs` and not in `item_relationships`,
//! for the reason that [`crate::impacts`] gives: each end of an
//! `item_relationships` row is an item, and the graph view, the traversal
//! of the search and the cascade of an archive read that table with that
//! rule. A URI is not an item: it has no short code and no row in
//! `entity_directory`.
//!
//! # The ends
//!
//! The SUBJECT is any live item: an RFC can back a task, an initiative, a
//! document or an ADR. The TARGET is a URI that passes
//! [`kairos_core::external_ref::parse_uri`]: the caller applies the rule
//! before the write, and [`link`] applies it again so that no row can hold
//! a URI that the web page must not render.
//!
//! # Liveness
//!
//! A write needs a live subject. A reference stays when its item is
//! archived: a read gives it, as the edges of the item stay.
//!
//! # Audit rows and events
//!
//! The add writes `action = 'relationship_add'`, and the remove
//! `action = 'relationship_remove'`, as the edges of [`crate::graph`] and
//! the links of [`crate::impacts`] do. `details` is
//! `relationship:external:{short_code}->uri:{uri}`. Each write emits
//! `relationship_changed` for the item.
//!
//! # Duplicate references
//!
//! `UNIQUE (item_id, uri)` is the enforcement. The add of a URI that the
//! item has is the typed [`ExternalRefError::AlreadyLinked`]. A second
//! label for one URI is an edit, not a second reference: remove and add.
//!
//! Every public function operates in the CURRENT `search_path` tenant
//! schema.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::{DatabaseErrorKind, Error as DieselError};
use diesel::sql_query;
use diesel::sql_types::{Array, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use uuid::Uuid;

use kairos_core::external_ref::{self, ExternalRefError as UriError};
use kairos_core::short_code::ItemType;

use crate::events::{self, EventKind};
use crate::models::enums::ActivityAction;
use crate::models::graph::NewActivityLogEntry;

/// The name of the relationship, as the API and the tools show it.
pub const RELATIONSHIP: &str = external_ref::RELATIONSHIP;

/// Errors from the external reference services.
#[derive(Debug, thiserror::Error)]
pub enum ExternalRefError {
    /// No live item has this id.
    #[error("The item {0} does not exist.")]
    ItemNotFound(Uuid),
    /// The URI or the label does not pass the rule.
    #[error(transparent)]
    Form(#[from] UriError),
    /// The item has this URI already: nothing changed, and no activity
    /// row was written.
    #[error("{short_code} has the external reference {uri} already.")]
    AlreadyLinked { short_code: String, uri: String },
    /// The item has no reference with this URI.
    #[error("{short_code} has no external reference {uri}.")]
    NotLinked { short_code: String, uri: String },
    /// Any other database error.
    #[error("The database gave an error: {0}.")]
    Database(#[from] DieselError),
}

/// One external reference of an item.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct ExternalRef {
    /// The id of the reference.
    #[diesel(sql_type = SqlUuid)]
    pub id: Uuid,
    /// The item.
    #[diesel(sql_type = SqlUuid)]
    pub item_id: Uuid,
    /// The URI, as [`kairos_core::external_ref::parse_uri`] gives it.
    #[diesel(sql_type = Text)]
    pub uri: String,
    /// The label, when the reference has one.
    #[diesel(sql_type = Nullable<Text>)]
    pub label: Option<String>,
    /// Who made the reference.
    #[diesel(sql_type = SqlUuid)]
    pub created_by: Uuid,
    /// When the reference was made.
    #[diesel(sql_type = Timestamptz)]
    pub created_at: DateTime<Utc>,
}

impl ExternalRef {
    /// The label when there is one, the URI when there is none.
    pub fn display_text(&self) -> String {
        external_ref::display_text(&self.uri, self.label.as_deref())
    }
}

#[derive(QueryableByName)]
struct SubjectRow {
    #[diesel(sql_type = Text)]
    entity_type: String,
    #[diesel(sql_type = Text)]
    short_code: String,
}

/// The type and the short code of a LIVE item.
fn live_subject(
    conn: &mut PgConnection,
    item_id: Uuid,
) -> Result<(ItemType, String), ExternalRefError> {
    let row: Option<SubjectRow> = sql_query(
        "SELECT entity_type, short_code FROM entity_directory \
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind::<SqlUuid, _>(item_id)
    .get_result(conn)
    .optional()?;
    let row = row.ok_or(ExternalRefError::ItemNotFound(item_id))?;
    let item_type = ItemType::ALL
        .iter()
        .copied()
        .find(|t| t.entity_type() == row.entity_type)
        .ok_or(ExternalRefError::ItemNotFound(item_id))?;
    Ok((item_type, row.short_code))
}

fn log_activity(
    conn: &mut PgConnection,
    actor: Uuid,
    action: ActivityAction,
    (item_id, item_type, short_code): (Uuid, ItemType, &str),
    uri: &str,
) -> Result<(), DieselError> {
    diesel::insert_into(crate::schema::activity_log::table)
        .values(NewActivityLogEntry {
            actor_id: actor,
            action,
            entity_id: Some(item_id),
            entity_type: Some(item_type.entity_type().to_string()),
            details: format!("relationship:{RELATIONSHIP}:{short_code}->uri:{uri}"),
        })
        .execute(conn)?;
    Ok(())
}

/// Make the reference "`item_id` -[external]-> `uri`", in ONE transaction:
///
/// 1. the URI and the label pass the rule ([`ExternalRefError::Form`]);
/// 2. the item is live ([`ExternalRefError::ItemNotFound`]);
/// 3. insert (a URI that the item has is
///    [`ExternalRefError::AlreadyLinked`]), write the `relationship_add`
///    activity row, and emit `relationship_changed` for the item.
///
/// NO PERMISSION RULE, as the services of [`crate::graph`]: the caller
/// asks the edit rule of the item first.
pub fn link(
    conn: &mut PgConnection,
    item_id: Uuid,
    uri: &str,
    label: Option<&str>,
    actor: Uuid,
) -> Result<ExternalRef, ExternalRefError> {
    let uri = external_ref::parse_uri(uri)?;
    let label = external_ref::parse_label(label)?;
    conn.transaction::<_, ExternalRefError, _>(|conn| {
        let (item_type, short_code) = live_subject(conn, item_id)?;
        let inserted = sql_query(
            "INSERT INTO item_external_refs (item_id, item_type, uri, label, created_by) \
             VALUES ($1, $2, $3, $4, $5) \
             RETURNING id, item_id, uri, label, created_by, created_at",
        )
        .bind::<SqlUuid, _>(item_id)
        .bind::<Text, _>(item_type.entity_type())
        .bind::<Text, _>(&uri)
        .bind::<Nullable<Text>, _>(label.as_deref())
        .bind::<SqlUuid, _>(actor)
        .get_result::<ExternalRef>(conn);
        let row = match inserted {
            Err(DieselError::DatabaseError(DatabaseErrorKind::UniqueViolation, _)) => {
                return Err(ExternalRefError::AlreadyLinked { short_code, uri });
            }
            other => other?,
        };
        log_activity(
            conn,
            actor,
            ActivityAction::RelationshipAdd,
            (item_id, item_type, &short_code),
            &uri,
        )?;
        events::emit_item_event_by_id(
            conn,
            EventKind::RelationshipChanged,
            item_type.entity_type(),
            item_id,
            actor,
        )?;
        Ok(row)
    })
}

/// Remove the reference "`item_id` -[external]-> `uri`", in ONE
/// transaction, with the `relationship_remove` activity row and the
/// `relationship_changed` event. The item must be live. A URI that the
/// item does not have is [`ExternalRefError::NotLinked`]: nothing changed,
/// and no activity row was written. The URI is read with the rule of
/// [`link`], so `HTTPS://x` removes `https://x`.
pub fn unlink(
    conn: &mut PgConnection,
    item_id: Uuid,
    uri: &str,
    actor: Uuid,
) -> Result<ExternalRef, ExternalRefError> {
    let uri = external_ref::parse_uri(uri)?;
    conn.transaction::<_, ExternalRefError, _>(|conn| {
        let (item_type, short_code) = live_subject(conn, item_id)?;
        let deleted: Option<ExternalRef> = sql_query(
            "DELETE FROM item_external_refs WHERE item_id = $1 AND uri = $2 \
             RETURNING id, item_id, uri, label, created_by, created_at",
        )
        .bind::<SqlUuid, _>(item_id)
        .bind::<Text, _>(&uri)
        .get_result(conn)
        .optional()?;
        let row = deleted.ok_or(ExternalRefError::NotLinked {
            short_code: short_code.clone(),
            uri: uri.clone(),
        })?;
        log_activity(
            conn,
            actor,
            ActivityAction::RelationshipRemove,
            (item_id, item_type, &short_code),
            &uri,
        )?;
        events::emit_item_event_by_id(
            conn,
            EventKind::RelationshipChanged,
            item_type.entity_type(),
            item_id,
            actor,
        )?;
        Ok(row)
    })
}

/// The reference of an item with this id, or `None`. The remove of REST
/// names a reference by its id: a URI in a path segment needs encoding.
pub fn by_id(
    conn: &mut PgConnection,
    item_id: Uuid,
    id: Uuid,
) -> Result<Option<ExternalRef>, ExternalRefError> {
    Ok(sql_query(
        "SELECT id, item_id, uri, label, created_by, created_at \
           FROM item_external_refs WHERE item_id = $1 AND id = $2",
    )
    .bind::<SqlUuid, _>(item_id)
    .bind::<SqlUuid, _>(id)
    .get_result(conn)
    .optional()?)
}

const REFS_OF_SQL: &str = "\
    SELECT id, item_id, uri, label, created_by, created_at \
      FROM item_external_refs \
     WHERE item_id = ANY($1) \
     ORDER BY created_at, uri";

/// The external references of one item, oldest first. The item can be
/// archived: the references are a property of the item, as its edges are.
pub fn refs_of(
    conn: &mut PgConnection,
    item_id: Uuid,
) -> Result<Vec<ExternalRef>, ExternalRefError> {
    Ok(sql_query(REFS_OF_SQL)
        .bind::<Array<SqlUuid>, _>(vec![item_id])
        .load(conn)?)
}

/// [`refs_of`] for MANY items, in one query: what `related_work` needs.
/// An item with no reference has no entry.
pub fn refs_of_items(
    conn: &mut PgConnection,
    item_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<ExternalRef>>, ExternalRefError> {
    let mut by_item: HashMap<Uuid, Vec<ExternalRef>> = HashMap::new();
    if item_ids.is_empty() {
        return Ok(by_item);
    }
    let rows: Vec<ExternalRef> = sql_query(REFS_OF_SQL)
        .bind::<Array<SqlUuid>, _>(item_ids)
        .load(conn)?;
    for row in rows {
        by_item.entry(row.item_id).or_default().push(row);
    }
    Ok(by_item)
}
