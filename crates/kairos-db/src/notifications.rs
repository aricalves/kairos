//! The notifications of a person (KAIROS-T-0381, COLLIERY-I-0614).
//!
//! # The 5 events
//!
//! Each producer runs in the transaction of the change that it is about, so
//! a rolled back change makes no notification:
//!
//! - `hand_off`: a hand-off of the claim of a task ([`crate::task_claims::hand_off`])
//!   tells the person who gets the claim.
//! - `blocker_cleared`: an item that blocks a task (a `blocks` edge from
//!   the item to the task) goes into a done column, or is archived. The
//!   person who has the claim on the task is told, when the task is not
//!   done. An item that was done already when it was archived blocked
//!   nothing, so its archive tells nobody.
//! - `cancelled`: a cancel of a task ([`crate::task_cancellations::cancel_task`])
//!   tells the person who created the task, with the reason.
//! - `mention`: `@user_name` in the content of a new item, or a NEW mention
//!   in an update of the content (a note of `append_note` is an update). A
//!   mention is new when the content has more mentions of the name than
//!   before ([`kairos_core::notifications::new_mentions`]). A mention in
//!   code (a code span or a fenced block) is not a mention. A name that is
//!   not the user name of a person of the organization makes nothing.
//! - `p0_bug`: a bug with the priority P0 ([`kairos_core::notifications::is_p0_priority`])
//!   lands on a board with a team: its priority changes to P0, or it moves
//!   to the board. Each member of the team is told.
//!
//! # Who gets a notification
//!
//! Only a person (`users.kind = 'human'`) who is a member of the
//! organization of the tenant. Never the actor of the change: a person is
//! not told what they did. Not a person who turned the kind off
//! (`notification_settings`). [`notify`] applies the 3 rules in its one
//! insert.
//!
//! Each new notification sends the event `notification_created` to its
//! person only ([`crate::events::emit_notification_event`]).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{Array, BigInt, Bool, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use kairos_core::notifications::{NotificationKind, is_p0_priority};
use uuid::Uuid;

use crate::events;

/// One notification as a person reads it: the code and the title of the
/// item now (`None` when the item is gone).
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct Notification {
    #[diesel(sql_type = SqlUuid)]
    pub id: Uuid,
    #[diesel(sql_type = Text)]
    pub kind: String,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    pub item_id: Option<Uuid>,
    #[diesel(sql_type = Nullable<Text>)]
    pub short_code: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub entity_type: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub title: Option<String>,
    #[diesel(sql_type = Text)]
    pub text: String,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    pub actor_id: Option<Uuid>,
    #[diesel(sql_type = Timestamptz)]
    pub created_at: DateTime<Utc>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub read_at: Option<DateTime<Utc>>,
}

/// [`unread`]: the newest unread notifications, and the count of all the
/// unread ones.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Unread {
    pub newest: Vec<Notification>,
    pub total: i64,
}

#[derive(QueryableByName)]
struct UnreadRow {
    #[diesel(embed)]
    notification: Notification,
    #[diesel(sql_type = BigInt)]
    total: i64,
}

#[derive(QueryableByName)]
struct Recipient {
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
}

/// The columns of [`Notification`], with the item from `entity_directory`.
const SELECT_NOTIFICATION: &str = "SELECT n.id, n.kind, n.item_id, d.short_code, d.entity_type, \
     d.title, n.text, n.actor_id, n.created_at, n.read_at \
     FROM notifications n LEFT JOIN entity_directory d ON d.id = n.item_id";

