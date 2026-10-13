//! The content of the rendered docs sites (KAIROS-T-0404, KAIROS-D-0013).
//!
//! A team's site: the documents owned by its delivery board, the ADRs of its
//! ADR board, and its team pages. The org site: the teams with a site, the
//! documents owned by a strategy or an initiative board, and the ADRs of the
//! ADR board with no team. Archived items are left out (`deleted_at`, or the
//! lifecycle `archived`).
//!
//! Each read here is a few queries for a whole page, never one per item.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_types::{Array, Bool, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use uuid::Uuid;

use crate::models::team_pages::TeamPage;
use crate::schema::teams;

/// A team, with its site switch.
#[derive(Debug, Clone, PartialEq, Eq, Queryable)]
pub struct SiteTeam {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub site_enabled: bool,
}

/// A document or an ADR in a list of a site.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct SiteItem {
    #[diesel(sql_type = SqlUuid)]
    pub id: Uuid,
    #[diesel(sql_type = Text)]
    pub short_code: String,
    #[diesel(sql_type = Text)]
    pub title: String,
    /// `document` or `adr`.
    #[diesel(sql_type = Text)]
    pub kind: String,
    /// A document: its lifecycle. An ADR: the name of its column.
    #[diesel(sql_type = Nullable<Text>)]
    pub status: Option<String>,
    #[diesel(sql_type = Timestamptz)]
    pub updated_at: DateTime<Utc>,
    /// An ADR that a newer ADR supersedes: the code of the newer one.
    #[diesel(sql_type = Nullable<Text>)]
    pub superseded_by: Option<String>,
    /// The strategy or initiative a document supports (org site): its code.
    #[diesel(sql_type = Nullable<Text>)]
    pub supports_code: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub supports_title: Option<String>,
}

/// One document or ADR in full, for its page.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct SitePage {
    #[diesel(sql_type = SqlUuid)]
    pub id: Uuid,
    #[diesel(sql_type = Text)]
    pub short_code: String,
    #[diesel(sql_type = Text)]
    pub title: String,
    #[diesel(sql_type = Text)]
    pub content: String,
    /// `document` or `adr`.
    #[diesel(sql_type = Text)]
    pub kind: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub status: Option<String>,
    #[diesel(sql_type = Timestamptz)]
    pub updated_at: DateTime<Utc>,
    #[diesel(sql_type = Text)]
    pub board_name: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub superseded_by: Option<String>,
}

/// A repository of a team. KAIROS-T-0407 adds the link to its site.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct SiteRepository {
    #[diesel(sql_type = SqlUuid)]
    pub id: Uuid,
    #[diesel(sql_type = Text)]
    pub slug: String,
}

/// Where a published document or ADR has its page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// The org site.
    Org,
    /// The site of the team with this slug.
    Team(String),
}

/// The live teams by slug, with their site switch.
pub fn team_by_slug(conn: &mut PgConnection, slug: &str) -> Result<Option<SiteTeam>, DieselError> {
    teams::table
        .filter(teams::slug.eq(slug))
        .filter(teams::deleted_at.is_null())
        .select((teams::id, teams::name, teams::slug, teams::site_enabled))
        .first(conn)
        .optional()
}

/// The live teams that have their site on, by name.
pub fn teams_with_site(conn: &mut PgConnection) -> Result<Vec<SiteTeam>, DieselError> {
    teams::table
        .filter(teams::deleted_at.is_null())
        .filter(teams::site_enabled.eq(true))
        .order(teams::name.asc())
        .select((teams::id, teams::name, teams::slug, teams::site_enabled))
        .load(conn)
}

/// The SELECT of a document in a list.
const DOCUMENT_ITEM: &str = "SELECT d.id, d.short_code, d.title, 'document' AS kind, \
     d.lifecycle AS status, d.updated_at, NULL::text AS superseded_by, \
     sup.short_code AS supports_code, sup.title AS supports_title \
     FROM documents d JOIN boards b ON b.id = d.board_id \
     LEFT JOIN LATERAL ( \
        SELECT e.short_code, e.title FROM item_relationships r \
        JOIN entity_directory e ON e.id = r.source_id \
        WHERE r.relationship = 'supports' AND r.target_id = d.id \
          AND e.entity_type IN ('strategy', 'initiative') AND e.deleted_at IS NULL \
        ORDER BY e.short_code LIMIT 1) sup ON true \
     WHERE d.deleted_at IS NULL AND d.lifecycle <> 'archived'";

/// The SELECT of an ADR in a list.
const ADR_ITEM: &str = "SELECT a.id, a.short_code, a.title, 'adr' AS kind, \
     c.name AS status, a.updated_at, \
     (SELECT n.short_code FROM item_relationships r JOIN adrs n ON n.id = r.source_id \
        WHERE r.relationship = 'supersedes' AND r.target_id = a.id AND n.deleted_at IS NULL \
        ORDER BY n.short_code LIMIT 1) AS superseded_by, \
     NULL::text AS supports_code, NULL::text AS supports_title \
     FROM adrs a JOIN boards b ON b.id = a.board_id \
     LEFT JOIN board_columns c ON c.id = a.column_id \
     WHERE a.deleted_at IS NULL";

