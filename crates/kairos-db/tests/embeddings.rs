//! The embedding store against real PostgreSQL and pgvector (KAIROS-T-0190).
//!
//! `kairos_db::embeddings` is raw SQL over a type diesel does not model, so its
//! unit tests can only cover the pure helpers. Everything that matters — that
//! the vectors actually land, that a re-embed replaces rather than duplicates,
//! that a shortened document does not leave orphaned chunks behind, that the
//! staleness counts mean what an operator will read them to mean — needs the
//! database.
//!
//! Runs against the compose stack (`angreal services up` /
//! `angreal test integration`), per KAIROS-A-0012: the database is never mocked.

#[path = "common/scratch.rs"]
mod scratch;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::BigInt;
use uuid::Uuid;

use kairos_db::embeddings::{
    ChunkWrite, EmbeddingError, StoredModel, counts, forget_item, metadata_for, pending_primary,
    pin_and_index, store_primary, stored_chunk_hashes, sync_chunks,
};
use kairos_db::items::{self, CreateInitiative, CreateTask};
use kairos_db::models::{
    BoardLevel, MetadataDefinition, NewItemMetadata, NewUser, RelationshipType, TaskType, WorkClass,
};
use kairos_db::{create_board, graph, provision_tenant, run_public_migrations, schema};

/// A team for the delivery board of a fixture (COLLIERY-T-0230): a row in
/// `teams` and nothing more. A delivery board always has a team, so a
/// fixture that makes a delivery board makes its team first.
fn seed_board_team(conn: &mut PgConnection, name: &str, slug: &str) -> Uuid {
    use kairos_db::schema::teams;
    diesel::insert_into(teams::table)
        .values(kairos_db::models::teams::NewTeam {
            name: name.to_string(),
            slug: slug.to_string(),
            team_type: kairos_db::models::TeamType::StreamAligned,
        })
        .returning(teams::id)
        .get_result(conn)
        .unwrap_or_else(|e| panic!("inserting team {slug:?}: {e}"))
}

const DEFAULT_DATABASE_URL: &str = "postgres://kairos:kairos@localhost:41432/kairos";
static SCRATCH_DB: scratch::ScratchName = scratch::ScratchName::new("kairos_embeddings_test");

/// The width the local model produces (KAIROS-T-0189), so the fixtures exercise
/// the real shape rather than a toy one.
const DIM: usize = 384;

fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database path segment");
    format!("{base}/{}", scratch::scratch_db_name(db_name))
}

/// A deterministic unit vector, so assertions can be exact.
fn unit(seed: u32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[(seed as usize) % DIM] = 1.0;
    v
}

