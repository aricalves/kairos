//! The owner of a strategy or an initiative (KAIROS-T-0411, KAIROS-A-0025
//! decisions 2 to 7).
//!
//! A strategy or an initiative has one owner at most: a person, who
//! answers for the item. It can have no owner. The owner is a CLAIM, as
//! the claim of a task ([`crate::task_claims`]), with three differences:
//!
//! - it is not tied to a column: it stays in each column, Completed too,
//!   and when the item is archived;
//! - it is taken explicitly ([`claim`]), never by a move;
//! - it gives no right. Nothing in [`crate::abac`] reads this module: the
//!   owner can do what a member of the board can do, and nothing more.
//!
//! A new item and an imported item have no owner: no create path writes
//! to `item_owners`.
//!
//! # The changes
//!
//! - [`claim`]: a person takes an item that has no owner.
//! - [`hand_off`]: the owner (or an org admin) gives the item to a person.
//!   An org admin also SETS the owner of an item with no owner with it.
//! - [`clear`]: an org admin removes the owner.
//! - [`drop_owner_of_user`]: the owner left the organization or was
//!   deactivated. The server calls it in the transaction that removes the
//!   membership, so the items have no owner from that moment.
//!
//! Who may do which change is the rule of the server
//! (`kairos_server::owners`); this module checks only the state.
//!
//! Each change writes one `activity_log` row with the item as the entity
//! (`entity_type` `strategy` or `initiative`), in the vocabulary of the
//! claim of a task:
//!
//! - `claim`: `claim:<user id>`.
//! - `hand_off`: `hand_off:<user id or none>-><user id>`.
//! - `release`: `release:<user id> reason:<reason>`, where the reason is
//!   `release` (the owner gave it up), `clear` (an org admin removed a
//!   different person) or `left` (the person left the organization).
//!
//! Each change also emits `item.updated` for the item.
//!
//! Every public function operates in the CURRENT `search_path` tenant
//! schema.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use kairos_core::short_code::ItemType;
use uuid::Uuid;

use crate::events::{self, EventKind};
use crate::models::enums::ActivityAction;
use crate::models::graph::NewActivityLogEntry;
use crate::schema::item_owners;
use crate::task_claims::{display_names, is_person};

/// Can an item of this type have an owner? A strategy and an initiative
/// can.
pub fn is_subject(item_type: ItemType) -> bool {
    matches!(item_type, ItemType::Initiative | ItemType::Strategy)
}

/// The owner of a strategy or an initiative.
#[derive(Debug, Clone, PartialEq, Eq, Queryable, Selectable, Identifiable)]
#[diesel(table_name = item_owners)]
#[diesel(primary_key(item_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ItemOwner {
    /// The strategy or the initiative.
    pub item_id: Uuid,
    /// The person (`public.users.id`). Always a person, never a service
    /// account.
    pub user_id: Uuid,
    /// The time of the claim or of the last hand-off.
    pub claimed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = item_owners)]
struct NewItemOwner {
    item_id: Uuid,
    user_id: Uuid,
}

/// Errors of [`claim`], [`hand_off`] and [`clear`].
#[derive(Debug, thiserror::Error)]
pub enum OwnerError {
    /// No live strategy or initiative has this id.
    #[error("No live strategy or initiative has the id {0}.")]
    ItemNotFound(Uuid),
    /// `claim`: the item has an owner.
    #[error(
        "{short_code} has an owner: {name}. Only the owner can hand it off, or an organization \
         admin can change it. Kairos changed nothing."
    )]
    HasOwner { short_code: String, name: String },
    /// `hand_off`: the person is the owner already.
    #[error("{name} is the owner of {short_code} already. Kairos changed nothing.")]
    AlreadyOwner { short_code: String, name: String },
    /// `clear`: the item has no owner.
    #[error("{short_code} has no owner. Kairos changed nothing.")]
    NoOwner { short_code: String },
    /// The person is not a person (a service account, or no user).
    #[error(
        "Only a person can be the owner. {0} is not a person. Send a person of the organization."
    )]
    NotAPerson(String),
    /// Any other database error.
    #[error("The database gave an error: {0}.")]
    Database(#[from] DieselError),
}

