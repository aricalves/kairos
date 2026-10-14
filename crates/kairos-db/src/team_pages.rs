//! Team-page write services (KAIROS-T-0082, design in KAIROS-I-0007).
//!
//! A new team gets one page: its charter, the landing page and social
//! contract of the team. The team grows the rest of its tree itself; there
//! is no seeded folder set (KAIROS-T-0436, decided in KAIROS-T-0435). The
//! migration `2026-10-14-000300_team_scaffold_removed` removed the unedited
//! seeded folders from the teams that had them.

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use uuid::Uuid;

use crate::models::enums::TeamPageKind;
use crate::models::team_pages::{NewTeamPage, NewTeamPageHistory, TeamPage, TeamPageHistory};

/// The charter's templated skeleton (mirrors the `team_charter` system
/// template's headings).
const CHARTER_CONTENT: &str =
    "# Team Charter\n\n## Mission\n\n## Scope\n\n## Ways of Working\n\n## Success Measures\n";

/// Give a team its charter (idempotent): the protected page `charter` at
/// the top of the tree, with a v1 history baseline. Runs in the caller's
/// transaction: team creation calls this beside the delivery-board
/// creation, so a team is never born without its landing page. The charter
/// cannot be deleted or moved, so an existing one is always live.
pub fn seed_team_charter(
    conn: &mut PgConnection,
    team_id: Uuid,
    actor: Uuid,
) -> Result<(), DieselError> {
    use crate::schema::team_pages::dsl;
    let existing: Option<Uuid> = dsl::team_pages
        .filter(dsl::team_id.eq(team_id))
        .filter(dsl::parent_id.is_null())
        .filter(dsl::slug.eq("charter"))
        .filter(dsl::deleted_at.is_null())
        .select(dsl::id)
        .first(conn)
        .optional()?;
    if existing.is_some() {
        return Ok(());
    }
    let created: TeamPage = diesel::insert_into(dsl::team_pages)
        .values(NewTeamPage {
            team_id,
            parent_id: None,
            kind: TeamPageKind::Page,
            slug: "charter".to_string(),
            title: "Team Charter".to_string(),
            content: CHARTER_CONTENT.to_string(),
            position: 0,
            is_protected: true,
            created_by: actor,
            updated_by: actor,
        })
        .returning(TeamPage::as_returning())
        .get_result(conn)?;
    diesel::insert_into(crate::schema::team_page_history::table)
        .values(NewTeamPageHistory {
            page_id: created.id,
            version: 1,
            title: created.title.clone(),
            content: created.content.clone(),
            edited_by: actor,
        })
        .execute(conn)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Page write services (KAIROS-T-0083)
// ---------------------------------------------------------------------------

use crate::models::team_pages::{TeamAnnouncement, TeamPageChangeset};

/// Content bytes cap for team pages and announcements (KAIROS-I-0007
/// decision: the tenant's first server-side content cap).
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

/// Typed rejections of the team-page write paths.
#[derive(Debug, thiserror::Error)]
pub enum TeamPageError {
    /// No live page with this id exists on this team.
    #[error("The team page {0} does not exist.")]
    PageNotFound(Uuid),
    /// A sibling with this slug already exists.
    #[error("A page in the same folder has the slug {0:?} already.")]
    SlugConflict(String),
    /// The slug does not have the form of a slug (KAIROS-T-0467): a `/`
    /// in it broke the path of the page.
    #[error(
        "{0:?} is not a slug. A slug is 2 to 63 characters: lowercase letters, digits, hyphens and underscores, with a letter first."
    )]
    BadSlug(String),
    /// A page or folder at the top of the tree cannot have a slug that the
    /// docs sites use as a path segment (KAIROS-T-0403).
    #[error(
        "A page or folder at the top of the tree cannot have the slug {0:?}. The docs sites use docs, adrs and repos in their paths. Use a different slug, or put the page in a folder."
    )]
    ReservedSlug(String),
    /// The referenced parent is missing, deleted, on another team, or not
    /// a folder.
    #[error("The parent {0} is not a live folder of this team.")]
    BadParent(Uuid),
    /// The page is protected (the Charter): rename/move/delete refused.
    #[error("The page {0} is the Charter. You cannot change its slug, move it, or delete it.")]
    ProtectedPage(Uuid),
    /// The item is not a document or an ADR that a board of the team owns
    /// (KAIROS-T-0437).
    #[error("The item {0} is not a document or an ADR of this team.")]
    NotTeamItem(Uuid),
    /// A restore of a page that is not archived (KAIROS-T-0439).
    #[error("The team page {0} is not archived. You can restore only an archived page.")]
    NotArchived(Uuid),
    /// KAIROS-A-0004-style optimistic-concurrency conflict on content.
    #[error(
        "The request has the version {expected_version} of the page {page_id}, and the \
         current version is {current_version}. Make the edit on the current version."
    )]
    VersionConflict {
        page_id: Uuid,
        expected_version: i32,
        current_version: i32,
        current_title: String,
        current_content: String,
    },
    /// A folder has no content (KAIROS-T-0441): a save may change its
    /// title, not its content.
    #[error("The node {0} is a folder. A folder has a title and no content.")]
    FolderContent(Uuid),
    /// Content exceeds [`MAX_CONTENT_BYTES`].
    #[error("The content has {actual} bytes. The limit is {MAX_CONTENT_BYTES} bytes.")]
    ContentTooLarge { actual: usize },
    #[error("The database gave an error: {0}.")]
    Database(#[from] DieselError),
}