/// Make one notification of `kind` for each person in `recipients` who may
/// get it (module docs: a person of the organization, not the actor, the
/// kind not turned off), and send each one the event. `short_code` is the
/// code of the item for the event. Returns the persons told.
pub fn notify(
    conn: &mut PgConnection,
    recipients: &[Uuid],
    kind: NotificationKind,
    item: Option<(Uuid, &str)>,
    text: &str,
    actor: Uuid,
) -> Result<Vec<Uuid>, DieselError> {
    if recipients.is_empty() || text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let told: Vec<Recipient> = sql_query(
        "INSERT INTO notifications (user_id, kind, item_id, text, actor_id) \
         SELECT DISTINCT u.id, $2, $3, $4, $5 \
           FROM public.users u \
           JOIN public.organization_members m ON m.user_id = u.id \
           JOIN public.organizations o ON o.id = m.organization_id \
          WHERE u.id = ANY($1) \
            AND u.kind = 'human' \
            AND u.id <> $5 \
            AND 'org_' || o.slug = current_schema() \
            AND NOT EXISTS (SELECT 1 FROM notification_settings s \
                             WHERE s.user_id = u.id AND s.kind = $2 AND NOT s.enabled) \
         RETURNING user_id",
    )
    .bind::<Array<SqlUuid>, _>(recipients)
    .bind::<Text, _>(kind.as_str())
    .bind::<Nullable<SqlUuid>, _>(item.map(|(id, _)| id))
    .bind::<Text, _>(text.trim())
    .bind::<SqlUuid, _>(actor)
    .load(conn)?;
    let short_code = item.map_or("", |(_, code)| code);
    let mut users: Vec<Uuid> = told.into_iter().map(|r| r.user_id).collect();
    users.sort();
    for user in &users {
        events::emit_notification_event(
            conn,
            events::NOTIFICATION_CREATED,
            *user,
            short_code,
            actor,
        )?;
    }
    Ok(users)
}

/// The display name of a user, or the id when the user has no name.
fn name_of(conn: &mut PgConnection, user: Uuid) -> Result<String, DieselError> {
    use crate::schema::users;
    let name: Option<String> = users::table
        .filter(users::id.eq(user))
        .select(users::display_name)
        .first(conn)
        .optional()?;
    Ok(name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| user.to_string()))
}

/// `(short_code, title, created_by)` of a task, archived or not.
fn task_head(
    conn: &mut PgConnection,
    task_id: Uuid,
) -> Result<Option<(String, String, Uuid)>, DieselError> {
    use crate::schema::tasks;
    tasks::table
        .filter(tasks::id.eq(task_id))
        .select((tasks::short_code, tasks::title, tasks::created_by))
        .first(conn)
        .optional()
}

/// `hand_off`: `to_user` got the claim of the task from `actor`.
pub fn on_hand_off(
    conn: &mut PgConnection,
    task_id: Uuid,
    to_user: Uuid,
    actor: Uuid,
) -> Result<(), DieselError> {
    let Some((code, title, _)) = task_head(conn, task_id)? else {
        return Ok(());
    };
    let text = format!(
        "{} gave you the claim on {code}: {title}",
        name_of(conn, actor)?
    );
    notify(
        conn,
        &[to_user],
        NotificationKind::HandOff,
        Some((task_id, &code)),
        &text,
        actor,
    )?;
    Ok(())
}

/// `cancelled`: `actor` cancelled the task, with `reason`. The person who
/// created the task is told.
pub fn on_cancel(
    conn: &mut PgConnection,
    task_id: Uuid,
    reason: &str,
    actor: Uuid,
) -> Result<(), DieselError> {
    let Some((code, title, created_by)) = task_head(conn, task_id)? else {
        return Ok(());
    };
    let text = format!(
        "{} cancelled {code}, which you created: {title}. The reason: {}",
        name_of(conn, actor)?,
        reason.trim()
    );
    notify(
        conn,
        &[created_by],
        NotificationKind::Cancelled,
        Some((task_id, &code)),
        &text,
        actor,
    )?;
    Ok(())
}

#[derive(QueryableByName)]
struct ClearedRow {
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    task_id: Uuid,
    #[diesel(sql_type = Text)]
    task_code: String,
    #[diesel(sql_type = Text)]
    blocker_code: String,
}

/// Why a blocker no longer blocks: [`on_blockers_cleared`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleared {
    /// The blocker went into a done column.
    Done,
    /// The blocker was archived. A blocker in a done column blocked nothing
    /// before the archive, so it tells nobody.
    Archived,
}