/// The documents of the team's delivery board, by code.
pub fn team_documents(
    conn: &mut PgConnection,
    team_id: Uuid,
) -> Result<Vec<SiteItem>, DieselError> {
    diesel::sql_query(format!(
        "{DOCUMENT_ITEM} AND b.board_level = 'delivery' AND b.team_id = $1 \
         AND b.deleted_at IS NULL ORDER BY d.short_code"
    ))
    .bind::<SqlUuid, _>(team_id)
    .load(conn)
}

/// The ADRs of the team's ADR board, newest first.
pub fn team_adrs(conn: &mut PgConnection, team_id: Uuid) -> Result<Vec<SiteItem>, DieselError> {
    diesel::sql_query(format!(
        "{ADR_ITEM} AND b.board_level = 'adr' AND b.team_id = $1 \
         AND b.deleted_at IS NULL ORDER BY a.created_at DESC, a.short_code DESC"
    ))
    .bind::<SqlUuid, _>(team_id)
    .load(conn)
}

/// The documents owned by a strategy or an initiative board, by code.
pub fn org_documents(conn: &mut PgConnection) -> Result<Vec<SiteItem>, DieselError> {
    diesel::sql_query(format!(
        "{DOCUMENT_ITEM} AND b.board_level IN ('strategy', 'initiative') \
         AND b.deleted_at IS NULL ORDER BY d.short_code"
    ))
    .load(conn)
}

/// The ADRs of the ADR board with no team, newest first.
pub fn org_adrs(conn: &mut PgConnection) -> Result<Vec<SiteItem>, DieselError> {
    diesel::sql_query(format!(
        "{ADR_ITEM} AND b.board_level = 'adr' AND b.team_id IS NULL \
         AND b.deleted_at IS NULL ORDER BY a.created_at DESC, a.short_code DESC"
    ))
    .load(conn)
}

/// The live repositories of a team, by slug.
pub fn team_repositories(
    conn: &mut PgConnection,
    team_id: Uuid,
) -> Result<Vec<SiteRepository>, DieselError> {
    diesel::sql_query(
        "SELECT r.id, r.slug FROM repositories r \
         WHERE r.team_id = $1 AND r.deleted_at IS NULL ORDER BY r.slug",
    )
    .bind::<SqlUuid, _>(team_id)
    .load(conn)
}

/// The repositories that each item impacts.
pub fn impacts(
    conn: &mut PgConnection,
    item_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, DieselError> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        item_id: Uuid,
        #[diesel(sql_type = SqlUuid)]
        target_id: Uuid,
    }
    let rows: Vec<Row> = diesel::sql_query(
        "SELECT item_id, target_id FROM item_impacts \
         WHERE target_kind = 'repository' AND item_id = ANY($1)",
    )
    .bind::<Array<SqlUuid>, _>(item_ids)
    .load(conn)?;
    let mut map: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for row in rows {
        map.entry(row.item_id).or_default().push(row.target_id);
    }
    Ok(map)
}

/// The live team pages of a team, flat, by position then title.
pub fn team_pages(conn: &mut PgConnection, team_id: Uuid) -> Result<Vec<TeamPage>, DieselError> {
    crate::team_pages::list_pages(conn, team_id)
}

