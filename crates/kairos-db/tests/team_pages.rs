//! Integration test for the team pages (KAIROS-T-0082, KAIROS-T-0436): a new
//! team gets its charter only; the charter cannot be deleted or moved but
//! can be edited; history baselines, idempotency, sibling-slug uniqueness
//! (including NULL-parent roots); and the migration that removes the unused
//! seeded folders of the old teams.
//!
//! Runs against the real compose Postgres (`angreal services up`); owns
//! the scratch database `kairos_team_pages_test`.

use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::{DatabaseErrorKind, Error as DieselError};
use diesel::sql_query;
use uuid::Uuid;

use kairos_db::models::enums::{TeamPageKind, TeamType};
use kairos_db::models::team_pages::{NewTeamPage, TeamPage};
use kairos_db::models::teams::{NewTeam, Team};
use kairos_db::team_pages::{CreatePage, TeamPageError, create_page, seed_team_charter};
use kairos_db::{provision_tenant, run_public_migrations, schema};

const DEFAULT_DATABASE_URL: &str = "postgres://kairos:kairos@localhost:41432/kairos";
const SCRATCH_DB: &str = "kairos_team_pages_test";
/// KAIROS-T-0436: the migration that removes the unused seeded folders.
const SCAFFOLD_REMOVED: &str =
    include_str!("../migrations/tenant/2026-10-14-000200_team_scaffold_removed/up.sql");

fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("DATABASE_URL must contain a database path segment");
    format!("{base}/{db_name}")
}

/// All live scaffold nodes of a team as (parent slug or "", slug, kind,
/// protected), sorted.
fn scaffold_shape(conn: &mut PgConnection, team: Uuid) -> Vec<(String, String, String, bool)> {
    let pages: Vec<TeamPage> = schema::team_pages::table
        .filter(schema::team_pages::team_id.eq(team))
        .filter(schema::team_pages::deleted_at.is_null())
        .select(TeamPage::as_select())
        .load(conn)
        .expect("loading team pages");
    let name_of = |id: Uuid| {
        pages
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.slug.clone())
            .unwrap_or_default()
    };
    let mut shape: Vec<(String, String, String, bool)> = pages
        .iter()
        .map(|p| {
            (
                p.parent_id.map(name_of).unwrap_or_default(),
                p.slug.clone(),
                p.kind.to_string(),
                p.is_protected,
            )
        })
        .collect();
    shape.sort();
    shape
}