/// `blocker_cleared`: the items in `blockers` no longer block. For each
/// live task that one of them blocks, that is not done and that is not in
/// `blockers`, the person with the claim on the task is told.
pub fn on_blockers_cleared(
    conn: &mut PgConnection,
    blockers: &[Uuid],
    how: Cleared,
    actor: Uuid,
) -> Result<(), DieselError> {
    if blockers.is_empty() {
        return Ok(());
    }
    let rows: Vec<ClearedRow> = sql_query(
        "SELECT c.user_id, t.id AS task_id, t.short_code AS task_code, \
                b.short_code AS blocker_code \
           FROM item_relationships r \
           JOIN entity_directory b ON b.id = r.source_id \
           JOIN tasks t ON t.id = r.target_id AND t.deleted_at IS NULL \
           JOIN board_columns tc ON tc.id = t.column_id AND NOT tc.is_done \
           JOIN task_claims c ON c.task_id = t.id \
          WHERE r.relationship = 'blocks' \
            AND r.source_id = ANY($1) \
            AND NOT (r.target_id = ANY($1)) \
            AND (NOT $2 OR NOT EXISTS ( \
                 SELECT 1 FROM (SELECT id, column_id FROM strategies \
                                UNION ALL SELECT id, column_id FROM initiatives \
                                UNION ALL SELECT id, column_id FROM tasks \
                                UNION ALL SELECT id, column_id FROM adrs) p \
                   JOIN board_columns bc ON bc.id = p.column_id \
                  WHERE p.id = r.source_id AND bc.is_done)) \
          ORDER BY t.short_code, b.short_code",
    )
    .bind::<Array<SqlUuid>, _>(blockers)
    .bind::<Bool, _>(how == Cleared::Archived)
    .load(conn)?;
    for row in rows {
        let why = match how {
            Cleared::Done => "it is done",
            Cleared::Archived => "it is archived",
        };
        let text = format!(
            "{} does not block {} now: {why}.",
            row.blocker_code, row.task_code
        );
        notify(
            conn,
            &[row.user_id],
            NotificationKind::BlockerCleared,
            Some((row.task_id, &row.task_code)),
            &text,
            actor,
        )?;
    }
    Ok(())
}

/// Whether the column is done. An unknown column is not.
pub(crate) fn column_is_done(
    conn: &mut PgConnection,
    column_id: Uuid,
) -> Result<bool, DieselError> {
    use crate::schema::board_columns;
    Ok(board_columns::table
        .filter(board_columns::id.eq(column_id))
        .select(board_columns::is_done)
        .first::<bool>(conn)
        .optional()?
        .unwrap_or(false))
}

/// `mention`: `names` (user names, lower case) were mentioned in the
/// content of the item by `actor`.
pub fn on_mentions(
    conn: &mut PgConnection,
    item_id: Uuid,
    names: &[String],
    actor: Uuid,
) -> Result<(), DieselError> {
    if names.is_empty() {
        return Ok(());
    }
    let users: Vec<Recipient> = sql_query(
        "SELECT id AS user_id FROM public.users WHERE lower(user_name) = ANY($1) ORDER BY id",
    )
    .bind::<Array<Text>, _>(names)
    .load(conn)?;
    if users.is_empty() {
        return Ok(());
    }
    let Some(ItemRow {
        short_code: code,
        title,
    }) = item_row(conn, item_id)?
    else {
        return Ok(());
    };
    let text = format!(
        "{} wrote your name in {code}: {title}",
        name_of(conn, actor)?
    );
    let ids: Vec<Uuid> = users.into_iter().map(|u| u.user_id).collect();
    notify(
        conn,
        &ids,
        NotificationKind::Mention,
        Some((item_id, &code)),
        &text,
        actor,
    )?;
    Ok(())
}

#[derive(QueryableByName)]
struct ItemRow {
    #[diesel(sql_type = Text)]
    short_code: String,
    #[diesel(sql_type = Text)]
    title: String,
}

/// The code and the title of an item of any type, archived or not.
fn item_row(conn: &mut PgConnection, item_id: Uuid) -> Result<Option<ItemRow>, DieselError> {
    sql_query("SELECT short_code, title FROM entity_directory WHERE id = $1")
        .bind::<SqlUuid, _>(item_id)
        .get_result(conn)
        .optional()
}