/// The owner of the item, or `None`.
pub fn owner_of(conn: &mut PgConnection, item_id: Uuid) -> Result<Option<ItemOwner>, DieselError> {
    item_owners::table
        .filter(item_owners::item_id.eq(item_id))
        .select(ItemOwner::as_select())
        .first(conn)
        .optional()
}

/// The owners of the items in `item_ids`, by item. An item with no owner
/// is not in the map. One query.
pub fn owners_of(
    conn: &mut PgConnection,
    item_ids: &[Uuid],
) -> Result<HashMap<Uuid, ItemOwner>, DieselError> {
    if item_ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(item_owners::table
        .filter(item_owners::item_id.eq_any(item_ids))
        .select(ItemOwner::as_select())
        .load(conn)?
        .into_iter()
        .map(|owner| (owner.item_id, owner))
        .collect())
}

/// `(short_code, title, entity_type)` of a strategy or an initiative.
/// `live_only` leaves out an archived item.
fn subject(
    conn: &mut PgConnection,
    item_id: Uuid,
    live_only: bool,
) -> Result<Option<(String, String, &'static str)>, DieselError> {
    use crate::schema::{initiatives, strategies};
    let mut query = strategies::table
        .filter(strategies::id.eq(item_id))
        .select((strategies::short_code, strategies::title))
        .into_boxed();
    if live_only {
        query = query.filter(strategies::deleted_at.is_null());
    }
    if let Some((code, title)) = query.first::<(String, String)>(conn).optional()? {
        return Ok(Some((code, title, "strategy")));
    }
    let mut query = initiatives::table
        .filter(initiatives::id.eq(item_id))
        .select((initiatives::short_code, initiatives::title))
        .into_boxed();
    if live_only {
        query = query.filter(initiatives::deleted_at.is_null());
    }
    Ok(query
        .first::<(String, String)>(conn)
        .optional()?
        .map(|(code, title)| (code, title, "initiative")))
}

/// The display name of a user, or the id.
fn name_of(conn: &mut PgConnection, user_id: Uuid) -> Result<String, DieselError> {
    Ok(display_names(conn, &[user_id])?
        .remove(&user_id)
        .unwrap_or_else(|| user_id.to_string()))
}

fn log(
    conn: &mut PgConnection,
    actor_id: Uuid,
    action: ActivityAction,
    (item_id, entity_type): (Uuid, &str),
    details: String,
) -> Result<(), DieselError> {
    diesel::insert_into(crate::schema::activity_log::table)
        .values(NewActivityLogEntry {
            actor_id,
            action,
            entity_id: Some(item_id),
            entity_type: Some(entity_type.to_string()),
            details,
        })
        .execute(conn)?;
    Ok(())
}

/// The person `user_id` takes the item, which has no owner (decision 3).
/// The actor is the person.
pub fn claim(
    conn: &mut PgConnection,
    item_id: Uuid,
    user_id: Uuid,
) -> Result<ItemOwner, OwnerError> {
    conn.transaction::<_, OwnerError, _>(|conn| {
        let (short_code, _, entity_type) =
            subject(conn, item_id, true)?.ok_or(OwnerError::ItemNotFound(item_id))?;
        if !is_person(conn, user_id)? {
            return Err(OwnerError::NotAPerson(user_id.to_string()));
        }
        // ON CONFLICT DO NOTHING: of two claims at the same time, one
        // gets the row, and the other is told who has it.
        let inserted: Option<ItemOwner> = diesel::insert_into(item_owners::table)
            .values(NewItemOwner { item_id, user_id })
            .on_conflict_do_nothing()
            .returning(ItemOwner::as_returning())
            .get_result(conn)
            .optional()?;
        let Some(owner) = inserted else {
            let holder = owner_of(conn, item_id)?.map_or(user_id, |o| o.user_id);
            return Err(OwnerError::HasOwner {
                short_code,
                name: name_of(conn, holder)?,
            });
        };
        log(
            conn,
            user_id,
            ActivityAction::Claim,
            (item_id, entity_type),
            format!("claim:{user_id}"),
        )?;
        events::emit_item_event_by_id(conn, EventKind::ItemUpdated, entity_type, item_id, user_id)?;
        Ok(owner)
    })
}

