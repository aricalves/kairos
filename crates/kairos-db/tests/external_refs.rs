//! KAIROS-T-0508 (COLLIERY-I-0630) — the external references of an item,
//! in the database layer:
//!
//! - A task, an initiative and a document can link to a URI with a label.
//! - The scheme rule: `javascript:` is refused and named; nothing is
//!   written.
//! - A second add of one URI is a typed error; a remove of a URI that the
//!   item does not have is a typed error. Each write makes an activity
//!   row and nothing else does.
//! - An archived item keeps its references, and gets no new one.
//! - `refs_of_items` gives the references of many items in one query.
//! - The migration: the down.sql removes the table; the up.sql makes it
//!   again, and a second run changes nothing.
//!
//! Against real Postgres from the compose stack (A-0012 tier 2). The test
//! owns a scratch database.

#[path = "common/scratch.rs"]
mod scratch;

use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Text, Uuid as SqlUuid};
use uuid::Uuid;

use kairos_core::external_ref::ExternalRefError as UriError;
use kairos_core::short_code::ItemType;
use kairos_db::external_refs::{self, ExternalRefError};
use kairos_db::items::{self, CreateDocument, CreateInitiative, CreateTask};
use kairos_db::models::enums::WorkClass;
use kairos_db::models::teams::NewTeam;
use kairos_db::models::{BoardLevel, NewUser, TaskType, TeamType, User};
use kairos_db::{
    CodePrefix, create_board, provision_tenant, run_public_migrations, soft_delete_item,
};

const UP: &str = include_str!("../migrations/tenant/2026-10-15-003100_item_external_refs/up.sql");
const DOWN: &str =
    include_str!("../migrations/tenant/2026-10-15-003100_item_external_refs/down.sql");

const DEFAULT_DATABASE_URL: &str = "postgres://kairos:kairos@localhost:41432/kairos";

const RFC: &str = "https://example.com/wiki/rfc-42";

fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("DATABASE_URL must contain a database path segment");
    format!("{base}/{}", scratch::scratch_db_name(db_name))
}

struct Scratch {
    admin: PgConnection,
    name: &'static str,
    _scratch_guard: scratch::ScratchGuard,
    conn: PgConnection,
    user: Uuid,
}

impl Scratch {
    fn new(name: &'static str) -> Self {
        let name: &'static str = Box::leak(scratch::scratch_db_name(name).into_boxed_str());
        let admin_url = admin_database_url();
        let mut admin = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
            panic!(
                "cannot connect to compose postgres at {admin_url}: {e} \
                 (is the stack up? `angreal services up`)"
            )
        });
        sql_query(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .execute(&mut admin)
            .expect("dropping scratch database");
        sql_query(format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .expect("creating scratch database");
        let mut conn = PgConnection::establish(&with_database(&admin_url, name))
            .expect("connecting to scratch database");
        run_public_migrations(&mut conn).expect("running public migrations");
        provision_tenant(&mut conn, "colliery", "Colliery").expect("provisioning colliery");
        sql_query("SET search_path TO org_colliery, public")
            .execute(&mut conn)
            .expect("pinning search_path");
        let user = {
            use kairos_db::schema::users;
            diesel::insert_into(users::table)
                .values(NewUser {
                    external_id: "dex|robin".into(),
                    user_name: "dex|robin".into(),
                    email: "robin@colliery.test".into(),
                    display_name: "Robin".into(),
                })
                .returning(User::as_returning())
                .get_result(&mut conn)
                .expect("inserting user")
                .id
        };
        Scratch {
            admin,
            name,
            _scratch_guard: scratch::ScratchGuard::new(name),
            conn,
            user,
        }
    }

    fn drop_database(mut self) {
        drop(self.conn);
        sql_query(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ))
        .execute(&mut self.admin)
        .expect("dropping scratch database");
    }
}

fn board_id(conn: &mut PgConnection, slug: &str) -> Uuid {
    use kairos_db::schema::boards::dsl;
    dsl::boards
        .filter(dsl::slug.eq(slug))
        .filter(dsl::deleted_at.is_null())
        .select(dsl::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("the board {slug}: {e}"))
}