/// A document or an ADR with its page, by its current code, and where its
/// page is. `None` when no live, published item has the code.
pub fn page(conn: &mut PgConnection, code: &str) -> Result<Option<(SitePage, Place)>, DieselError> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: Uuid,
        #[diesel(sql_type = Text)]
        short_code: String,
        #[diesel(sql_type = Text)]
        title: String,
        #[diesel(sql_type = Text)]
        content: String,
        #[diesel(sql_type = Text)]
        kind: String,
        #[diesel(sql_type = Nullable<Text>)]
        status: Option<String>,
        #[diesel(sql_type = Timestamptz)]
        updated_at: DateTime<Utc>,
        #[diesel(sql_type = Text)]
        board_name: String,
        #[diesel(sql_type = Nullable<Text>)]
        superseded_by: Option<String>,
        #[diesel(sql_type = Text)]
        board_level: String,
        #[diesel(sql_type = Nullable<Text>)]
        team_slug: Option<String>,
        #[diesel(sql_type = Nullable<Bool>)]
        team_site: Option<bool>,
    }
    let rows: Vec<Row> = diesel::sql_query(
        "SELECT d.id, d.short_code, d.title, d.content, 'document' AS kind, \
           d.lifecycle AS status, d.updated_at, b.name AS board_name, \
           NULL::text AS superseded_by, b.board_level, t.slug AS team_slug, \
           t.site_enabled AS team_site \
         FROM documents d JOIN boards b ON b.id = d.board_id \
         LEFT JOIN teams t ON t.id = b.team_id AND t.deleted_at IS NULL \
         WHERE d.short_code = $1 AND d.deleted_at IS NULL AND d.lifecycle <> 'archived' \
         UNION ALL \
         SELECT a.id, a.short_code, a.title, a.content, 'adr' AS kind, c.name AS status, \
           a.updated_at, b.name AS board_name, \
           (SELECT n.short_code FROM item_relationships r JOIN adrs n ON n.id = r.source_id \
              WHERE r.relationship = 'supersedes' AND r.target_id = a.id \
                AND n.deleted_at IS NULL ORDER BY n.short_code LIMIT 1), \
           b.board_level, t.slug AS team_slug, t.site_enabled AS team_site \
         FROM adrs a JOIN boards b ON b.id = a.board_id \
         LEFT JOIN board_columns c ON c.id = a.column_id \
         LEFT JOIN teams t ON t.id = b.team_id AND t.deleted_at IS NULL \
         WHERE a.short_code = $1 AND a.deleted_at IS NULL",
    )
    .bind::<Text, _>(code)
    .load(conn)?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let Some(place) = place_of(&row.kind, &row.board_level, row.team_slug, row.team_site) else {
        return Ok(None);
    };
    Ok(Some((
        SitePage {
            id: row.id,
            short_code: row.short_code,
            title: row.title,
            content: row.content,
            kind: row.kind,
            status: row.status,
            updated_at: row.updated_at,
            board_name: row.board_name,
            superseded_by: row.superseded_by,
        },
        place,
    )))
}

/// Where an item has its page, from its board: a team's delivery or ADR
/// board with the team site on, or a strategy, initiative or team-less ADR
/// board for the org. Anything else has no page.
fn place_of(
    kind: &str,
    board_level: &str,
    team_slug: Option<String>,
    team_site: Option<bool>,
) -> Option<Place> {
    match (kind, board_level, team_slug) {
        ("document", "delivery", Some(team)) | ("adr", "adr", Some(team)) => {
            team_site.unwrap_or(false).then_some(Place::Team(team))
        }
        ("document", "strategy" | "initiative", None) | ("adr", "adr", None) => Some(Place::Org),
        _ => None,
    }
}

/// For each of `codes` that a live, published document or ADR has, where
/// its page is. One query for the whole list.
pub fn places(
    conn: &mut PgConnection,
    codes: &[String],
) -> Result<HashMap<String, (String, Place)>, DieselError> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        short_code: String,
        #[diesel(sql_type = Text)]
        kind: String,
        #[diesel(sql_type = Text)]
        board_level: String,
        #[diesel(sql_type = Nullable<Text>)]
        team_slug: Option<String>,
        #[diesel(sql_type = Nullable<Bool>)]
        team_site: Option<bool>,
    }
    if codes.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Row> = diesel::sql_query(
        "SELECT d.short_code, 'document' AS kind, b.board_level, t.slug AS team_slug, \
           t.site_enabled AS team_site \
         FROM documents d JOIN boards b ON b.id = d.board_id \
         LEFT JOIN teams t ON t.id = b.team_id AND t.deleted_at IS NULL \
         WHERE d.short_code = ANY($1) AND d.deleted_at IS NULL AND d.lifecycle <> 'archived' \
         UNION ALL \
         SELECT a.short_code, 'adr' AS kind, b.board_level, t.slug AS team_slug, \
           t.site_enabled AS team_site \
         FROM adrs a JOIN boards b ON b.id = a.board_id \
         LEFT JOIN teams t ON t.id = b.team_id AND t.deleted_at IS NULL \
         WHERE a.short_code = ANY($1) AND a.deleted_at IS NULL",
    )
    .bind::<Array<Text>, _>(codes)
    .load(conn)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let place = place_of(&row.kind, &row.board_level, row.team_slug, row.team_site)?;
            Some((row.short_code, (row.kind, place)))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn where_an_item_has_its_page() {
        let team = || Some("platform".to_string());
        assert_eq!(
            place_of("document", "delivery", team(), Some(true)),
            Some(Place::Team("platform".into()))
        );
        assert_eq!(place_of("document", "delivery", team(), Some(false)), None);
        assert_eq!(
            place_of("adr", "adr", team(), Some(true)),
            Some(Place::Team("platform".into()))
        );
        assert_eq!(place_of("adr", "adr", None, None), Some(Place::Org));
        assert_eq!(
            place_of("document", "initiative", None, None),
            Some(Place::Org)
        );
        assert_eq!(
            place_of("document", "strategy", None, None),
            Some(Place::Org)
        );
        // A document on a board with no team that is not a strategy or an
        // initiative board has no page.
        assert_eq!(place_of("document", "delivery", None, None), None);
    }
}