#[test]
fn team_page_scaffold() {
    let admin_url = admin_database_url();
    let mut admin_conn = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
        panic!(
            "cannot connect to compose postgres at {admin_url}: {e} \
             (is the stack up? `angreal services up`)"
        )
    });
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin_conn)
        .expect("dropping scratch database");
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin_conn)
        .expect("creating scratch database");
    let scratch_url = with_database(&admin_url, SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connecting to scratch database");

    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");

    let actor = Uuid::new_v4();
    let team: Team = diesel::insert_into(schema::teams::table)
        .values(NewTeam {
            name: "Platform".to_string(),
            slug: "platform".to_string(),
            team_type: TeamType::Platform,
        })
        .returning(Team::as_returning())
        .get_result(&mut conn)
        .expect("creating team");

    // ---- a new team gets its charter only (KAIROS-T-0436) --------------------
    seed_team_charter(&mut conn, team.id, actor).expect("seeding the charter");
    assert_eq!(
        scaffold_shape(&mut conn, team.id),
        [(
            "".to_string(),
            "charter".to_string(),
            "page".to_string(),
            true
        )],
        "the charter, protected, at the top; no seeded folders"
    );
    let history_count: i64 = schema::team_page_history::table
        .count()
        .get_result(&mut conn)
        .expect("counting history");
    assert_eq!(history_count, 1, "a v1 baseline for the charter");

    // ---- idempotency ----------------------------------------------------------
    seed_team_charter(&mut conn, team.id, actor).expect("re-seeding the charter");
    assert_eq!(
        scaffold_shape(&mut conn, team.id).len(),
        1,
        "re-running the seed adds nothing"
    );

    // ---- the charter: fixed, editable ------------------------------------------
    let charter: TeamPage = schema::team_pages::table
        .filter(schema::team_pages::team_id.eq(team.id))
        .filter(schema::team_pages::slug.eq("charter"))
        .select(TeamPage::as_select())
        .first(&mut conn)
        .expect("the charter");
    match kairos_db::team_pages::soft_delete_page(&mut conn, team.id, charter.id, actor) {
        Err(TeamPageError::ProtectedPage(_)) => {}
        other => panic!("deleting the charter must be refused, got {other:?}"),
    }
    let folder = create_page(
        &mut conn,
        team.id,
        CreatePage {
            parent_id: None,
            kind: TeamPageKind::Folder,
            slug: "handbook",
            title: "Handbook",
            content: "",
            position: 1,
        },
        actor,
    )
    .expect("a folder the team makes");
    for (slug, parent, position) in [
        (Some("landing"), None, None),
        (None, Some(Some(folder.id)), None),
        (None, None, Some(5)),
    ] {
        match kairos_db::team_pages::rename_move_page(
            &mut conn, team.id, charter.id, slug, parent, position, actor,
        ) {
            Err(TeamPageError::ProtectedPage(_)) => {}
            other => panic!("renaming or moving the charter must be refused, got {other:?}"),
        }
    }
    let edited = kairos_db::team_pages::update_page_content(
        &mut conn,
        team.id,
        charter.id,
        Some("Platform charter"),
        "# Platform charter\n\n## Mission\n\nRun the platform.\n",
        charter.version,
        actor,
    )
    .expect("the title and the content of the charter can be edited");
    assert_eq!(edited.title, "Platform charter");
    assert_eq!(edited.version, charter.version + 1);

    // ---- sibling-slug uniqueness, including NULL-parent roots -----------------
    let duplicate_root = diesel::insert_into(schema::team_pages::table)
        .values(NewTeamPage {
            team_id: team.id,
            parent_id: None,
            kind: TeamPageKind::Page,
            slug: "charter".to_string(),
            title: "Impostor charter".to_string(),
            content: String::new(),
            position: 9,
            is_protected: false,
            created_by: actor,
            updated_by: actor,
        })
        .execute(&mut conn);
    match duplicate_root {
        Err(DieselError::DatabaseError(DatabaseErrorKind::UniqueViolation, _)) => {}
        other => panic!("duplicate root slug must violate uniqueness, got {other:?}"),
    }

    // Moving a folder under itself or its own descendant is refused
    // (cycle prevention in rename_move_page).
    let tutorials = create_page(
        &mut conn,
        team.id,
        CreatePage {
            parent_id: Some(folder.id),
            kind: TeamPageKind::Folder,
            slug: "onboarding",
            title: "Onboarding",
            content: "",
            position: 0,
        },
        actor,
    )
    .expect("a nested folder");
    for bad_parent in [folder.id, tutorials.id] {
        match kairos_db::team_pages::rename_move_page(
            &mut conn,
            team.id,
            folder.id,
            None,
            Some(Some(bad_parent)),
            None,
            actor,
        ) {
            Err(TeamPageError::BadParent(_)) => {}
            other => panic!("cycle move must be BadParent, got {other:?}"),
        }
    }

    // ---- the migration removes the unused seeded folders -----------------------
    // A team from before KAIROS-T-0436, with the old seed. It used part of it:
    // it edited the Overview page and put a page in Reference. Those stay,
    // with their folders; the rest of the seed goes.
    let other: Team = diesel::insert_into(schema::teams::table)
        .values(NewTeam {
            name: "Web".to_string(),
            slug: "web".to_string(),
            team_type: TeamType::StreamAligned,
        })
        .returning(Team::as_returning())
        .get_result(&mut conn)
        .expect("creating second team");
    seed_team_charter(&mut conn, other.id, actor).expect("seeding second charter");
    let old_seed = |conn: &mut PgConnection,
                    parent: Option<Uuid>,
                    kind: TeamPageKind,
                    slug: &str,
                    position: i32| {
        diesel::insert_into(schema::team_pages::table)
            .values(NewTeamPage {
                team_id: other.id,
                parent_id: parent,
                kind,
                slug: slug.to_string(),
                title: slug.to_string(),
                content: String::new(),
                position,
                is_protected: false,
                created_by: actor,
                updated_by: actor,
            })
            .returning(TeamPage::as_returning())
            .get_result(conn)
            .expect("an old seed node")
    };
    let support = old_seed(
        &mut conn,
        None,
        TeamPageKind::Folder,
        "support-processes",
        1,
    );
    let overview = old_seed(
        &mut conn,
        Some(support.id),
        TeamPageKind::Page,
        "overview",
        0,
    );
    let docs = old_seed(&mut conn, None, TeamPageKind::Folder, "documentation", 2);
    let mut reference = None;
    for (position, slug) in [
        "planning",
        "tutorials",
        "how-to-guides",
        "reference",
        "explanation",
        "design-docs",
    ]
    .into_iter()
    .enumerate()
    {
        let section = old_seed(
            &mut conn,
            Some(docs.id),
            TeamPageKind::Folder,
            slug,
            position as i32,
        );
        if slug == "reference" {
            reference = Some(section.id);
        }
    }
    old_seed(&mut conn, reference, TeamPageKind::Page, "api", 0);
    kairos_db::team_pages::update_page_content(
        &mut conn,
        other.id,
        overview.id,
        None,
        "# Support\n\nPost in the channel.\n",
        overview.version,
        actor,
    )
    .expect("the team edits its Overview page");
    // The first team is new: its own folders are not the old seed, and stay.
    let platform_before = scaffold_shape(&mut conn, team.id);

    conn.batch_execute(SCAFFOLD_REMOVED)
        .expect("the up migration");
    conn.batch_execute(SCAFFOLD_REMOVED)
        .expect("the up migration runs twice");

    let row = |parent: &str, slug: &str, kind: &str, protected: bool| {
        (
            parent.to_string(),
            slug.to_string(),
            kind.to_string(),
            protected,
        )
    };
    assert_eq!(
        scaffold_shape(&mut conn, other.id),
        [
            row("", "charter", "page", true),
            row("", "documentation", "folder", false),
            row("", "support-processes", "folder", false),
            row("documentation", "reference", "folder", false),
            row("reference", "api", "page", false),
            row("support-processes", "overview", "page", false),
        ],
        "the edited Overview and the used Reference stay, with their folders"
    );
    assert_eq!(scaffold_shape(&mut conn, team.id), platform_before);

    drop(conn);
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin_conn)
        .expect("dropping scratch database after test");
}
