//! KAIROS-T-0629 — the lists read bounded, metadata-only rows, and the
//! cascade reads the `parent` edges below its root only.
//!
//! The statements run on a connection that records each one (diesel
//! instrumentation). The test checks the SQL text and the rows:
//!
//! - the version list of an item, the page tree, the archived nodes and the
//!   version list of a page select no `content` column;
//! - the page history and the announcements are paged (limit, offset,
//!   total);
//! - one version of a page, and one page, still have their content;
//! - [`subtree_parent_edges`] is one recursive CTE, and it gives the edges
//!   below the root and no other edge of the tenant.
//!
//! Runs against the real compose Postgres (`angreal services up`); owns
//! the scratch database `kairos_metadata_lists_t0629_test`.

#[path = "common/scratch.rs"]
mod scratch;

use std::sync::{Arc, Mutex};

use diesel::connection::{Instrumentation, InstrumentationEvent};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{Integer, Text, Uuid as SqlUuid};
use uuid::Uuid;

use kairos_db::items::subtree_parent_edges;
use kairos_db::models::enums::{TeamPageKind, TeamType};
use kairos_db::models::team_pages::NewTeamAnnouncement;
use kairos_db::models::teams::{NewTeam, Team};
use kairos_db::team_pages::{self, CreatePage};
use kairos_db::{provision_tenant, run_public_migrations, schema};

static SCRATCH_DB: scratch::ScratchName =
    scratch::ScratchName::new("kairos_metadata_lists_t0629_test");

/// The text that each content of the fixture has. No list may return it.
const SECRET: &str = "CONTENT-OF-THE-FIXTURE";

/// Records the SQL of each statement that the connection starts.
struct Recorder {
    queries: Arc<Mutex<Vec<String>>>,
}

impl Instrumentation for Recorder {
    fn on_connection_event(&mut self, event: InstrumentationEvent<'_>) {
        if let InstrumentationEvent::StartQuery { query, .. } = event {
            self.queries
                .lock()
                .expect("queries")
                .push(query.to_string());
        }
    }
}