/// Load a live page of a team, or the typed not-found.
pub fn load_page(
    conn: &mut PgConnection,
    team_id: Uuid,
    page_id: Uuid,
) -> Result<TeamPage, TeamPageError> {
    use crate::schema::team_pages::dsl;
    dsl::team_pages
        .filter(dsl::id.eq(page_id))
        .filter(dsl::team_id.eq(team_id))
        .filter(dsl::deleted_at.is_null())
        .select(TeamPage::as_select())
        .first(conn)
        .optional()?
        .ok_or(TeamPageError::PageNotFound(page_id))
}

/// A team's live page tree, parents-with-position order (flat list; the
/// client nests by parent_id).
pub fn list_pages(conn: &mut PgConnection, team_id: Uuid) -> Result<Vec<TeamPage>, DieselError> {
    use crate::schema::team_pages::dsl;
    dsl::team_pages
        .filter(dsl::team_id.eq(team_id))
        .filter(dsl::deleted_at.is_null())
        .order((dsl::position.asc(), dsl::title.asc()))
        .select(TeamPage::as_select())
        .load(conn)
}

/// Validate a prospective parent: a live folder of the same team.
fn check_parent(
    conn: &mut PgConnection,
    team_id: Uuid,
    parent_id: Uuid,
) -> Result<(), TeamPageError> {
    let parent =
        load_page(conn, team_id, parent_id).map_err(|_| TeamPageError::BadParent(parent_id))?;
    if parent.kind != TeamPageKind::Folder {
        return Err(TeamPageError::BadParent(parent_id));
    }
    Ok(())
}

fn check_size(content: &str) -> Result<(), TeamPageError> {
    if content.len() > MAX_CONTENT_BYTES {
        return Err(TeamPageError::ContentTooLarge {
            actual: content.len(),
        });
    }
    Ok(())
}

/// Input of [`create_page`].
#[derive(Debug, Clone)]
pub struct CreatePage<'a> {
    pub parent_id: Option<Uuid>,
    pub kind: TeamPageKind,
    pub slug: &'a str,
    pub title: &'a str,
    pub content: &'a str,
    pub position: i32,
}