fn delivery_board(conn: &mut PgConnection, user: Uuid) -> Uuid {
    use kairos_db::schema::teams;
    let team: Uuid = diesel::insert_into(teams::table)
        .values(NewTeam {
            name: "Team skadi".into(),
            slug: "skadi".into(),
            team_type: TeamType::StreamAligned,
        })
        .returning(teams::id)
        .get_result(conn)
        .expect("inserting team");
    create_board(
        conn,
        BoardLevel::Delivery,
        "skadi",
        "skadi",
        CodePrefix::Given("SKADI"),
        Some(team),
        Some(user),
    )
    .expect("the delivery board")
    .id
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

fn activity_rows(conn: &mut PgConnection, item: Uuid, details: &str) -> i64 {
    sql_query(
        "SELECT count(*) AS count FROM activity_log \
          WHERE entity_id = $1 AND details = $2",
    )
    .bind::<SqlUuid, _>(item)
    .bind::<Text, _>(details)
    .get_result::<CountRow>(conn)
    .expect("counting activity")
    .count
}

fn row_count(conn: &mut PgConnection) -> i64 {
    sql_query("SELECT count(*) AS count FROM item_external_refs")
        .get_result::<CountRow>(conn)
        .expect("counting references")
        .count
}

#[test]
fn an_item_links_to_a_uri_with_a_label() {
    let mut scratch = Scratch::new("kairos_external_refs_t0508_test");
    let user = scratch.user;
    let conn = &mut scratch.conn;
    let skadi = delivery_board(conn, user);
    let task = items::create_task(
        conn,
        CreateTask {
            board_id: skadi,
            column_id: None,
            title: "Policy engine",
            content: "",
            task_type: TaskType::Task,
            work_class: WorkClass::Planned,
            repository_id: None,
        },
        user,
    )
    .expect("creating a task");
    let initiatives = board_id(conn, "initiatives");
    let initiative = items::create_initiative(
        conn,
        CreateInitiative {
            board_id: initiatives,
            column_id: None,
            title: "Zen",
            content: "",
            complexity: None,
            bucket_type: None,
        },
        user,
    )
    .expect("creating an initiative")
    .id;
    let document = items::create_document(
        conn,
        CreateDocument {
            board_id: skadi,
            title: "Vision",
            content: Some(""),
            template_id: None,
        },
        user,
    )
    .expect("creating a document")
    .id;

    // --- link, with and without a label; the scheme in lower case --------
    let made = external_refs::link(
        conn,
        task.id,
        "  HTTPS://example.com/wiki/rfc-42 ",
        Some(" RFC: zen as the policy engine "),
        user,
    )
    .expect("linking the task");
    assert_eq!(made.uri, RFC);
    assert_eq!(made.label.as_deref(), Some("RFC: zen as the policy engine"));
    assert_eq!(made.display_text(), "RFC: zen as the policy engine");
    let bare = external_refs::link(conn, task.id, "mailto:robin@example.com", Some("  "), user)
        .expect("linking with no label");
    assert_eq!(bare.label, None);
    assert_eq!(bare.display_text(), "mailto:robin@example.com");
    external_refs::link(conn, initiative, RFC, Some("The RFC"), user)
        .expect("the initiative links to the same URI");
    external_refs::link(conn, document, "http://example.com/design", None, user)
        .expect("the document links");
    assert_eq!(row_count(conn), 4);

    let refs = external_refs::refs_of(conn, task.id).expect("the references of the task");
    assert_eq!(
        refs.iter().map(|r| r.uri.as_str()).collect::<Vec<_>>(),
        [RFC, "mailto:robin@example.com"],
        "oldest first"
    );
    assert_eq!(
        activity_rows(
            conn,
            task.id,
            &format!("relationship:external:{}->uri:{RFC}", task.short_code)
        ),
        1
    );

    // --- the scheme rule, before any write --------------------------------
    for (uri, scheme) in [
        ("javascript:alert(1)", "javascript"),
        ("data:text/html,x", "data"),
        ("ftp://example.com/x", "ftp"),
    ] {
        match external_refs::link(conn, task.id, uri, None, user) {
            Err(ExternalRefError::Form(UriError::Scheme { scheme: got })) => {
                assert_eq!(got, scheme)
            }
            other => panic!("{uri}: {other:?}"),
        }
    }
    assert!(matches!(
        external_refs::link(conn, task.id, "example.com/x", None, user),
        Err(ExternalRefError::Form(UriError::NoScheme { .. }))
    ));
    assert!(matches!(
        external_refs::link(conn, task.id, RFC, Some(&"x".repeat(201)), user),
        Err(ExternalRefError::Form(UriError::LabelTooLong))
    ));
    assert_eq!(row_count(conn), 4, "a refused URI writes nothing");

    // --- duplicates and a remove of nothing ------------------------------
    match external_refs::link(
        conn,
        task.id,
        "https://example.com/wiki/rfc-42",
        Some("x"),
        user,
    ) {
        Err(ExternalRefError::AlreadyLinked { short_code, uri }) => {
            assert_eq!(short_code, task.short_code);
            assert_eq!(uri, RFC);
        }
        other => panic!("{other:?}"),
    }
    match external_refs::unlink(conn, task.id, "https://example.com/other", user) {
        Err(ExternalRefError::NotLinked { uri, .. }) => {
            assert_eq!(uri, "https://example.com/other")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        external_refs::unlink(conn, task.id, "javascript:x", user)
            .unwrap_err()
            .to_string(),
        "Kairos does not permit the scheme \"javascript\". The schemes are: http, https, mailto."
    );
    assert_eq!(row_count(conn), 4);

    // --- an unknown item -------------------------------------------------
    assert!(matches!(
        external_refs::link(conn, Uuid::new_v4(), RFC, None, user),
        Err(ExternalRefError::ItemNotFound(_))
    ));

    // --- many items in one query -----------------------------------------
    let by_item =
        external_refs::refs_of_items(conn, &[task.id, initiative, document, Uuid::new_v4()])
            .expect("the references of many items");
    assert_eq!(by_item.len(), 3);
    assert_eq!(by_item[&task.id].len(), 2);
    assert_eq!(by_item[&initiative][0].label.as_deref(), Some("The RFC"));
    assert_eq!(by_item[&document][0].uri, "http://example.com/design");
    assert!(
        external_refs::refs_of_items(conn, &[])
            .expect("empty")
            .is_empty()
    );

    // --- by id -------------------------------------------------------------
    assert_eq!(
        external_refs::by_id(conn, task.id, made.id).expect("by id"),
        Some(made.clone())
    );
    assert_eq!(
        external_refs::by_id(conn, initiative, made.id).expect("by id"),
        None,
        "the id is read with the item"
    );

    // --- unlink, with the scheme in upper case ----------------------------
    let removed =
        external_refs::unlink(conn, task.id, "MAILTO:robin@example.com", user).expect("unlinking");
    assert_eq!(removed.id, bare.id);
    assert_eq!(
        activity_rows(
            conn,
            task.id,
            &format!(
                "relationship:external:{}->uri:mailto:robin@example.com",
                task.short_code
            )
        ),
        2,
        "one add and one remove"
    );
    assert_eq!(
        external_refs::refs_of(conn, task.id).expect("refs").len(),
        1
    );

    // --- an archived item keeps its references, and gets no new one -------
    soft_delete_item(conn, ItemType::Task, task.id, user).expect("archiving the task");
    assert_eq!(
        external_refs::refs_of(conn, task.id).expect("refs").len(),
        1,
        "the reference stays"
    );
    assert!(matches!(
        external_refs::link(conn, task.id, "https://example.com/new", None, user),
        Err(ExternalRefError::ItemNotFound(_))
    ));

    // --- the migration, down and up ----------------------------------------
    conn.batch_execute(DOWN).expect("down.sql");
    let exists = || -> bool {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = BigInt)]
            count: i64,
        }
        0 < sql_query(
            "SELECT count(*) AS count FROM information_schema.tables \
              WHERE table_schema = 'org_colliery' AND table_name = 'item_external_refs'",
        )
        .get_result::<Row>(
            &mut PgConnection::establish(&with_database(
                &admin_database_url(),
                "kairos_external_refs_t0508_test",
            ))
            .expect("second connection"),
        )
        .expect("information_schema")
        .count
    };
    assert!(!exists(), "down.sql removes the table");
    conn.batch_execute(UP).expect("up.sql");
    conn.batch_execute(UP).expect("up.sql, second run");
    assert!(exists(), "up.sql makes the table again");
    assert_eq!(row_count(conn), 0);

    scratch.drop_database();
}
