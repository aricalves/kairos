//! Thin change-event emission over PostgreSQL `NOTIFY` (KAIROS-T-0022,
//! contract per KAIROS-A-0005 §5 / KAIROS-S-0005 "Event Push").
//!
//! Every mutating service (create/update/transition/delete, relationship
//! and metadata changes) emits one `NOTIFY kairos_events` carrying the
//! tenant-tagged thin payload:
//!
//! ```json
//! {"tenant": "acme", "event": "item_transitioned", "entity_type": "task",
//!  "short_code": "ACME-T-0123", "board_id": "uuid", "column_id": "uuid",
//!  "actor": "user-uuid", "occurred_at": "2026-07-10T12:00:00Z"}
//! ```
//!
//! `agent_key_id` is there only when an agent made the change with the
//! agent key of a person (KAIROS-T-0359): `actor` is the person, and
//! `agent_key_id` is the key ([`crate::agent_mark`]).
//!
//! The `tenant` field is fan-out routing metadata for the server's LISTEN
//! side (`kairos-server/src/ws.rs`) and is stripped before the event
//! reaches a client socket; the remaining fields are exactly the S-0005
//! client shape (`column_id` omitted when not applicable, `board_id`
//! `null` for off-board items — documents and unplaced ADRs).
//!
//! # Boards across a `blocks` edge (COLLIERY-T-0233)
//!
//! A card shows the count of its open blockers
//! ([`crate::graph::blocks_summary`]), and a blocker can be on a different
//! board. When the blocker moves to a done column, the count on the card
//! changes, but the event carries the board of the BLOCKER: a socket that
//! filters to the board of the card did not get it, and the count stayed
//! stale. So an event that can change a count also carries `blocks_boards`:
//! the boards of the live items at the other end of the item's `blocks`
//! edges, in the two directions ([`blocks_boards`]). Like `tenant`, this
//! field is routing metadata: the server delivers the event to a socket
//! filtered to one of these boards, and strips the field before the event
//! reaches a client. The client shape does not change.
//!
//! Only the kinds in [`EventKind::changes_blocks_counts`] carry it. The
//! other kinds cannot change a count, and a `relationship_changed` is
//! emitted for each end of the edge with its own board.
//!
//! # The events of a person (KAIROS-T-0381)
//!
//! A notification is for ONE person. [`emit_notification_event`] sends
//! `notification_created` (a new notification) or `notifications_read`
//! (the person read some) with the routing field `recipient`: the server
//! delivers the event only to a socket of that person, and strips the field
//! as it strips `tenant`. The event has the S-0005 shape: `entity_type` is
//! `notification`, `short_code` is the code of the item of the notification
//! (empty for `notifications_read`), and `board_id` is `null`, so a socket
//! filtered to a board does not get it. The text of a notification is not
//! in the event: the client reads it through the REST API.
//!
//! # The precise kinds (KAIROS-T-0574)
//!
//! `task_state_changed`, `task_cancelled` and `claim_changed` come IN
//! ADDITION to the coarse kind of the same write: `item_updated` for a
//! write of the state, a claim, a hand-off and a release, and
//! `item_transitioned` for a cancel and a move. A subscriber that knows
//! only the coarse kinds keeps working. A subscriber that wants to know
//! what changed reads the precise kind. Both come in the same transaction.
//!
//! KAIROS-T-0630: `note_appended` and `note_promoted` come after the
//! `item_updated` of a note, with the id of the note as `change`. A note
//! writes no version of the content, so a client that shows the notes
//! reads the item again for them.
//!
//! `claim_changed` and `board_configured` carry the field `change`
//! ([`emit_event_with_change`]): what changed. For `claim_changed` it is
//! a [`ClaimChange`]; for `board_configured` it is the setting
//! (`column_add`, `transition_remove`, `board_settings`, ...).
//! `board_configured` has `entity_type` `board`, the slug of the board as
//! `short_code`, and the board as `board_id` ([`emit_board_configured`]).
//!
//! # Post-commit semantics (A-0005 §5)
//!
//! [`emit_event`] runs INSIDE the service transaction, and that is the
//! point: PostgreSQL queues `pg_notify` payloads with the transaction and
//! delivers them to listeners only when the transaction COMMITS. A rolled
//! back write therefore emits nothing, and a delivered notification always
//! describes committed state — A-0005's "emitted post-commit" by
//! construction, with no application-side after-commit hook to forget.
//!
//! # Delivery is best-effort
//!
//! Events are UI-freshness hints, not a durable stream (A-0005 §5): no
//! replay, no ordering guarantee beyond what one LISTEN connection
//! observes. Clients reconcile by re-fetching through the REST API.
//!
//! # Tenant tagging
//!
//! The services run on connections whose `search_path` is pinned to the
//! tenant schema and never receive the slug as an argument, so the tenant
//! tag is derived from `current_schema()` (`org_{slug}` → `{slug}`) — the
//! same derivation [`crate::items::tenant_code_prefix`] uses for the
//! prefix of an item with no board.