/// Create a page or folder; pages get the v1 history baseline. One
/// transaction.
pub fn create_page(
    conn: &mut PgConnection,
    team_id: Uuid,
    input: CreatePage<'_>,
    actor: Uuid,
) -> Result<TeamPage, TeamPageError> {
    conn.transaction::<_, TeamPageError, _>(|conn| {
        check_size(input.content)?;
        if !kairos_core::slug::is_valid_slug(input.slug) {
            return Err(TeamPageError::BadSlug(input.slug.to_string()));
        }
        if input.parent_id.is_none() && kairos_core::slug::is_site_reserved(input.slug) {
            return Err(TeamPageError::ReservedSlug(input.slug.to_string()));
        }
        if let Some(parent) = input.parent_id {
            check_parent(conn, team_id, parent)?;
        }
        let created: TeamPage = diesel::insert_into(crate::schema::team_pages::table)
            .values(NewTeamPage {
                team_id,
                parent_id: input.parent_id,
                kind: input.kind,
                slug: input.slug.to_string(),
                title: input.title.to_string(),
                content: input.content.to_string(),
                position: input.position,
                is_protected: false,
                created_by: actor,
                updated_by: actor,
            })
            .returning(TeamPage::as_returning())
            .get_result(conn)
            .map_err(|e| match &e {
                DieselError::DatabaseError(
                    diesel::result::DatabaseErrorKind::UniqueViolation,
                    _,
                ) => TeamPageError::SlugConflict(input.slug.to_string()),
                _ => TeamPageError::Database(e),
            })?;
        if created.kind == TeamPageKind::Page {
            diesel::insert_into(crate::schema::team_page_history::table)
                .values(NewTeamPageHistory {
                    page_id: created.id,
                    version: 1,
                    title: created.title.clone(),
                    content: created.content.clone(),
                    edited_by: actor,
                })
                .execute(conn)?;
        }
        Ok(created)
    })
}

/// Version-checked title/content edit (the A-0004 pattern): the UPDATE
/// guards on the expected version; zero rows means conflict and the
/// current row rides in the error for the merge UI. Every save writes a
/// history row. One transaction.
///
/// KAIROS-T-0268: a save whose title and content are equal to the stored
/// values writes nothing (no new version, no history row) and gives the
/// stored page back.
pub fn update_page_content(
    conn: &mut PgConnection,
    team_id: Uuid,
    page_id: Uuid,
    new_title: Option<&str>,
    new_content: &str,
    expected_version: i32,
    actor: Uuid,
) -> Result<TeamPage, TeamPageError> {
    conn.transaction::<_, TeamPageError, _>(|conn| {
        use crate::schema::team_pages::dsl;
        check_size(new_content)?;
        let current = load_page(conn, team_id, page_id)?;
        if current.kind == TeamPageKind::Folder && new_content != current.content {
            return Err(TeamPageError::FolderContent(page_id));
        }
        let title = new_title.unwrap_or(&current.title).to_string();
        if title == current.title && new_content == current.content {
            return Ok(current);
        }
        let updated: Option<TeamPage> = diesel::update(
            dsl::team_pages
                .filter(dsl::id.eq(page_id))
                .filter(dsl::version.eq(expected_version)),
        )
        .set((
            dsl::title.eq(&title),
            dsl::content.eq(new_content),
            dsl::version.eq(dsl::version + 1),
            dsl::updated_by.eq(actor),
            dsl::updated_at.eq(diesel::dsl::now),
        ))
        .returning(TeamPage::as_returning())
        .get_result(conn)
        .optional()?;
        let Some(updated) = updated else {
            return Err(TeamPageError::VersionConflict {
                page_id,
                expected_version,
                current_version: current.version,
                current_title: current.title,
                current_content: current.content,
            });
        };
        diesel::insert_into(crate::schema::team_page_history::table)
            .values(NewTeamPageHistory {
                page_id,
                version: updated.version,
                title: updated.title.clone(),
                content: updated.content.clone(),
                edited_by: actor,
            })
            .execute(conn)?;
        Ok(updated)
    })
}