/// The item gets the owner `to_user` (decision 4): a hand-off from the
/// owner, or an org admin who sets the owner. The item can have an owner
/// or not. `actor_id` sends it. The new owner is told.
pub fn hand_off(
    conn: &mut PgConnection,
    item_id: Uuid,
    to_user: Uuid,
    actor_id: Uuid,
) -> Result<ItemOwner, OwnerError> {
    conn.transaction::<_, OwnerError, _>(|conn| {
        let (short_code, title, entity_type) =
            subject(conn, item_id, true)?.ok_or(OwnerError::ItemNotFound(item_id))?;
        if !is_person(conn, to_user)? {
            return Err(OwnerError::NotAPerson(to_user.to_string()));
        }
        let existing = owner_of(conn, item_id)?;
        if existing.as_ref().is_some_and(|o| o.user_id == to_user) {
            return Err(OwnerError::AlreadyOwner {
                short_code,
                name: name_of(conn, to_user)?,
            });
        }
        let owner: ItemOwner = diesel::insert_into(item_owners::table)
            .values(NewItemOwner {
                item_id,
                user_id: to_user,
            })
            .on_conflict(item_owners::item_id)
            .do_update()
            .set((
                item_owners::user_id.eq(to_user),
                item_owners::claimed_at.eq(diesel::dsl::now),
            ))
            .returning(ItemOwner::as_returning())
            .get_result(conn)?;
        let from = existing.map_or_else(|| "none".to_string(), |o| o.user_id.to_string());
        log(
            conn,
            actor_id,
            ActivityAction::HandOff,
            (item_id, entity_type),
            format!("hand_off:{from}->{to_user}"),
        )?;
        crate::notifications::on_owner_hand_off(
            conn,
            (item_id, &short_code, &title),
            to_user,
            actor_id,
        )?;
        events::emit_item_event_by_id(
            conn,
            EventKind::ItemUpdated,
            entity_type,
            item_id,
            actor_id,
        )?;
        Ok(owner)
    })
}

/// Remove the owner of the item (decision 4 and its amendment 3: the owner
/// or an org admin). The reason is `release` when `actor_id` is the owner,
/// else `clear`. Returns the owner that the item had.
pub fn clear(
    conn: &mut PgConnection,
    item_id: Uuid,
    actor_id: Uuid,
) -> Result<ItemOwner, OwnerError> {
    conn.transaction::<_, OwnerError, _>(|conn| {
        let (short_code, _, entity_type) =
            subject(conn, item_id, true)?.ok_or(OwnerError::ItemNotFound(item_id))?;
        let ended: Option<ItemOwner> =
            diesel::delete(item_owners::table.filter(item_owners::item_id.eq(item_id)))
                .returning(ItemOwner::as_returning())
                .get_result(conn)
                .optional()?;
        let ended = ended.ok_or(OwnerError::NoOwner { short_code })?;
        log(
            conn,
            actor_id,
            ActivityAction::Release,
            (item_id, entity_type),
            format!(
                "release:{} reason:{}",
                ended.user_id,
                if ended.user_id == actor_id {
                    "release"
                } else {
                    "clear"
                }
            ),
        )?;
        events::emit_item_event_by_id(
            conn,
            EventKind::ItemUpdated,
            entity_type,
            item_id,
            actor_id,
        )?;
        Ok(ended)
    })
}

/// The person `user_id` left the organization or was deactivated
/// (decision 7): each item that the person owns has no owner now. Call it
/// in the transaction that removes the membership. `actor_id` removed the
/// membership. Returns the items, archived ones too.
pub fn drop_owner_of_user(
    conn: &mut PgConnection,
    user_id: Uuid,
    actor_id: Uuid,
) -> Result<Vec<Uuid>, DieselError> {
    let ended: Vec<ItemOwner> =
        diesel::delete(item_owners::table.filter(item_owners::user_id.eq(user_id)))
            .returning(ItemOwner::as_returning())
            .get_results(conn)?;
    let mut items = Vec::with_capacity(ended.len());
    for owner in ended {
        let entity_type = subject(conn, owner.item_id, false)?.map_or("initiative", |s| s.2);
        log(
            conn,
            actor_id,
            ActivityAction::Release,
            (owner.item_id, entity_type),
            format!("release:{user_id} reason:left"),
        )?;
        events::emit_item_event_by_id(
            conn,
            EventKind::ItemUpdated,
            entity_type,
            owner.item_id,
            actor_id,
        )?;
        items.push(owner.item_id);
    }
    Ok(items)
}