use chrono::Utc;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{Nullable, Text, Uuid as SqlUuid};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The one NOTIFY channel every Kairos event travels on (A-0005 §5).
pub const EVENT_CHANNEL: &str = "kairos_events";

/// The S-0005 event vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// An item was created.
    ItemCreated,
    /// An item's content changed (edit or rollback).
    ItemUpdated,
    /// An item moved to another board column.
    ItemTransitioned,
    /// A task moved to another delivery board (KAIROS-I-0012). Emitted
    /// once for the board it left (`board_id` = source, no column) and
    /// once for the board it joined, so both boards' subscribers refetch.
    ItemMoved,
    /// An item was soft-deleted.
    ItemDeleted,
    /// An archived item was put back (KAIROS-A-0020). Distinct from
    /// `ItemCreated`: the item and its history existed all along, so a
    /// client that treats this as a create would show a new card with an
    /// old version number.
    ItemRestored,
    /// A relationship edge touching the item was added or removed.
    RelationshipChanged,
    /// The item's metadata values changed.
    MetadataChanged,
    /// The item's forge links (branches, pull/merge requests) changed
    /// (KAIROS-T-0099).
    ItemLinksChanged,
    /// A run of the code index builder started or ended for a repository
    /// (KAIROS-T-0333). `entity_type` is `repository`, `short_code` is the
    /// slug of the repository, and there is no board. A reader of the
    /// repository reads its runs again.
    CodeIndexBuildChanged,
    /// The live state of a task changed (KAIROS-T-0574): a write of the
    /// state, or the clear of the next step when the claim changes. Comes
    /// after the `item_updated` of the same write.
    TaskStateChanged,
    /// A task was cancelled (KAIROS-T-0574). `column_id` is the done column.
    /// Comes after the `item_transitioned` of the cancel.
    TaskCancelled,
    /// The claim of a task changed (KAIROS-T-0574). `change` is a
    /// [`ClaimChange`].
    ClaimChanged,
    /// The configuration of a board changed (KAIROS-T-0574): its columns,
    /// its transitions, a flag or a setting. `entity_type` is `board`,
    /// `short_code` is the slug of the board, `change` names the setting.
    BoardConfigured,
    /// A note was added to the item (KAIROS-T-0630). `change` is the id of
    /// the note (`n-3`). Comes after the `item_updated` of the same write.
    NoteAppended,
    /// A note of the item became a draft ADR or document (KAIROS-T-0630).
    /// `change` is the id of the note. Comes after the `item_updated` of
    /// the same write.
    NotePromoted,
}

/// What happened to a claim: the `change` of a `claim_changed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimChange {
    /// A person took the claim: a claim, or a move into Active.
    Taken,
    /// A hand-off gave the claim to a person.
    HandedOff,
    /// The holder or a manager released the claim. The task stays.
    Released,
    /// The claim ended for a different reason: a move out of Active, a
    /// cancel, an archive, a column flag, a person who left.
    Ended,
}