/// Rename and/or move a page (slug, parent, position). Refused for
/// protected pages; not version-checked (structure, not content).
///
/// KAIROS-T-0268: only a value that is different from the stored value
/// goes to the write. When no value is different, the function writes
/// nothing and gives the stored page back.
pub fn rename_move_page(
    conn: &mut PgConnection,
    team_id: Uuid,
    page_id: Uuid,
    new_slug: Option<&str>,
    new_parent: Option<Option<Uuid>>,
    new_position: Option<i32>,
    actor: Uuid,
) -> Result<TeamPage, TeamPageError> {
    conn.transaction::<_, TeamPageError, _>(|conn| {
        use crate::schema::team_pages::dsl;
        let current = load_page(conn, team_id, page_id)?;
        if current.is_protected {
            return Err(TeamPageError::ProtectedPage(page_id));
        }
        let new_slug = new_slug.filter(|slug| *slug != current.slug);
        let new_parent = new_parent.filter(|parent| *parent != current.parent_id);
        let new_position = new_position.filter(|position| *position != current.position);
        if new_slug.is_none() && new_parent.is_none() && new_position.is_none() {
            return Ok(current);
        }
        // KAIROS-T-0403: a rename or a move that puts a reserved slug at the
        // top of the tree. A page that has one there from before stays.
        if let Some(slug) = new_slug
            && !kairos_core::slug::is_valid_slug(slug)
        {
            return Err(TeamPageError::BadSlug(slug.to_string()));
        }
        let final_slug = new_slug.unwrap_or(&current.slug);
        let final_parent = new_parent.unwrap_or(current.parent_id);
        if (new_slug.is_some() || new_parent.is_some())
            && final_parent.is_none()
            && kairos_core::slug::is_site_reserved(final_slug)
        {
            return Err(TeamPageError::ReservedSlug(final_slug.to_string()));
        }
        if let Some(Some(parent)) = new_parent {
            check_parent(conn, team_id, parent)?;
            // No cycles: the new parent must not be the page itself or any
            // of its descendants (walk ancestors of the target parent).
            let mut cursor = Some(parent);
            while let Some(node) = cursor {
                if node == page_id {
                    return Err(TeamPageError::BadParent(parent));
                }
                cursor = dsl::team_pages
                    .filter(dsl::id.eq(node))
                    .select(dsl::parent_id)
                    .first::<Option<Uuid>>(conn)
                    .map_err(TeamPageError::Database)?;
            }
        }
        let updated: TeamPage = diesel::update(dsl::team_pages.filter(dsl::id.eq(page_id)))
            .set(TeamPageChangeset {
                slug: new_slug.map(str::to_string),
                parent_id: new_parent,
                position: new_position,
                updated_by: Some(actor),
                updated_at: Some(chrono::Utc::now()),
                ..Default::default()
            })
            .returning(TeamPage::as_returning())
            .get_result(conn)
            .map_err(|e| match &e {
                DieselError::DatabaseError(
                    diesel::result::DatabaseErrorKind::UniqueViolation,
                    _,
                ) => TeamPageError::SlugConflict(new_slug.unwrap_or(&current.slug).to_string()),
                _ => TeamPageError::Database(e),
            })?;
        Ok(updated)
    })
}

/// The live nodes inside `folder`, at any depth (not the folder itself).
fn live_descendants(
    conn: &mut PgConnection,
    team_id: Uuid,
    folder: Uuid,
) -> Result<Vec<Uuid>, DieselError> {
    use crate::schema::team_pages::dsl;
    let mut found = Vec::new();
    let mut frontier = vec![folder];
    // The depth bound guards against a malformed cycle.
    for _ in 0..64 {
        if frontier.is_empty() {
            break;
        }
        let next: Vec<Uuid> = dsl::team_pages
            .filter(dsl::team_id.eq(team_id))
            .filter(dsl::parent_id.eq_any(&frontier))
            .filter(dsl::deleted_at.is_null())
            .select(dsl::id)
            .load(conn)?;
        found.extend(next.iter().copied());
        frontier = next;
    }
    Ok(found)
}