/// Run `read` and give its result and the SQL of each statement it ran.
fn recorded<T>(
    conn: &mut PgConnection,
    read: impl FnOnce(&mut PgConnection) -> T,
) -> (T, Vec<String>) {
    let queries = Arc::new(Mutex::new(Vec::new()));
    conn.set_instrumentation(Recorder {
        queries: queries.clone(),
    });
    let out = read(conn);
    conn.set_instrumentation(|_: InstrumentationEvent<'_>| {});
    let queries = queries.lock().expect("queries").clone();
    (out, queries)
}

/// No statement selects a column named `content`.
fn assert_no_content(what: &str, queries: &[String]) {
    assert!(!queries.is_empty(), "{what}: no statement was recorded");
    for query in queries {
        assert!(
            !query.contains("\"content\"") && !query.contains(".content"),
            "{what} reads a content column: {query}"
        );
    }
}

#[test]
fn the_lists_read_no_content_and_the_cascade_reads_the_subtree() {
    let admin_url = scratch::admin_database_url();
    let mut admin_conn = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
        panic!("cannot connect to compose postgres at {admin_url}: {e} (`angreal services up`)")
    });
    scratch::drop_quietly(&mut admin_conn, &SCRATCH_DB);
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin_conn)
        .expect("creating scratch database");
    let _guard = scratch::ScratchGuard::new(&SCRATCH_DB);
    let (base, _) = admin_url.rsplit_once('/').expect("a database in the URL");
    let mut conn =
        PgConnection::establish(&format!("{base}/{SCRATCH_DB}")).expect("scratch database");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("search_path");

    let actor = Uuid::new_v4();
    let team: Team = diesel::insert_into(schema::teams::table)
        .values(NewTeam {
            name: "Platform".to_string(),
            slug: "platform".to_string(),
            team_type: TeamType::Platform,
        })
        .returning(Team::as_returning())
        .get_result(&mut conn)
        .expect("team");
    team_pages::seed_team_charter(&mut conn, team.id, actor).expect("charter");
    let folder = team_pages::create_page(
        &mut conn,
        team.id,
        CreatePage {
            parent_id: None,
            kind: TeamPageKind::Folder,
            slug: "guides",
            title: "Guides",
            content: "",
            position: 1,
        },
        actor,
    )
    .expect("folder");
    let page = team_pages::create_page(
        &mut conn,
        team.id,
        CreatePage {
            parent_id: Some(folder.id),
            kind: TeamPageKind::Page,
            slug: "deploy",
            title: "Deploy",
            content: &format!("v1 {SECRET}"),
            position: 0,
        },
        actor,
    )
    .expect("page");
    for version in 1..5 {
        team_pages::update_page_content(
            &mut conn,
            team.id,
            page.id,
            None,
            &format!("v{} {SECRET}", version + 1),
            version,
            actor,
        )
        .expect("an edit");
    }
    let gone = team_pages::create_page(
        &mut conn,
        team.id,
        CreatePage {
            parent_id: None,
            kind: TeamPageKind::Page,
            slug: "old",
            title: "Old",
            content: SECRET,
            position: 2,
        },
        actor,
    )
    .expect("a page to archive");
    team_pages::soft_delete_page(&mut conn, team.id, gone.id, actor).expect("archive");
    for n in 0..3 {
        diesel::insert_into(schema::team_announcements::table)
            .values(NewTeamAnnouncement {
                team_id: team.id,
                body: format!("announcement {n}"),
                pinned: n == 0,
                created_by: actor,
            })
            .execute(&mut conn)
            .expect("announcement");
    }

    // ---- the page tree and the archive: no content ------------------------
    let (pages, queries) = recorded(&mut conn, |c| team_pages::list_pages(c, team.id));
    let pages = pages.expect("the tree");
    assert_eq!(pages.len(), 3, "charter, folder, page: {pages:?}");
    assert_no_content("list_pages", &queries);
    let (archived, queries) = recorded(&mut conn, |c| team_pages::list_archived_pages(c, team.id));
    assert_eq!(archived.expect("the archive").len(), 1);
    assert_no_content("list_archived_pages", &queries);
    // One page still has its content.
    let one = team_pages::load_page(&mut conn, team.id, page.id).expect("one page");
    assert_eq!(one.content, format!("v5 {SECRET}"));

    // ---- the page history: paged, no content -----------------------------
    let (history, queries) = recorded(&mut conn, |c| {
        team_pages::page_history(c, team.id, page.id, 2, 1)
    });
    let (total, rows) = history.expect("the history");
    assert_eq!(total, 5);
    assert_eq!(
        rows.iter().map(|r| r.version).collect::<Vec<_>>(),
        [4, 3],
        "newest first, after the offset"
    );
    assert_no_content("page_history", &queries);
    let v2 = team_pages::page_version(&mut conn, team.id, page.id, 2)
        .expect("one version")
        .expect("version 2");
    assert_eq!(v2.content, format!("v2 {SECRET}"));
    assert!(
        team_pages::page_version(&mut conn, team.id, page.id, 9)
            .expect("no version 9")
            .is_none()
    );
    assert!(matches!(
        team_pages::page_history(&mut conn, team.id, Uuid::new_v4(), 10, 0),
        Err(team_pages::TeamPageError::PageNotFound(_))
    ));

    // ---- the announcements: paged -----------------------------------------
    let (total, rows) =
        team_pages::list_announcements(&mut conn, team.id, 2, 0).expect("announcements");
    assert_eq!(total, 3);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].pinned, "pinned first");
    let (_, rest) = team_pages::list_announcements(&mut conn, team.id, 2, 2).expect("page 2");
    assert_eq!(rest.len(), 1);

    // ---- the version list of an item: no title, no content ----------------
    let item = Uuid::new_v4();
    for version in 1..=4 {
        sql_query(
            "INSERT INTO item_history (item_id, version, title, content, edited_by) \
             VALUES ($1, $2, $3, $3, $4)",
        )
        .bind::<SqlUuid, _>(item)
        .bind::<Integer, _>(version)
        .bind::<Text, _>(SECRET)
        .bind::<SqlUuid, _>(actor)
        .execute(&mut conn)
        .expect("a history row");
    }
    let (versions, queries) = recorded(&mut conn, |c| {
        kairos_db::items::history_versions(c, item, 3, 0)
    });
    let (total, rows) = versions.expect("the version list");
    assert_eq!(total, 4);
    assert_eq!(
        rows.iter().map(|r| r.version).collect::<Vec<_>>(),
        [4, 3, 2]
    );
    assert_no_content("history_versions", &queries);
    for query in &queries {
        assert!(
            !query.contains(".\"title\""),
            "the list reads the title: {query}"
        );
    }

    // ---- the cascade: the edges below the root, in one CTE ----------------
    let [up, root, a, b, c, x, y, z] = [(); 8].map(|_| Uuid::new_v4());
    for (source, target, kind) in [
        (up, root, "parent"),
        (root, a, "parent"),
        (a, b, "parent"),
        (root, c, "parent"),
        (root, x, "blocks"),
        (x, y, "parent"),
        (y, z, "parent"),
    ] {
        sql_query(
            "INSERT INTO item_relationships (source_id, target_id, relationship) \
             VALUES ($1, $2, $3)",
        )
        .bind::<SqlUuid, _>(source)
        .bind::<SqlUuid, _>(target)
        .bind::<Text, _>(kind)
        .execute(&mut conn)
        .expect("an edge");
    }
    let (edges, queries) = recorded(&mut conn, |c| subtree_parent_edges(c, root));
    let mut edges: Vec<(Uuid, Uuid)> = edges
        .expect("the subtree")
        .into_iter()
        .map(|e| (e.parent_id, e.child_id))
        .collect();
    edges.sort();
    let mut want = vec![(root, a), (a, b), (root, c)];
    want.sort();
    assert_eq!(
        edges, want,
        "the parent edges below the root only: not the edge above it, not a blocks edge, not another tree"
    );
    assert_eq!(queries.len(), 1, "one statement: {queries:?}");
    assert!(queries[0].contains("WITH RECURSIVE"), "{}", queries[0]);
    let leaf = subtree_parent_edges(&mut conn, b).expect("a leaf");
    assert!(leaf.is_empty(), "a leaf has no edge below it");
}