impl ClaimChange {
    /// The wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimChange::Taken => "taken",
            ClaimChange::HandedOff => "handed_off",
            ClaimChange::Released => "released",
            ClaimChange::Ended => "ended",
        }
    }
}

impl EventKind {
    /// Whether this kind can change the `blocks` counts on the cards of
    /// OTHER items (COLLIERY-T-0233): the item changed column (a done
    /// column opens or closes the edge, COLLIERY-T-0214), or changed
    /// liveness (archived work does not count). A content, metadata or
    /// link change does not. An edge change does, but
    /// `relationship_changed` is emitted once for each end, so each board
    /// gets the event of its own item.
    pub fn changes_blocks_counts(self) -> bool {
        matches!(
            self,
            EventKind::ItemTransitioned
                | EventKind::ItemMoved
                | EventKind::ItemDeleted
                | EventKind::ItemRestored
        )
    }

    /// The wire name (`event` field).
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::ItemCreated => "item_created",
            EventKind::ItemUpdated => "item_updated",
            EventKind::ItemTransitioned => "item_transitioned",
            EventKind::ItemMoved => "item_moved",
            EventKind::ItemDeleted => "item_deleted",
            EventKind::ItemRestored => "item_restored",
            EventKind::RelationshipChanged => "relationship_changed",
            EventKind::MetadataChanged => "metadata_changed",
            EventKind::ItemLinksChanged => "item_links_changed",
            EventKind::CodeIndexBuildChanged => "code_index_build_changed",
            EventKind::TaskStateChanged => "task_state_changed",
            EventKind::TaskCancelled => "task_cancelled",
            EventKind::ClaimChanged => "claim_changed",
            EventKind::BoardConfigured => "board_configured",
            EventKind::NoteAppended => "note_appended",
            EventKind::NotePromoted => "note_promoted",
        }
    }
}

/// One thin change event (S-0005 shape, before tenant tagging).
#[derive(Debug, Clone)]
pub struct ThinEvent {
    /// What happened.
    pub event: EventKind,
    /// `strategy|initiative|task|document|adr`.
    pub entity_type: String,
    /// The affected item's short code.
    pub short_code: String,
    /// The item's board (`None` for off-board items).
    pub board_id: Option<Uuid>,
    /// The item's (new) column, where applicable.
    pub column_id: Option<Uuid>,
    /// The acting user (`public.users.id`).
    pub actor: Uuid,
}

#[derive(QueryableByName)]
struct SchemaName {
    #[diesel(sql_type = Text)]
    name: String,
    /// The agent key of the request (KAIROS-T-0359, [`crate::agent_mark`]).
    #[diesel(sql_type = Nullable<SqlUuid>)]
    agent_key: Option<Uuid>,
}

#[derive(QueryableByName)]
struct PlacementRow {
    #[diesel(sql_type = Text)]
    short_code: String,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    board_id: Option<Uuid>,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    column_id: Option<Uuid>,
}

#[derive(QueryableByName)]
struct BoardRow {
    #[diesel(sql_type = SqlUuid)]
    board_id: Uuid,
}