/// Archive a page, or a folder and everything inside it (KAIROS-T-0439):
/// one `deleted_at` for the node and its live descendants, so a restore can
/// bring back exactly what was archived with it. The charter is refused.
/// Gives the number of nodes archived (1 for a page). One transaction.
pub fn soft_delete_page(
    conn: &mut PgConnection,
    team_id: Uuid,
    page_id: Uuid,
    actor: Uuid,
) -> Result<usize, TeamPageError> {
    conn.transaction::<_, TeamPageError, _>(|conn| {
        use crate::schema::team_pages::dsl;
        let current = load_page(conn, team_id, page_id)?;
        if current.is_protected {
            return Err(TeamPageError::ProtectedPage(page_id));
        }
        let mut ids = vec![page_id];
        if current.kind == TeamPageKind::Folder {
            ids.extend(live_descendants(conn, team_id, page_id)?);
        }
        let at = chrono::Utc::now();
        diesel::update(dsl::team_pages.filter(dsl::id.eq_any(&ids)))
            .set((
                dsl::deleted_at.eq(at),
                dsl::updated_by.eq(actor),
                dsl::updated_at.eq(diesel::dsl::now),
            ))
            .execute(conn)?;
        Ok(ids.len())
    })
}

/// The archived nodes of a team, newest archive first (KAIROS-T-0439).
pub fn list_archived_pages(
    conn: &mut PgConnection,
    team_id: Uuid,
) -> Result<Vec<TeamPage>, DieselError> {
    use crate::schema::team_pages::dsl;
    dsl::team_pages
        .filter(dsl::team_id.eq(team_id))
        .filter(dsl::deleted_at.is_not_null())
        .order((dsl::deleted_at.desc(), dsl::title.asc()))
        .select(TeamPage::as_select())
        .load(conn)
}