fn model() -> StoredModel {
    StoredModel {
        provider: "deterministic".into(),
        model: "sha256-384".into(),
        dimension: DIM,
    }
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

fn count_rows(conn: &mut PgConnection, sql: &str) -> i64 {
    sql_query(sql)
        .get_result::<Count>(conn)
        .expect("count query")
        .count
}

#[test]
fn embedding_store_lifecycle() {
    let admin_url = admin_database_url();
    let mut admin = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
        panic!("cannot connect to compose postgres at {admin_url}: {e} (is the stack up?)")
    });
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin)
        .expect("dropping scratch database");
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin)
        .expect("creating scratch database");
    let _scratch_guard = scratch::ScratchGuard::new(&SCRATCH_DB);

    let url = with_database(&admin_url, &SCRATCH_DB);
    let mut conn = PgConnection::establish(&url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");

    let alice = diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: "dex|alice".into(),
            user_name: "dex|alice".into(),
            email: "alice@acme.test".into(),
            display_name: "Alice".into(),
        })
        .returning(schema::users::id)
        .get_result::<Uuid>(&mut conn)
        .expect("inserting alice");

    // COLLIERY-T-0230: a delivery board always has a team.
    let delivery_team = seed_board_team(&mut conn, "Delivery Team", "delivery-team");
    let board = create_board(
        &mut conn,
        BoardLevel::Delivery,
        "Delivery",
        "delivery",
        kairos_db::CodePrefix::FromSlug,
        Some(delivery_team),
        Some(alice),
    )
    .expect("creating the delivery board")
    .id;

    let make_task = |conn: &mut PgConnection, title: &str, content: &str| {
        items::create_task(
            conn,
            CreateTask {
                board_id: board,
                column_id: None,
                title,
                content,
                task_type: TaskType::Task,
                work_class: WorkClass::Planned,
                repository_id: None,
            },
            alice,
        )
        .expect("creating a task")
    };

    let one = make_task(&mut conn, "Refund rounding", "Rounded before tax.");
    let two = make_task(&mut conn, "Dark mode toggle", "On the settings page.");

    // ---- nothing embedded yet ---------------------------------------------
    let m = model();
    let c = counts(&mut conn, &m).expect("counts");
    assert_eq!(c.items, 2, "two live items");
    assert_eq!(c.embedded, 0);
    assert_eq!(c.missing(), 2, "both are behind");
    assert_eq!(c.wrong_model, 0);

    let pending = pending_primary(&mut conn, &m, 100, 0).expect("pending");
    assert_eq!(pending.len(), 2);
    assert!(
        pending.iter().all(|p| p.stored_hash.is_none()),
        "never embedded means no stored hash"
    );
    assert!(
        pending
            .iter()
            .any(|p| p.title == "Refund rounding" && p.content == "Rounded before tax."),
        "the text to compose from comes back with the row"
    );
    assert_eq!(
        pending[0].short_code.cmp(&pending[1].short_code),
        std::cmp::Ordering::Less,
        "ordered by short_code, so a resumable backfill walks the same sequence"
    );

    // ---- rule 4's structural inputs come back with the row ----------------
    // Composing the primary text needs more than title and content, and none of
    // it is in `searchable_items`, so `pending_primary` joins it. Asserted here
    // because a silently-NULL parent would not fail anything — it would just
    // quietly make every child item less findable.
    let initiative_board = create_board(
        &mut conn,
        BoardLevel::Initiative,
        "Initiatives",
        "initiatives-embed",
        kairos_db::CodePrefix::FromSlug,
        None,
        Some(alice),
    )
    .expect("creating the initiative board")
    .id;
    let parent = items::create_initiative(
        &mut conn,
        CreateInitiative {
            board_id: initiative_board,
            column_id: None,
            title: "Billing correctness",
            content: "The umbrella.",
            complexity: None,
            bucket_type: None,
        },
        alice,
    )
    .expect("creating the parent initiative");
    graph::link_items(
        &mut conn,
        parent.id,
        one.id,
        RelationshipType::Parent,
        alice,
    )
    .expect("parenting the task");

    // `priority` is seeded as a system default at provisioning, so look it up
    // rather than creating it — and using the real one is the more honest
    // fixture anyway.
    let definition = schema::metadata_definitions::table
        .filter(schema::metadata_definitions::slug.eq("priority"))
        .select(MetadataDefinition::as_select())
        .first(&mut conn)
        .expect("the seeded priority definition");
    let definition_name = definition.name.clone();
    let definition = definition.id;
    diesel::insert_into(schema::item_metadata::table)
        .values(NewItemMetadata {
            item_id: one.id,
            metadata_definition_id: definition,
            value: "high".into(),
        })
        .execute(&mut conn)
        .expect("stamping metadata");

    let pending = pending_primary(&mut conn, &m, 100, 0).expect("pending");
    let enriched = pending
        .iter()
        .find(|p| p.id == one.id)
        .expect("the child task");
    assert_eq!(
        enriched.parent_title.as_deref(),
        Some("Billing correctness"),
        "the parent's title travels with the row"
    );
    // NULL rather than missing: a document has no repository, and composing a
    // blank `repository:` line would put one token into every document alike.
    assert_eq!(enriched.repository, None, "this task has no repository");
    // No team travels with the row (COLLIERY-T-1840): the probe has no
    // `team:` line any more, because one team on every task made it the same
    // line in every probe.

    let metadata = metadata_for(&mut conn, &[one.id, two.id]).expect("metadata");
    assert_eq!(
        metadata,
        vec![(one.id, definition_name, "high".to_string())],
        "the LABEL is the definition's name, which is what a tenant wrote down"
    );
    assert!(
        metadata_for(&mut conn, &[])
            .expect("empty batch")
            .is_empty(),
        "an empty batch is not a round trip"
    );

    // ---- store one primary vector -----------------------------------------
    store_primary(&mut conn, one.id, "task", &unit(1), &m, "hash-one").expect("store primary");
    let c = counts(&mut conn, &m).expect("counts");
    assert_eq!(c.embedded, 1);
    // Three live items now — the parent initiative created above is itself an
    // item, and counts the same as any other.
    assert_eq!(c.items, 3);
    assert_eq!(c.missing(), 2);

    let pending = pending_primary(&mut conn, &m, 100, 0).expect("pending");
    let stored = pending
        .iter()
        .find(|p| p.id == one.id)
        .expect("the embedded item is still listed");
    assert_eq!(
        stored.stored_hash.as_deref(),
        Some("hash-one"),
        "so the caller can compare it against what the text hashes to NOW"
    );

    // ---- re-embedding replaces rather than duplicating --------------------
    store_primary(&mut conn, one.id, "task", &unit(2), &m, "hash-one-v2").expect("re-embed");
    assert_eq!(
        count_rows(
            &mut conn,
            "SELECT count(*)::bigint AS count FROM item_embeddings"
        ),
        1,
        "upsert, not a second row"
    );
    let pending = pending_primary(&mut conn, &m, 100, 0).expect("pending");
    assert_eq!(
        pending
            .iter()
            .find(|p| p.id == one.id)
            .unwrap()
            .stored_hash
            .as_deref(),
        Some("hash-one-v2"),
        "the hash moved with the vector"
    );

    // ---- a vector that lies about its width is refused --------------------
    let err = store_primary(&mut conn, two.id, "task", &[0.0, 1.0], &m, "short")
        .expect_err("a 2-wide vector declared as 384 must be refused");
    assert!(matches!(
        err,
        EmbeddingError::DimensionMismatch {
            got: 2,
            declared: 384
        }
    ));
    assert_eq!(
        count_rows(
            &mut conn,
            "SELECT count(*)::bigint AS count FROM item_embeddings"
        ),
        1,
        "and nothing was written"
    );

    // ---- chunks -----------------------------------------------------------
    let v0 = unit(10);
    let v1 = unit(11);
    let v2 = unit(12);
    let chunks = vec![
        ChunkWrite {
            ordinal: 0,
            heading: Some("Objective"),
            char_start: 0,
            char_end: 10,
            text: "first part",
            content_hash: "c0",
            vector: Some(&v0),
        },
        ChunkWrite {
            ordinal: 1,
            heading: None,
            char_start: 10,
            char_end: 21,
            text: "second part",
            content_hash: "c1",
            vector: Some(&v1),
        },
        ChunkWrite {
            ordinal: 2,
            heading: Some("Status Updates"),
            char_start: 21,
            char_end: 31,
            text: "third part",
            content_hash: "c2",
            vector: Some(&v2),
        },
    ];
    sync_chunks(&mut conn, one.id, "task", &chunks, &m).expect("storing chunks");
    assert_eq!(
        stored_chunk_hashes(&mut conn, one.id, &m).expect("hashes"),
        vec![
            (0, "c0".to_string()),
            (1, "c1".to_string()),
            (2, "c2".to_string())
        ],
        "ordered by ordinal, so a caller can compare position for position"
    );
    // A heading is nullable on purpose: a sliding-window chunk has none.
    assert_eq!(
        count_rows(
            &mut conn,
            "SELECT count(*)::bigint AS count FROM item_chunks WHERE heading IS NULL"
        ),
        1
    );

    // ---- an unchanged chunk is NOT rewritten ------------------------------
    // The cost argument for chunking at all: appending to one section of a
    // nine-section document must cost one embedding, not nine. A caller that has
    // compared hashes passes `None` for the chunks that did not move, and this
    // asserts the stored rows really are left alone rather than rewritten with
    // identical content — `updated_at` is the witness.
    #[derive(QueryableByName)]
    struct Stamp {
        #[diesel(sql_type = diesel::sql_types::Timestamptz)]
        updated_at: chrono::DateTime<chrono::Utc>,
    }
    let stamps = |conn: &mut PgConnection| -> Vec<chrono::DateTime<chrono::Utc>> {
        sql_query("SELECT updated_at FROM item_chunks WHERE item_id = $1 ORDER BY ordinal")
            .bind::<diesel::sql_types::Uuid, _>(one.id)
            .load::<Stamp>(conn)
            .expect("stamps")
            .into_iter()
            .map(|s| s.updated_at)
            .collect()
    };
    let before = stamps(&mut conn);
    let v1_new = unit(99);
    let appended = vec![
        ChunkWrite {
            ordinal: 0,
            heading: Some("Objective"),
            char_start: 0,
            char_end: 10,
            text: "first part",
            content_hash: "c0",
            vector: None, // unchanged
        },
        ChunkWrite {
            ordinal: 1,
            heading: None,
            char_start: 10,
            char_end: 24,
            text: "second part v2",
            content_hash: "c1-v2",
            vector: Some(&v1_new), // the section that was appended to
        },
        ChunkWrite {
            ordinal: 2,
            heading: Some("Status Updates"),
            char_start: 24,
            char_end: 34,
            text: "third part",
            content_hash: "c2",
            vector: None, // unchanged
        },
    ];
    sync_chunks(&mut conn, one.id, "task", &appended, &m).expect("incremental sync");
    let after = stamps(&mut conn);
    assert_eq!(after.len(), 3);
    assert_eq!(after[0], before[0], "chunk 0 was not touched");
    assert!(after[1] > before[1], "chunk 1 was rewritten");
    assert_eq!(after[2], before[2], "chunk 2 was not touched");
    assert_eq!(
        stored_chunk_hashes(&mut conn, one.id, &m).expect("hashes"),
        vec![
            (0, "c0".to_string()),
            (1, "c1-v2".to_string()),
            (2, "c2".to_string())
        ],
        "and only the changed chunk's hash moved"
    );

    // ---- a shortened document must not leave orphans ----------------------
    // This is why chunks are replaced rather than upserted: ordinals are
    // positions, not identities, so an upsert keyed on (item_id, ordinal) would
    // leave the old tail behind, still matching queries, forever.
    let shorter = vec![ChunkWrite {
        ordinal: 0,
        heading: Some("Objective"),
        char_start: 0,
        char_end: 12,
        text: "rewritten",
        content_hash: "c0-v2",
        vector: Some(&v0),
    }];
    sync_chunks(&mut conn, one.id, "task", &shorter, &m).expect("replacing chunks");
    assert_eq!(
        stored_chunk_hashes(&mut conn, one.id, &m).expect("hashes"),
        vec![(0, "c0-v2".to_string())],
        "the tail is gone, not orphaned"
    );

    // ---- pgvector really stored vectors, not text that looks like them ----
    let distance = count_rows(
        &mut conn,
        "SELECT count(*)::bigint AS count FROM item_embeddings \
         WHERE (embedding <=> embedding) < 0.000001",
    );
    assert_eq!(
        distance, 1,
        "the cosine operator works on the stored column"
    );

    // ---- a model change is visible, not silent ----------------------------
    let other = StoredModel {
        provider: "local".into(),
        model: "bge-small-en-v1.5-q".into(),
        dimension: DIM,
    };
    let c = counts(&mut conn, &other).expect("counts under a different model");
    assert_eq!(
        c.embedded, 0,
        "vectors from another model do not count as embedded"
    );
    assert_eq!(
        c.wrong_model, 1,
        "they count as wrong-model, which is what an operator needs to see"
    );
    let pending = pending_primary(&mut conn, &other, 100, 0).expect("pending");
    assert!(
        pending.iter().all(|p| p.stored_hash.is_none()),
        "and every item reads as never embedded under the new model, so a \
         backfill re-does them rather than trusting a hash from another space"
    );

    // ---- archived items keep their vectors --------------------------------
    // KAIROS-A-0020 made put-away work searchable, and prior art in completed
    // work is one of the three claims retrieval exists to make.
    items::soft_delete_item(
        &mut conn,
        kairos_core::short_code::ItemType::Task,
        one.id,
        alice,
    )
    .expect("archiving");
    assert_eq!(
        count_rows(
            &mut conn,
            "SELECT count(*)::bigint AS count FROM item_embeddings"
        ),
        1,
        "archiving does not drop the vector"
    );
    let c = counts(&mut conn, &m).expect("counts after archiving");
    assert_eq!(
        c.items, 3,
        "and it is still counted: archived items carry vectors too, because \
         prior art in finished work is one of the claims retrieval makes"
    );

    // ---- an archived parent still names its child (COLLIERY-T-1840) ------
    // 931 of 1,370 parent edges of the colliery tenant end at an archived
    // parent. Its title is the context of the child, so it stays.
    items::soft_delete_item(
        &mut conn,
        kairos_core::short_code::ItemType::Initiative,
        parent.id,
        alice,
    )
    .expect("archiving the parent");
    let pending = pending_primary(&mut conn, &m, 100, 0).expect("pending");
    assert_eq!(
        pending
            .iter()
            .find(|p| p.id == one.id)
            .expect("the archived child is still listed")
            .parent_title
            .as_deref(),
        Some("Billing correctness"),
        "the title of an archived parent travels with the row"
    );

    // ---- forgetting an item removes both tables ---------------------------
    forget_item(&mut conn, one.id).expect("forget");
    assert_eq!(
        count_rows(
            &mut conn,
            "SELECT count(*)::bigint AS count FROM item_embeddings"
        ),
        0
    );
    assert_eq!(
        count_rows(
            &mut conn,
            "SELECT count(*)::bigint AS count FROM item_chunks"
        ),
        0
    );
    // Idempotent: a delete path that runs twice must not fail.
    forget_item(&mut conn, one.id).expect("forget again");

    // ---- pinning and indexing ---------------------------------------------
    // pgvector refuses to index a column of unspecified width, so the columns
    // have to be pinned first — but the width is a DEPLOYMENT fact (384 here,
    // 1536 for OpenAI), which is why this is a command rather than a migration.
    store_primary(&mut conn, two.id, "task", &unit(7), &m, "hash-two").expect("a row to index");
    let report = pin_and_index(&mut conn, DIM).expect("pinning and indexing");
    assert!(report.pinned, "the columns were unpinned before this");
    assert_eq!(
        report.created,
        vec![
            "idx_item_embeddings_hnsw".to_string(),
            "idx_item_chunks_hnsw".to_string()
        ],
        "both tables get an index"
    );

    // Idempotent: the second run must not rebuild an index, which on a real
    // tenant would be a surprise outage rather than a no-op.
    let again = pin_and_index(&mut conn, DIM).expect("second run");
    assert!(!again.pinned, "already pinned");
    assert!(again.created.is_empty(), "already indexed");

    // The column really carries the width now, which is what the index needed.
    assert_eq!(
        count_rows(
            &mut conn,
            "SELECT count(*)::bigint AS count FROM information_schema.columns c \
             JOIN pg_attribute a ON a.attname = c.column_name \
             JOIN pg_class cl ON cl.oid = a.attrelid AND cl.relname = c.table_name \
             WHERE c.table_schema = 'org_acme' AND c.column_name = 'embedding' \
               AND a.atttypmod = 384"
        ),
        2,
        "both embedding columns are vector(384)"
    );

    // ---- and it refuses rather than converting ----------------------------
    // A row from a narrower model must stop the pin, not be silently dropped by
    // a half-finished ALTER.
    let narrow = StoredModel {
        provider: "deterministic".into(),
        model: "sha256-8".into(),
        dimension: 8,
    };
    store_primary(&mut conn, one.id, "task", &[0.5f32; 8], &narrow, "narrow")
        .expect_err("a vector narrower than the pinned column cannot be stored");

    // Pinning to a width the stored rows disagree with is refused up front.
    let err = pin_and_index(&mut conn, 8).expect_err("stored rows are 384-wide");
    assert!(matches!(
        err,
        EmbeddingError::MixedDimensions {
            table: "item_embeddings",
            wanted: 8,
            ..
        }
    ));
    assert!(
        err.to_string().contains("backfill-embeddings"),
        "and it says how to fix it: {err}"
    );
}