/// The boards of the live items at the other end of the `blocks` edges of
/// the item `short_code`, in the two directions (COLLIERY-T-0233, module
/// docs). `own_board` is left out: the event already goes there. The item
/// itself can be archived (an `item_deleted` event); the other end must be
/// live, because an archived item has no card to refresh. Ordered, so the
/// payload is deterministic.
pub fn blocks_boards(
    conn: &mut PgConnection,
    short_code: &str,
    own_board: Option<Uuid>,
) -> Result<Vec<Uuid>, DieselError> {
    // KAIROS-T-0628: one UNION ALL arm per direction, so each arm probes its
    // relationship index (source or target); the CHECK source_id != target_id
    // keeps the arms disjoint, so no edge is counted twice.
    let rows: Vec<BoardRow> = sql_query(
        "SELECT DISTINCT other.board_id \
         FROM entity_directory me \
         CROSS JOIN LATERAL ( \
             SELECT r.target_id AS other_id FROM item_relationships r \
                 WHERE r.source_id = me.id AND r.relationship = 'blocks' \
             UNION ALL \
             SELECT r.source_id FROM item_relationships r \
                 WHERE r.target_id = me.id AND r.relationship = 'blocks' \
         ) n \
         JOIN entity_directory other \
           ON other.id = n.other_id \
          AND other.deleted_at IS NULL \
         WHERE me.short_code = $1 \
           AND other.board_id IS NOT NULL \
           AND other.board_id IS DISTINCT FROM $2 \
         ORDER BY other.board_id",
    )
    .bind::<Text, _>(short_code)
    .bind::<Nullable<SqlUuid>, _>(own_board)
    .load(conn)?;
    Ok(rows.into_iter().map(|row| row.board_id).collect())
}

/// Emit `event` on [`EVENT_CHANNEL`], tagged with the current connection's
/// tenant (see module docs). Call INSIDE the mutating transaction —
/// PostgreSQL delivers the notification on commit and drops it on
/// rollback, which is what makes emission post-commit (A-0005 §5).
pub fn emit_event(conn: &mut PgConnection, event: &ThinEvent) -> Result<(), DieselError> {
    emit_event_with_change(conn, event, None)
}

/// [`emit_event`] with the field `change` (module docs, "The precise
/// kinds"). `None` omits the field.
pub fn emit_event_with_change(
    conn: &mut PgConnection,
    event: &ThinEvent,
    change: Option<&str>,
) -> Result<(), DieselError> {
    let schema: SchemaName = sql_query(
        "SELECT COALESCE(current_schema()::text, 'public') AS name, \
         NULLIF(current_setting('kairos.agent_key', true), '')::uuid AS agent_key",
    )
    .get_result(conn)?;
    let tenant = schema
        .name
        .strip_prefix("org_")
        .unwrap_or(schema.name.as_str());

    let mut payload = Map::new();
    payload.insert("tenant".into(), json!(tenant));
    payload.insert("event".into(), json!(event.event.as_str()));
    payload.insert("entity_type".into(), json!(event.entity_type));
    payload.insert("short_code".into(), json!(event.short_code));
    payload.insert("board_id".into(), json!(event.board_id));
    if let Some(column_id) = event.column_id {
        payload.insert("column_id".into(), json!(column_id));
    }
    if let Some(change) = change {
        payload.insert("change".into(), json!(change));
    }
    payload.insert("actor".into(), json!(event.actor));
    // KAIROS-T-0359: the agent key, when an agent made the change. Omitted
    // otherwise.
    if let Some(agent_key) = schema.agent_key {
        payload.insert("agent_key_id".into(), json!(agent_key));
    }
    payload.insert("occurred_at".into(), json!(Utc::now()));
    // COLLIERY-T-0233: routing metadata, stripped by the server like
    // `tenant`. Omitted when empty, which is the usual case.
    if event.event.changes_blocks_counts() {
        let boards = blocks_boards(conn, &event.short_code, event.board_id)?;
        if !boards.is_empty() {
            payload.insert("blocks_boards".into(), json!(boards));
        }
    }

    sql_query("SELECT pg_notify($1, $2)")
        .bind::<Text, _>(EVENT_CHANNEL)
        .bind::<Text, _>(Value::Object(payload).to_string())
        .execute(conn)?;
    Ok(())
}

/// The wire name of the event of a new notification (KAIROS-T-0381).
pub const NOTIFICATION_CREATED: &str = "notification_created";
/// The wire name of the event of notifications that the person read.
pub const NOTIFICATIONS_READ: &str = "notifications_read";