/// Restore an archived node and what was archived with it (the nodes inside
/// it with the same `deleted_at`). It goes back to its folder when that
/// folder is live, or to the end of the root when the folder is gone. A live
/// sibling with the same slug refuses the restore (`SlugConflict`). Gives the
/// restored node. One transaction.
pub fn restore_page(
    conn: &mut PgConnection,
    team_id: Uuid,
    page_id: Uuid,
    actor: Uuid,
) -> Result<TeamPage, TeamPageError> {
    conn.transaction::<_, TeamPageError, _>(|conn| {
        use crate::schema::team_pages::dsl;
        let current: TeamPage = dsl::team_pages
            .filter(dsl::id.eq(page_id))
            .filter(dsl::team_id.eq(team_id))
            .select(TeamPage::as_select())
            .first(conn)
            .optional()?
            .ok_or(TeamPageError::PageNotFound(page_id))?;
        let Some(archived_at) = current.deleted_at else {
            return Err(TeamPageError::NotArchived(page_id));
        };
        // The nodes archived with it: inside it, with the same timestamp.
        let mut with_it = Vec::new();
        let mut frontier = vec![page_id];
        for _ in 0..64 {
            if frontier.is_empty() {
                break;
            }
            let next: Vec<Uuid> = dsl::team_pages
                .filter(dsl::team_id.eq(team_id))
                .filter(dsl::parent_id.eq_any(&frontier))
                .filter(dsl::deleted_at.eq(archived_at))
                .select(dsl::id)
                .load(conn)?;
            with_it.extend(next.iter().copied());
            frontier = next;
        }
        // Its folder, when that folder is live; else the root.
        let parent_live = match current.parent_id {
            Some(parent) => dsl::team_pages
                .filter(dsl::id.eq(parent))
                .filter(dsl::deleted_at.is_null())
                .select(dsl::id)
                .first::<Uuid>(conn)
                .optional()?
                .is_some(),
            None => true,
        };
        let (parent_id, position) = if parent_live {
            (current.parent_id, current.position)
        } else {
            let last: Option<i32> = dsl::team_pages
                .filter(dsl::team_id.eq(team_id))
                .filter(dsl::parent_id.is_null())
                .filter(dsl::deleted_at.is_null())
                .select(diesel::dsl::max(dsl::position))
                .first(conn)?;
            (None, last.map_or(1, |p| p + 1))
        };
        // The root refuses a slug that the docs sites use (KAIROS-T-0403),
        // also for a node that comes back there because its folder is gone.
        if parent_id.is_none() && kairos_core::slug::is_site_reserved(&current.slug) {
            return Err(TeamPageError::ReservedSlug(current.slug.clone()));
        }
        let restored: TeamPage = diesel::update(dsl::team_pages.filter(dsl::id.eq(page_id)))
            .set((
                dsl::deleted_at.eq(None::<chrono::DateTime<chrono::Utc>>),
                dsl::parent_id.eq(parent_id),
                dsl::position.eq(position),
                dsl::updated_by.eq(actor),
                dsl::updated_at.eq(diesel::dsl::now),
            ))
            .returning(TeamPage::as_returning())
            .get_result(conn)
            .map_err(|e| match &e {
                DieselError::DatabaseError(
                    diesel::result::DatabaseErrorKind::UniqueViolation,
                    _,
                ) => TeamPageError::SlugConflict(current.slug.clone()),
                _ => TeamPageError::Database(e),
            })?;
        if !with_it.is_empty() {
            diesel::update(dsl::team_pages.filter(dsl::id.eq_any(&with_it)))
                .set((
                    dsl::deleted_at.eq(None::<chrono::DateTime<chrono::Utc>>),
                    dsl::updated_by.eq(actor),
                    dsl::updated_at.eq(diesel::dsl::now),
                ))
                .execute(conn)?;
        }
        Ok(restored)
    })
}

/// The saved versions of a page, newest first (KAIROS-T-0439). An archived
/// page has its history too.
pub fn page_history(
    conn: &mut PgConnection,
    team_id: Uuid,
    page_id: Uuid,
) -> Result<Vec<TeamPageHistory>, TeamPageError> {
    use crate::schema::{team_page_history as history, team_pages as pages};
    let exists: Option<Uuid> = pages::table
        .filter(pages::id.eq(page_id))
        .filter(pages::team_id.eq(team_id))
        .select(pages::id)
        .first(conn)
        .optional()?;
    exists.ok_or(TeamPageError::PageNotFound(page_id))?;
    Ok(history::table
        .filter(history::page_id.eq(page_id))
        .order(history::version.desc())
        .select(TeamPageHistory::as_select())
        .load(conn)?)
}

/// A team's announcements, pinned first then newest first.
pub fn list_announcements(
    conn: &mut PgConnection,
    team_id: Uuid,
) -> Result<Vec<TeamAnnouncement>, DieselError> {
    use crate::schema::team_announcements::dsl;
    dsl::team_announcements
        .filter(dsl::team_id.eq(team_id))
        .order((dsl::pinned.desc(), dsl::created_at.desc()))
        .select(TeamAnnouncement::as_select())
        .load(conn)
}

// ---------------------------------------------------------------------------
// Items in the tree (KAIROS-T-0437)
// ---------------------------------------------------------------------------

/// The items of a team that are part of its tree: each live document and
/// ADR whose owner board is a live board of the team. The SQL is shared by
/// [`team_tree_items`] and the check of [`place_item`].
const TEAM_ITEMS_SQL: &str = "\
    SELECT d.id, d.short_code, 'document'::text AS entity_type, d.title, \
        d.lifecycle::text AS status \
    FROM documents d \
    JOIN boards b ON b.id = d.board_id AND b.deleted_at IS NULL AND b.team_id = $1 \
    WHERE d.deleted_at IS NULL \
    UNION ALL \
    SELECT a.id, a.short_code, 'adr'::text AS entity_type, a.title, \
        COALESCE(c.name, '') AS status \
    FROM adrs a \
    JOIN boards b ON b.id = a.board_id AND b.deleted_at IS NULL AND b.team_id = $1 \
    LEFT JOIN board_columns c ON c.id = a.column_id \
    WHERE a.deleted_at IS NULL";