#[derive(QueryableByName)]
struct P0Row {
    #[diesel(sql_type = Text)]
    short_code: String,
    #[diesel(sql_type = Text)]
    title: String,
    #[diesel(sql_type = Text)]
    board: String,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    team_id: Option<Uuid>,
    #[diesel(sql_type = Nullable<Text>)]
    priority: Option<String>,
}

fn p0_row(conn: &mut PgConnection, task_id: Uuid) -> Result<Option<P0Row>, DieselError> {
    sql_query(
        "SELECT t.short_code, t.title, b.name AS board, b.team_id, \
                (SELECT m.value FROM item_metadata m \
                   JOIN metadata_definitions d ON d.id = m.metadata_definition_id \
                  WHERE m.item_id = t.id AND d.slug = 'priority') AS priority \
           FROM tasks t JOIN boards b ON b.id = t.board_id \
          WHERE t.id = $1 AND t.deleted_at IS NULL AND t.task_type = 'bug'",
    )
    .bind::<SqlUuid, _>(task_id)
    .get_result(conn)
    .optional()
}

/// Whether the item is a live bug with the priority P0. Any other item is
/// not.
pub fn is_p0_bug(conn: &mut PgConnection, item_id: Uuid) -> Result<bool, DieselError> {
    Ok(p0_row(conn, item_id)?
        .and_then(|row| row.priority)
        .is_some_and(|p| is_p0_priority(&p)))
}

/// `p0_bug`: the item is a live P0 bug on its board now (it became P0, or
/// it moved to the board). Each member of the team of the board is told.
/// Nothing when the item is not a P0 bug, or the board has no team.
pub fn on_p0_bug(conn: &mut PgConnection, item_id: Uuid, actor: Uuid) -> Result<(), DieselError> {
    let Some(row) = p0_row(conn, item_id)? else {
        return Ok(());
    };
    if !row.priority.as_deref().is_some_and(is_p0_priority) {
        return Ok(());
    }
    let Some(team_id) = row.team_id else {
        return Ok(());
    };
    let members: Vec<Uuid> = {
        use crate::schema::team_members;
        team_members::table
            .filter(team_members::team_id.eq(team_id))
            .select(team_members::user_id)
            .load(conn)?
    };
    let text = format!(
        "{} is a critical bug on the board {}: {}",
        row.short_code, row.board, row.title
    );
    notify(
        conn,
        &members,
        NotificationKind::P0Bug,
        Some((item_id, &row.short_code)),
        &text,
        actor,
    )?;
    Ok(())
}

/// The newest `limit` unread notifications of `user`, and the count of all
/// the unread ones. One query, on the partial index of the unread rows.
pub fn unread(conn: &mut PgConnection, user: Uuid, limit: i64) -> Result<Unread, DieselError> {
    let rows: Vec<UnreadRow> = sql_query(format!(
        "{SELECT_NOTIFICATION}, (SELECT count(*) FROM notifications \
                                  WHERE user_id = $1 AND read_at IS NULL) AS total \
          WHERE n.user_id = $1 AND n.read_at IS NULL \
          ORDER BY n.created_at DESC, n.id DESC LIMIT $2"
    ))
    .bind::<SqlUuid, _>(user)
    .bind::<BigInt, _>(limit)
    .load(conn)?;
    let total = rows.first().map_or(0, |r| r.total);
    Ok(Unread {
        newest: rows.into_iter().map(|r| r.notification).collect(),
        total,
    })
}

/// The notifications of `user` for a list: each unread one (newest first,
/// at most `unread_limit`), then the newest `read_limit` read ones.
pub fn list(
    conn: &mut PgConnection,
    user: Uuid,
    unread_limit: i64,
    read_limit: i64,
) -> Result<Vec<Notification>, DieselError> {
    let mut out: Vec<Notification> = sql_query(format!(
        "{SELECT_NOTIFICATION} WHERE n.user_id = $1 AND n.read_at IS NULL \
          ORDER BY n.created_at DESC, n.id DESC LIMIT $2"
    ))
    .bind::<SqlUuid, _>(user)
    .bind::<BigInt, _>(unread_limit)
    .load(conn)?;
    let read: Vec<Notification> = sql_query(format!(
        "{SELECT_NOTIFICATION} WHERE n.user_id = $1 AND n.read_at IS NOT NULL \
          ORDER BY n.read_at DESC, n.id DESC LIMIT $2"
    ))
    .bind::<SqlUuid, _>(user)
    .bind::<BigInt, _>(read_limit)
    .load(conn)?;
    out.extend(read);
    Ok(out)
}