/// Emit an event for ONE person (module docs, "The events of a person"):
/// `event` is [`NOTIFICATION_CREATED`] or [`NOTIFICATIONS_READ`]. Call it in
/// the transaction of the write, as [`emit_event`].
pub fn emit_notification_event(
    conn: &mut PgConnection,
    event: &str,
    recipient: Uuid,
    short_code: &str,
    actor: Uuid,
) -> Result<(), DieselError> {
    let schema: SchemaName = sql_query(
        "SELECT COALESCE(current_schema()::text, 'public') AS name, \
         NULLIF(current_setting('kairos.agent_key', true), '')::uuid AS agent_key",
    )
    .get_result(conn)?;
    let tenant = schema
        .name
        .strip_prefix("org_")
        .unwrap_or(schema.name.as_str());
    let payload = json!({
        "tenant": tenant,
        "recipient": recipient,
        "event": event,
        "entity_type": "notification",
        "short_code": short_code,
        "board_id": Value::Null,
        "actor": actor,
        "occurred_at": Utc::now(),
    });
    sql_query("SELECT pg_notify($1, $2)")
        .bind::<Text, _>(EVENT_CHANNEL)
        .bind::<Text, _>(payload.to_string())
        .execute(conn)?;
    Ok(())
}

/// [`item_placement`]'s row: `(short_code, board_id, column_id)`.
pub type ItemPlacement = (String, Option<Uuid>, Option<Uuid>);

/// `(short_code, board_id, column_id)` of an item by id, straight from its
/// entity table (soft-deleted rows included — `item_deleted` events need
/// the placement of the row that was just stamped). `None` for unknown
/// ids or unknown entity types.
pub fn item_placement(
    conn: &mut PgConnection,
    entity_type: &str,
    item_id: Uuid,
) -> Result<Option<ItemPlacement>, DieselError> {
    let sql = match entity_type {
        "strategy" => "SELECT short_code, board_id, column_id FROM strategies WHERE id = $1",
        "initiative" => "SELECT short_code, board_id, column_id FROM initiatives WHERE id = $1",
        "task" => "SELECT short_code, board_id, column_id FROM tasks WHERE id = $1",
        "document" => {
            "SELECT short_code, NULL::uuid AS board_id, NULL::uuid AS column_id \
             FROM documents WHERE id = $1"
        }
        "adr" => "SELECT short_code, board_id, column_id FROM adrs WHERE id = $1",
        _ => return Ok(None),
    };
    let row: Option<PlacementRow> = sql_query(sql)
        .bind::<SqlUuid, _>(item_id)
        .get_result(conn)
        .optional()?;
    Ok(row.map(|r| (r.short_code, r.board_id, r.column_id)))
}

/// Convenience for call sites that only hold an item id: look up the
/// item's short code and placement, then [`emit_event`]. Unknown ids emit
/// nothing (the mutation itself already failed or the row is gone).
pub fn emit_item_event_by_id(
    conn: &mut PgConnection,
    event: EventKind,
    entity_type: &str,
    item_id: Uuid,
    actor: Uuid,
) -> Result<(), DieselError> {
    emit_item_change_by_id(conn, event, entity_type, item_id, actor, None)
}

/// [`emit_item_event_by_id`] with the field `change` (KAIROS-T-0574).
pub fn emit_item_change_by_id(
    conn: &mut PgConnection,
    event: EventKind,
    entity_type: &str,
    item_id: Uuid,
    actor: Uuid,
    change: Option<&str>,
) -> Result<(), DieselError> {
    if let Some((short_code, board_id, column_id)) = item_placement(conn, entity_type, item_id)? {
        emit_event_with_change(
            conn,
            &ThinEvent {
                event,
                entity_type: entity_type.to_string(),
                short_code,
                board_id,
                column_id,
                actor,
            },
            change,
        )?;
    }
    Ok(())
}

/// Emit `claim_changed` for the task `task_id` (KAIROS-T-0574).
pub fn emit_claim_changed(
    conn: &mut PgConnection,
    task_id: Uuid,
    change: ClaimChange,
    actor: Uuid,
) -> Result<(), DieselError> {
    emit_item_change_by_id(
        conn,
        EventKind::ClaimChanged,
        "task",
        task_id,
        actor,
        Some(change.as_str()),
    )
}