/// One document or ADR in the tree of a team.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct TreeItem {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub item_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub short_code: String,
    /// `document` or `adr`.
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub entity_type: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub title: String,
    /// The lifecycle of a document, or the column of an ADR: the board
    /// shows where the item is; the tree shows where it lives.
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub status: String,
    /// Its folder, when it is placed in a live folder; `None` is the root.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    pub parent_id: Option<Uuid>,
    /// Its position among its siblings; `None` when nobody placed it (it
    /// shows at the end of the root).
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Integer>)]
    pub position: Option<i32>,
}

/// The documents and ADRs of a team, each with its place in the tree
/// (KAIROS-T-0437). An item that nobody placed, or whose folder is archived,
/// is at the root.
pub fn team_tree_items(
    conn: &mut PgConnection,
    team_id: Uuid,
) -> Result<Vec<TreeItem>, DieselError> {
    diesel::sql_query(format!(
        "SELECT x.id AS item_id, x.short_code, x.entity_type, x.title, x.status, \
             CASE WHEN f.id IS NOT NULL THEN p.parent_id END AS parent_id, \
             p.position \
         FROM ({TEAM_ITEMS_SQL}) x \
         LEFT JOIN team_page_placements p ON p.item_id = x.id AND p.team_id = $1 \
         LEFT JOIN team_pages f ON f.id = p.parent_id AND f.deleted_at IS NULL \
         ORDER BY x.short_code"
    ))
    .bind::<diesel::sql_types::Uuid, _>(team_id)
    .load(conn)
}

/// Put a document or an ADR of the team in a folder (`None`: the root) at a
/// position (KAIROS-T-0437). An item has one place: a second call moves it.
/// Refused: an item that is not the team's (`NotTeamItem`), a parent that
/// is not a live folder of the team (`BadParent`).
pub fn place_item(
    conn: &mut PgConnection,
    team_id: Uuid,
    item_id: Uuid,
    parent_id: Option<Uuid>,
    position: i32,
    actor: Uuid,
) -> Result<(), TeamPageError> {
    conn.transaction::<_, TeamPageError, _>(|conn| {
        #[derive(QueryableByName)]
        struct Found {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        let found: Found = diesel::sql_query(format!(
            "SELECT count(*) AS n FROM ({TEAM_ITEMS_SQL}) x WHERE x.id = $2"
        ))
        .bind::<diesel::sql_types::Uuid, _>(team_id)
        .bind::<diesel::sql_types::Uuid, _>(item_id)
        .get_result(conn)?;
        if found.n == 0 {
            return Err(TeamPageError::NotTeamItem(item_id));
        }
        if let Some(parent) = parent_id {
            check_parent(conn, team_id, parent)?;
        }
        diesel::sql_query(
            "INSERT INTO team_page_placements (item_id, team_id, parent_id, position, placed_by) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (item_id) DO UPDATE SET team_id = EXCLUDED.team_id, \
                 parent_id = EXCLUDED.parent_id, position = EXCLUDED.position, \
                 placed_by = EXCLUDED.placed_by, updated_at = now()",
        )
        .bind::<diesel::sql_types::Uuid, _>(item_id)
        .bind::<diesel::sql_types::Uuid, _>(team_id)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(parent_id)
        .bind::<diesel::sql_types::Integer, _>(position)
        .bind::<diesel::sql_types::Uuid, _>(actor)
        .execute(conn)?;
        Ok(())
    })
}