// ===========================================================================
// KAIROS-T-0636: the marks of the embedding sweep
// ===========================================================================

static MARKS_DB: scratch::ScratchName = scratch::ScratchName::new("kairos_embedding_marks_t0636");

/// The marked items, oldest first.
fn marked(conn: &mut PgConnection) -> Vec<kairos_db::embeddings::DirtyMark> {
    kairos_db::embeddings::dirty_page(conn, 1000).expect("the marks")
}

fn marked_ids(conn: &mut PgConnection) -> Vec<Uuid> {
    marked(conn).iter().map(|m| m.item_id).collect()
}

/// Clear every mark, as a run that read them all.
fn clear_all(conn: &mut PgConnection) {
    let marks = marked(conn);
    kairos_db::embeddings::clear_dirty(conn, &marks).expect("clear");
    assert!(marked(conn).is_empty());
}

/// Each write that changes the text of an item marks it in its transaction:
/// a create, an edit, a parent, a repository, a title of the parent, a new
/// repository slug. A clear deletes a mark only when no write came after
/// the read. `mark_missing` marks the items with no vector of the model.
#[test]
fn writes_mark_their_items_and_a_clear_keeps_a_newer_mark() {
    use kairos_core::short_code::ItemType;
    use kairos_db::embeddings::{
        DirtyMark, clear_dirty, mark_definition_dirty, mark_missing, mark_repository_dirty,
    };
    use kairos_db::models::Forge;
    use kairos_db::models::repositories::{NewRepository, RepositoryChangeset};

    let admin_url = admin_database_url();
    let mut admin = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
        panic!("cannot connect to compose postgres at {admin_url}: {e} (is the stack up?)")
    });
    sql_query(format!("DROP DATABASE IF EXISTS {MARKS_DB} WITH (FORCE)"))
        .execute(&mut admin)
        .expect("dropping scratch database");
    sql_query(format!("CREATE DATABASE {MARKS_DB}"))
        .execute(&mut admin)
        .expect("creating scratch database");
    let _scratch_guard = scratch::ScratchGuard::new(&MARKS_DB);
    let url = with_database(&admin_url, &MARKS_DB);
    let mut conn = PgConnection::establish(&url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");
    let robin = diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: "dex|robin".into(),
            user_name: "dex|robin".into(),
            email: "robin@acme.test".into(),
            display_name: "Robin".into(),
        })
        .returning(schema::users::id)
        .get_result::<Uuid>(&mut conn)
        .expect("robin");
    let team = seed_board_team(&mut conn, "Delivery Team", "delivery-team");
    let board = create_board(
        &mut conn,
        BoardLevel::Delivery,
        "Delivery",
        "delivery",
        kairos_db::CodePrefix::FromSlug,
        Some(team),
        Some(robin),
    )
    .expect("board")
    .id;
    let initiatives = create_board(
        &mut conn,
        BoardLevel::Initiative,
        "Initiatives of the marks",
        "initiatives-marks",
        kairos_db::CodePrefix::FromSlug,
        None,
        Some(robin),
    )
    .expect("initiative board")
    .id;

    // ---- a create marks the new item; the order is the order of the writes
    let parent = items::create_initiative(
        &mut conn,
        CreateInitiative {
            board_id: initiatives,
            column_id: None,
            title: "Billing correctness",
            content: "The umbrella.",
            complexity: None,
            bucket_type: None,
        },
        robin,
    )
    .expect("parent");
    let make_task = |conn: &mut PgConnection, title: &str| {
        items::create_task(
            conn,
            CreateTask {
                board_id: board,
                column_id: None,
                title,
                content: "## Objective\n\nThe work.\n",
                task_type: TaskType::Task,
                work_class: WorkClass::Planned,
                repository_id: None,
            },
            robin,
        )
        .expect("task")
    };
    let one = make_task(&mut conn, "Refund rounding");
    let two = make_task(&mut conn, "Dark mode toggle");
    assert_eq!(
        marked_ids(&mut conn),
        vec![parent.id, one.id, two.id],
        "each create marks its item, oldest first"
    );
    let first = marked(&mut conn);
    assert_eq!(
        clear_dirty(&mut conn, &first).expect("clear"),
        3,
        "a clear with the marks that it read deletes them"
    );
    assert!(marked(&mut conn).is_empty());

    // ---- a parent marks both ends ------------------------------------------
    graph::link_items(
        &mut conn,
        parent.id,
        one.id,
        RelationshipType::Parent,
        robin,
    )
    .expect("parent link");
    let mut ids = marked_ids(&mut conn);
    ids.sort();
    let mut both = vec![parent.id, one.id];
    both.sort();
    assert_eq!(ids, both, "the edge marks the parent and the child");
    clear_all(&mut conn);

    // ---- a new title of the parent marks its child, and only it ------------
    items::update_item_content(
        &mut conn,
        ItemType::Initiative,
        parent.id,
        items::ContentUpdate {
            new_title: Some("Billing correctness, the second phase"),
            new_content: "The umbrella.",
            expected_version: 1,
        },
        robin,
    )
    .expect("new title");
    let mut ids = marked_ids(&mut conn);
    ids.sort();
    assert_eq!(ids, both, "the parent and its child; not the other task");
    clear_all(&mut conn);

    // ---- an edit of the content marks the item only ------------------------
    items::update_item_content(
        &mut conn,
        ItemType::Task,
        two.id,
        items::ContentUpdate {
            new_title: None,
            new_content: "## Objective\n\nThe toggle.\n",
            expected_version: 1,
        },
        robin,
    )
    .expect("edit");
    let read = marked(&mut conn);
    assert_eq!(
        read.iter().map(|m| m.item_id).collect::<Vec<_>>(),
        vec![two.id]
    );

    // ---- a write after the read: the clear keeps the newer mark ------------
    items::update_item_content(
        &mut conn,
        ItemType::Task,
        two.id,
        items::ContentUpdate {
            new_title: None,
            new_content: "## Objective\n\nThe toggle, again.\n",
            expected_version: 2,
        },
        robin,
    )
    .expect("second edit");
    assert_eq!(
        clear_dirty(&mut conn, &read).expect("clear"),
        0,
        "the mark that the clear read is not the mark of the item now"
    );
    let now: Vec<DirtyMark> = marked(&mut conn);
    assert_eq!(now.len(), 1, "the item stays dirty");
    assert_eq!(now[0].item_id, two.id);
    assert_ne!(now[0].mark, read[0].mark, "the write gave a new mark");
    clear_all(&mut conn);

    // ---- a repository: the link marks the task, a new slug marks its tasks
    let repository = kairos_db::repositories::create(
        &mut conn,
        NewRepository {
            slug: "payments-api".to_string(),
            forge: Forge::Github,
            repo_full_name: "acme/payments-api".to_string(),
            repo_url: "https://github.com/acme/payments-api".to_string(),
            default_branch: "main".to_string(),
            team_id: team,
            description: String::new(),
            created_by: robin,
            updated_by: robin,
        },
    )
    .expect("repository");
    items::set_task_repository(&mut conn, one.id, Some(repository.id), robin).expect("the link");
    assert_eq!(
        marked_ids(&mut conn),
        vec![one.id],
        "the link marks the task"
    );
    clear_all(&mut conn);
    kairos_db::repositories::update(
        &mut conn,
        repository.id,
        RepositoryChangeset {
            description: Some("Same slug.".to_string()),
            ..Default::default()
        },
        robin,
    )
    .expect("a change with the same slug");
    assert!(
        marked(&mut conn).is_empty(),
        "the slug did not change: no mark"
    );
    kairos_db::repositories::update(
        &mut conn,
        repository.id,
        RepositoryChangeset {
            slug: Some("payments".to_string()),
            ..Default::default()
        },
        robin,
    )
    .expect("a new slug");
    assert_eq!(
        marked_ids(&mut conn),
        vec![one.id],
        "a new slug marks the tasks of the repository"
    );
    clear_all(&mut conn);
    assert_eq!(
        mark_repository_dirty(&mut conn, repository.id).expect("mark"),
        1
    );
    clear_all(&mut conn);

    // ---- the items with a value of a definition ----------------------------
    let definition = schema::metadata_definitions::table
        .filter(schema::metadata_definitions::slug.eq("priority"))
        .select(MetadataDefinition::as_select())
        .first(&mut conn)
        .expect("the seeded priority definition")
        .id;
    diesel::insert_into(schema::item_metadata::table)
        .values(NewItemMetadata {
            item_id: two.id,
            metadata_definition_id: definition,
            value: "high".into(),
        })
        .execute(&mut conn)
        .expect("stamping metadata");
    assert_eq!(
        mark_definition_dirty(&mut conn, definition).expect("mark"),
        1
    );
    assert_eq!(marked_ids(&mut conn), vec![two.id]);
    clear_all(&mut conn);

    // ---- the items with no vector of the model ----------------------------
    let m = model();
    assert_eq!(mark_missing(&mut conn, &m).expect("missing"), 3);
    clear_all(&mut conn);
    store_primary(&mut conn, one.id, "task", &unit(1), &m, "hash-one").expect("store");
    assert_eq!(
        mark_missing(&mut conn, &m).expect("missing"),
        2,
        "the item with a vector of the model gets no mark"
    );
    let other = StoredModel {
        model: "another-model".into(),
        ..model()
    };
    clear_all(&mut conn);
    assert_eq!(
        mark_missing(&mut conn, &other).expect("missing"),
        3,
        "a vector of another model counts as none"
    );

    // ---- forget_item takes the mark too ------------------------------------
    forget_item(&mut conn, two.id).expect("forget");
    assert!(!marked_ids(&mut conn).contains(&two.id));
}