/// Mark notifications of `user` read: the ones in `ids`, or each unread
/// one when `ids` is `None`. An id of a different person, or of a read
/// notification, changes nothing. Returns the ids that changed, and sends
/// the event `notifications_read` to the person when one changed.
pub fn mark_read(
    conn: &mut PgConnection,
    user: Uuid,
    ids: Option<&[Uuid]>,
) -> Result<Vec<Uuid>, DieselError> {
    use crate::schema::notifications::dsl;
    let changed: Vec<Uuid> = match ids {
        Some(ids) => diesel::update(
            dsl::notifications
                .filter(dsl::user_id.eq(user))
                .filter(dsl::read_at.is_null())
                .filter(dsl::id.eq_any(ids)),
        )
        .set(dsl::read_at.eq(diesel::dsl::now))
        .returning(dsl::id)
        .get_results(conn)?,
        None => diesel::update(
            dsl::notifications
                .filter(dsl::user_id.eq(user))
                .filter(dsl::read_at.is_null()),
        )
        .set(dsl::read_at.eq(diesel::dsl::now))
        .returning(dsl::id)
        .get_results(conn)?,
    };
    if !changed.is_empty() {
        events::emit_notification_event(conn, events::NOTIFICATIONS_READ, user, "", user)?;
    }
    Ok(changed)
}

/// The settings of `user`: each kind, and whether it is on.
pub fn settings(
    conn: &mut PgConnection,
    user: Uuid,
) -> Result<BTreeMap<NotificationKind, bool>, DieselError> {
    use crate::schema::notification_settings::dsl;
    let stored: Vec<(String, bool)> = dsl::notification_settings
        .filter(dsl::user_id.eq(user))
        .select((dsl::kind, dsl::enabled))
        .load(conn)?;
    let mut out: BTreeMap<NotificationKind, bool> = NotificationKind::ALL
        .into_iter()
        .map(|k| (k, true))
        .collect();
    for (kind, enabled) in stored {
        if let Some(kind) = NotificationKind::parse(&kind) {
            out.insert(kind, enabled);
        }
    }
    Ok(out)
}

/// Turn kinds on or off for `user`. A kind that is not in `changes` does
/// not change.
pub fn set_settings(
    conn: &mut PgConnection,
    user: Uuid,
    changes: &[(NotificationKind, bool)],
) -> Result<BTreeMap<NotificationKind, bool>, DieselError> {
    use crate::schema::notification_settings::dsl;
    conn.transaction(|conn| {
        for (kind, enabled) in changes {
            diesel::insert_into(dsl::notification_settings)
                .values((
                    dsl::user_id.eq(user),
                    dsl::kind.eq(kind.as_str()),
                    dsl::enabled.eq(*enabled),
                ))
                .on_conflict((dsl::user_id, dsl::kind))
                .do_update()
                .set(dsl::enabled.eq(*enabled))
                .execute(conn)?;
        }
        settings(conn, user)
    })
}

/// Delete the notifications that were read before `cutoff` (the retention
/// pass). Returns the count.
pub fn delete_read_before(
    conn: &mut PgConnection,
    cutoff: DateTime<Utc>,
) -> Result<usize, DieselError> {
    use crate::schema::notifications::dsl;
    diesel::delete(dsl::notifications.filter(dsl::read_at.lt(cutoff))).execute(conn)
}

/// Delete the notifications about an item (a purge of the item).
pub(crate) fn delete_for_item(
    conn: &mut PgConnection,
    item_id: Uuid,
) -> Result<usize, DieselError> {
    use crate::schema::notifications::dsl;
    diesel::delete(dsl::notifications.filter(dsl::item_id.eq(item_id))).execute(conn)
}