#[derive(QueryableByName)]
struct DirectoryRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = Text)]
    entity_type: String,
}

/// [`emit_item_event_by_id`] for a call site that holds a short code
/// (the descendants of an archive cascade, KAIROS-T-0574). An unknown
/// code emits nothing.
pub fn emit_item_event_by_code(
    conn: &mut PgConnection,
    event: EventKind,
    short_code: &str,
    actor: Uuid,
) -> Result<(), DieselError> {
    let row: Option<DirectoryRow> =
        sql_query("SELECT id, entity_type FROM entity_directory WHERE short_code = $1")
            .bind::<Text, _>(short_code)
            .get_result(conn)
            .optional()?;
    match row {
        Some(row) => emit_item_event_by_id(conn, event, &row.entity_type, row.id, actor),
        None => Ok(()),
    }
}

#[derive(QueryableByName)]
struct BoardSlugRow {
    #[diesel(sql_type = Text)]
    slug: String,
}

/// Emit `board_configured` for the board `board_id` (KAIROS-T-0574):
/// `entity_type` `board`, the slug of the board as `short_code`, and
/// `change` the setting that changed. Call it in the transaction of the
/// write. An unknown board emits nothing.
pub fn emit_board_configured(
    conn: &mut PgConnection,
    board_id: Uuid,
    actor: Uuid,
    change: &str,
) -> Result<(), DieselError> {
    let row: Option<BoardSlugRow> = sql_query("SELECT slug FROM boards WHERE id = $1")
        .bind::<SqlUuid, _>(board_id)
        .get_result(conn)
        .optional()?;
    let Some(row) = row else {
        return Ok(());
    };
    emit_event_with_change(
        conn,
        &ThinEvent {
            event: EventKind::BoardConfigured,
            entity_type: "board".to_string(),
            short_code: row.slug,
            board_id: Some(board_id),
            column_id: None,
            actor,
        },
        Some(change),
    )
}

/// The setting of a board configuration record: the text before the
/// first `:` of the `board_config` details (`column_add:Review@3` gives
/// `column_add`).
pub fn board_config_change(details: &str) -> &str {
    details.split(':').next().unwrap_or(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_kinds_have_their_wire_names() {
        assert_eq!(EventKind::TaskStateChanged.as_str(), "task_state_changed");
        assert_eq!(EventKind::TaskCancelled.as_str(), "task_cancelled");
        assert_eq!(EventKind::ClaimChanged.as_str(), "claim_changed");
        assert_eq!(EventKind::BoardConfigured.as_str(), "board_configured");
        assert_eq!(EventKind::NoteAppended.as_str(), "note_appended");
        assert_eq!(EventKind::NotePromoted.as_str(), "note_promoted");
        let changes: Vec<&str> = [
            ClaimChange::Taken,
            ClaimChange::HandedOff,
            ClaimChange::Released,
            ClaimChange::Ended,
        ]
        .into_iter()
        .map(ClaimChange::as_str)
        .collect();
        assert_eq!(changes, ["taken", "handed_off", "released", "ended"]);
    }

    #[test]
    fn the_new_kinds_change_no_blocks_count() {
        for kind in [
            EventKind::TaskStateChanged,
            EventKind::TaskCancelled,
            EventKind::ClaimChanged,
            EventKind::BoardConfigured,
            EventKind::NoteAppended,
            EventKind::NotePromoted,
        ] {
            assert!(!kind.changes_blocks_counts(), "{}", kind.as_str());
        }
    }

    #[test]
    fn the_change_of_a_board_record_is_its_setting() {
        assert_eq!(board_config_change("column_add:Review@3"), "column_add");
        assert_eq!(
            board_config_change("transition_remove:Todo->Active"),
            "transition_remove"
        );
        assert_eq!(board_config_change("board_settings"), "board_settings");
    }
}
