//! Promote a note into a draft ADR or document (KAIROS-T-0505,
//! COLLIERY-I-0629): the ONE implementation of
//! `POST /api/{family}/{short_code}/notes/{note_id}/promote` and of the MCP
//! tool `promote_note`.
//!
//! The new item gets the text of the note and a line that names the note,
//! its author and its date. The new item `supports` the source item when
//! the source is a strategy, an initiative or a task (the type rule of a
//! `supports` edge, [`kairos_core::graph::check_link`]); a note on a
//! document or an ADR gets no edge. The note gets a trailing line
//! `Promoted to KAIROS-A-0029.`
//!
//! Who may: the edit rule of the source item (the trailing line is an
//! edit), and the right to create on the target board (`manage_adrs`, or
//! `manage_documents` through [`crate::api::documents::create`]). Each
//! check runs before the first write; the caller runs this in one
//! transaction ([`crate::api::atomically`]).

use diesel::pg::PgConnection;
use kairos_core::short_code::ItemType;
use kairos_db::items::{self, ContentUpdate};
use kairos_db::models::RelationshipType;
use uuid::Uuid;

use crate::api::documents::{self, CreateWording, NewDocument};
use crate::api::{
    board_id_by_ref, map_graph_error, map_item_error, require_capability, require_edge_write,
    require_item_edit,
};
use crate::error::ApiError;

/// The kinds that a note can become.
pub const KINDS: [&str; 2] = ["adr", "document"];

/// The inputs of a promote, from each surface.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PromoteInput<'a> {
    /// The source item: its id, type and short code.
    pub source: (Uuid, ItemType, &'a str),
    /// The id of the note: `n-3`.
    pub note_id: &'a str,
    /// `adr` or `document`.
    pub kind: &'a str,
    /// The target board, by slug or UUID.
    pub board: &'a str,
    /// The title of the new item; the first line of the note when absent.
    pub title: Option<&'a str>,
}

/// What a promote made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Promoted {
    pub source_code: String,
    pub note_id: String,
    pub kind: ItemType,
    pub created_code: String,
    pub created_id: Uuid,
    /// True when the new item `supports` the source item.
    pub supports: bool,
}

/// The kind as an item type, or the refusal that names the kinds.
pub(crate) fn parse_kind(kind: &str, wording: CreateWording) -> Result<ItemType, ApiError> {
    match kind.trim() {
        "adr" => Ok(ItemType::Adr),
        "document" => Ok(ItemType::Document),
        other => Err(ApiError::validation(format!(
            "The {} has the kind {other:?}. A note becomes an adr or a document. Send kind: \
             adr or document.",
            wording.request
        ))
        .with_details(serde_json::json!({ wording.details_key: "kind", "allowed": KINDS }))),
    }
}

/// Promote the note: the new item, its edge, and the line on the note.
pub(crate) fn promote_note(
    conn: &mut PgConnection,
    slug: &str,
    role: kairos_db::models::OrgRole,
    user: Uuid,
    input: PromoteInput<'_>,
    wording: CreateWording,
) -> Result<Promoted, ApiError> {
    let (source_id, source_type, source_code) = input.source;
    let kind = parse_kind(input.kind, wording)?;
    if input.board.trim().is_empty() {
        return Err(ApiError::validation(format!(
            "The {} has no {}. Send the slug or the id of the board that gets the new item.",
            wording.request, wording.board
        ))
        .with_details(serde_json::json!({ wording.details_key: "board" })));
    }
    // The trailing line on the note is an edit of the source item.
    require_item_edit(conn, role, user, source_id, source_type)?;
    let (version, content) =
        items::lock_item_content(conn, source_type, source_id).map_err(map_item_error)?;
    let notes = kairos_core::items::notes(&content);
    let Some(note) = notes.iter().find(|note| note.id == input.note_id) else {
        let ids: Vec<&str> = notes.iter().map(|note| note.id.as_str()).collect();
        return Err(ApiError::not_found(if ids.is_empty() {
            format!("The item {source_code} has no note.")
        } else {
            format!(
                "The item {source_code} has no note {:?}. Its notes: {}.",
                input.note_id,
                ids.join(", ")
            )
        }));
    };
    let title = match input.title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => title.to_string(),
        None if !note.first_line.is_empty() => clip(&note.first_line, 120),
        None => format!("Note {} of {source_code}", note.id),
    };
    let from = format!(
        "From note {} on {source_code} ({}, {}).",
        note.id, note.author, note.date
    );
    let new_content = if note.text.is_empty() {
        format!("{from}\n")
    } else {
        format!("{}\n\n{from}\n", note.text)
    };
    let supports = matches!(
        source_type,
        ItemType::Strategy | ItemType::Initiative | ItemType::Task
    );

    let (created_code, created_id) = match kind {
        ItemType::Adr => {
            let board_id = board_id_by_ref(conn, input.board)?;
            require_capability(conn, slug, Some(board_id), user, "manage_adrs")?;
            let created = items::create_adr(
                conn,
                items::CreateAdr {
                    board_id: Some(board_id),
                    column_id: None,
                    title: &title,
                    content: &new_content,
                    decision_maker: None,
                    decision_date: None,
                },
                user,
            )
            .map_err(map_item_error)?;
            if supports {
                require_edge_write(
                    conn,
                    role,
                    user,
                    RelationshipType::Supports.as_str(),
                    (source_id, source_type),
                    (created.id, ItemType::Adr),
                )?;
                kairos_db::graph::link_items(
                    conn,
                    source_id,
                    created.id,
                    RelationshipType::Supports,
                    user,
                )
                .map_err(map_graph_error)?;
            }
            (created.short_code, created.id)
        }
        ItemType::Document => {
            let created = documents::create(
                conn,
                slug,
                role,
                user,
                NewDocument {
                    title: &title,
                    content: Some(&new_content),
                    template_id: None,
                    board: Some(input.board),
                    parent: supports.then_some(source_code),
                },
                wording,
            )?;
            (created.document.short_code, created.document.id)
        }
        _ => unreachable!("parse_kind gives an adr or a document"),
    };

    let line = format!("Promoted to {created_code}.");
    let with_line = kairos_core::items::content_with_note_line(&content, &note.id, &line)
        .ok_or_else(|| ApiError::internal("the note was found a moment ago"))?;
    items::update_item_content(
        conn,
        source_type,
        source_id,
        ContentUpdate {
            new_title: None,
            new_content: &with_line,
            expected_version: version,
        },
        user,
    )
    .map_err(map_item_error)?;

    Ok(Promoted {
        source_code: source_code.to_string(),
        note_id: note.id.clone(),
        kind,
        created_code,
        created_id,
        supports,
    })
}

/// `text` cut to `max` characters, with `…` when it was longer.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}
