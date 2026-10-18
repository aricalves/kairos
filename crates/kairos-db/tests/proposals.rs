//! Edge proposals against a real database (KAIROS-T-0192).
//!
//! The pure helpers are unit-tested in `kairos_db::proposals`. What needs a
//! database is everything that matters: that confirming really creates the edge
//! through the **existing** graph path (and so inherits its cycle check), that a
//! rejection is kept rather than erased, that an agent cannot bury the signal
//! under its own output, and that a decided proposal cannot be decided twice.
//! And who decides (KAIROS-T-0659): a holder of `approve_proposals` on the
//! board of one end, or an org admin.

#[path = "common/scratch.rs"]
mod scratch;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use uuid::Uuid;

use kairos_db::graph::GraphError;
use kairos_db::models::{BoardLevel, NewUser, RelationshipType, TaskType, WorkClass};
use kairos_db::proposals::{
    Decider, MAX_PENDING_PER_ITEM, NewProposal, ProposalError, confirm, pending_for_item, propose,
    reject, stats,
};
use kairos_db::{abac, create_board, items, provision_tenant, run_public_migrations, schema};

/// The slug of the organization of each test here.
const ORG: &str = "acme";

/// A caller who is not an org admin.
fn member(user_id: Uuid) -> Decider {
    Decider {
        user_id,
        is_org_admin: false,
    }
}

/// An org admin. The server reads the role from the context of the
/// request (KAIROS-T-0625).
fn org_admin(user_id: Uuid) -> Decider {
    Decider {
        user_id,
        is_org_admin: true,
    }
}

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
static SCRATCH_DB: scratch::ScratchName = scratch::ScratchName::new("kairos_proposals_test");

fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn with_database(url: &str, db: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database path segment");
    format!("{base}/{}", scratch::scratch_db_name(db))
}

#[test]
fn edge_proposal_lifecycle() {
    let admin_url = admin_database_url();
    let mut admin = PgConnection::establish(&admin_url)
        .unwrap_or_else(|e| panic!("cannot connect to compose postgres at {admin_url}: {e}"));
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin)
        .expect("dropping scratch");
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin)
        .expect("creating scratch");
    let _scratch_guard = scratch::ScratchGuard::new(&SCRATCH_DB);
    let url = with_database(&admin_url, &SCRATCH_DB);

    let mut conn = PgConnection::establish(&url).expect("connecting");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning");

    let alice = diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: "dex|alice".into(),
            user_name: "dex|alice".into(),
            email: "alice@acme.test".into(),
            display_name: "Alice".into(),
        })
        .returning(schema::users::id)
        .get_result::<Uuid>(&mut conn)
        .expect("alice");
    let bob = diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: "dex|bob".into(),
            user_name: "dex|bob".into(),
            email: "bob@acme.test".into(),
            display_name: "Bob".into(),
        })
        .returning(schema::users::id)
        .get_result::<Uuid>(&mut conn)
        .expect("bob");
    let agent = diesel::insert_into(schema::users::table)
        .values(kairos_db::models::public::NewServiceAccountUser {
            external_id: "svc|agent".into(),
            user_name: "svc|agent".into(),
            email: "agent@acme.test".into(),
            display_name: "Agent".into(),
            kind: kairos_db::models::public::USER_KIND_SERVICE_ACCOUNT.into(),
        })
        .returning(schema::users::id)
        .get_result::<Uuid>(&mut conn)
        .expect("agent");

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
    .expect("board")
    .id;
    // KAIROS-T-0659: alice and bob decide the proposals of this board.
    for holder in [alice, bob] {
        abac::grant_capability(
            &mut conn,
            board,
            holder,
            kairos_core::abac::APPROVE_PROPOSALS,
            alice,
        )
        .expect("the grant");
    }
    let task = |conn: &mut PgConnection, title: &str| {
        items::create_task(
            conn,
            items::CreateTask {
                board_id: board,
                column_id: None,
                title,
                content: "…",
                task_type: TaskType::Task,
                work_class: WorkClass::Planned,
                repository_id: None,
            },
            alice,
        )
        .expect("task")
    };
    let a = task(&mut conn, "A");
    let b = task(&mut conn, "B");
    let c = task(&mut conn, "C");

    let evidence = |score: f32| NewProposal {
        source_id: a.id,
        target_id: b.id,
        relationship: RelationshipType::Blocks,
        claim: "possible dependency",
        why: "Reads as being about the same thing, and nothing in the graph joins them.",
        score,
    };

    // ---- propose -----------------------------------------------------------
    let p = propose(&mut conn, evidence(0.03), agent).expect("proposing");
    assert_eq!(p.state, "pending");
    assert_eq!(p.proposed_by, agent);
    assert!(p.decided_by.is_none() && p.decided_at.is_none());
    assert!(
        p.why.contains("nothing in the graph joins them"),
        "the evidence is kept verbatim, so a reviewer sees what the agent saw"
    );

    // Visible from BOTH ends: a proposal concerns two items, and a human looking
    // at either should see it.
    assert_eq!(pending_for_item(&mut conn, a.id).unwrap().len(), 1);
    assert_eq!(pending_for_item(&mut conn, b.id).unwrap().len(), 1);
    assert!(pending_for_item(&mut conn, c.id).unwrap().is_empty());

    // ---- an agent cannot bury the signal under its own output --------------
    let again = propose(&mut conn, evidence(0.9), agent);
    assert!(
        matches!(again, Err(ProposalError::AlreadyPending)),
        "re-proposing the same pair is work already done, not a second proposal"
    );

    // Only parent and blocks are proposable.
    let editorial = propose(
        &mut conn,
        NewProposal {
            relationship: RelationshipType::Supports,
            ..evidence(0.1)
        },
        agent,
    );
    assert!(matches!(editorial, Err(ProposalError::NotProposable(_))));

    // ---- an agent with no grant does not decide (KAIROS-T-0659) -----------
    // Enforced in the service, not in the surfaces: a rule implemented in two
    // handlers is a rule the third handler will not have.
    for refused in [
        confirm(&mut conn, p.id, member(agent)),
        reject(&mut conn, p.id, member(agent)),
    ] {
        assert!(
            matches!(refused, Err(ProposalError::NotApprover { .. })),
            "a service account with no grant decides nothing: {refused:?}"
        );
    }

    // ---- the proposer decides with the grant (KAIROS-T-0659) ---------------
    let by_alice = propose(
        &mut conn,
        NewProposal {
            target_id: c.id,
            ..evidence(0.2)
        },
        alice,
    )
    .expect("a person proposes");
    let own = confirm(&mut conn, by_alice.id, member(alice)).expect("the proposer confirms");
    assert_eq!(own.decided_by, Some(alice));
    // Two decisions of one proposal: the second one is refused.
    assert!(matches!(
        reject(&mut conn, by_alice.id, member(bob)),
        Err(ProposalError::AlreadyDecided { ref state, .. }) if state == "confirmed"
    ));

    // ---- reject is recorded, not erased ------------------------------------
    let rejected = reject(&mut conn, p.id, member(alice)).expect("rejecting");
    assert_eq!(rejected.state, "rejected");
    assert_eq!(rejected.decided_by, Some(alice));
    assert!(rejected.decided_at.is_some());
    assert!(
        pending_for_item(&mut conn, a.id).unwrap().is_empty(),
        "no longer pending"
    );
    let s = stats(&mut conn).expect("stats");
    assert_eq!((s.pending, s.confirmed, s.rejected), (0, 1, 1));
    assert_eq!(s.confirm_rate(), Some(0.5));

    // A rejected pair CAN be proposed again — a rejection judges the evidence at
    // the time, and better evidence deserves another hearing.
    let second = propose(&mut conn, evidence(0.9), agent).expect("re-proposing after rejection");

    // ---- a decision is not revisited ---------------------------------------
    assert!(matches!(
        reject(&mut conn, p.id, member(alice)),
        Err(ProposalError::AlreadyDecided { .. })
    ));
    assert!(matches!(
        confirm(&mut conn, p.id, member(alice)),
        Err(ProposalError::AlreadyDecided { .. })
    ));

    // ---- confirm creates the real edge -------------------------------------
    let confirmed = confirm(&mut conn, second.id, member(alice)).expect("confirming");
    assert_eq!(confirmed.state, "confirmed");
    assert_eq!(confirmed.decided_by, Some(alice));
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }
    let edge = sql_query(
        "SELECT count(*)::bigint AS count FROM item_relationships \
         WHERE source_id = $1 AND target_id = $2 AND relationship = 'blocks'",
    )
    .bind::<diesel::sql_types::Uuid, _>(a.id)
    .bind::<diesel::sql_types::Uuid, _>(b.id)
    .get_result::<Count>(&mut conn)
    .expect("looking for the edge")
    .count;
    assert_eq!(edge, 1, "the edge really exists now");
    let s = stats(&mut conn).expect("stats");
    assert_eq!((s.pending, s.confirmed, s.rejected), (0, 2, 1));

    // ---- the graph's OWN checks are inherited, not re-implemented ----------
    // Two of them, both refused at confirmation time rather than at proposal
    // time: an agent proposing something is a suggestion, and the graph is the
    // thing that gets to say no.

    // 1. A cycle. a blocks b already (confirmed above), so b blocks a closes it.
    let loop_proposal = propose(
        &mut conn,
        NewProposal {
            source_id: b.id,
            target_id: a.id,
            relationship: RelationshipType::Blocks,
            claim: "possible dependency",
            why: "…",
            score: 0.5,
        },
        agent,
    )
    .expect("proposing is allowed — the graph is consulted on confirmation");
    let refused = confirm(&mut conn, loop_proposal.id, member(alice))
        .expect_err("confirming a cycle must be refused");
    assert!(
        matches!(
            refused,
            ProposalError::Graph(GraphError::CycleDetected { .. })
        ),
        "by the graph's own cycle check, not a second copy: {refused}"
    );
    // The refusal leaves it undecided: a refused confirmation is not a decision,
    // and the proposal is still there for a human to reject properly.
    let still = pending_for_item(&mut conn, b.id).expect("pending");
    assert!(
        still
            .iter()
            .any(|p| p.id == loop_proposal.id && p.state == "pending"),
        "still waiting: {still:?}"
    );
    reject(&mut conn, loop_proposal.id, member(alice)).expect("rejected on its merits instead");

    // 2. The rule matrix. `parent` does not run task -> task, and an agent that
    // proposes one gets the same refusal a human would.
    let wrong_shape = propose(
        &mut conn,
        NewProposal {
            source_id: c.id,
            target_id: a.id,
            relationship: RelationshipType::Parent,
            claim: "possible duplicate",
            why: "…",
            score: 0.5,
        },
        agent,
    )
    .expect("proposing an ill-shaped edge is allowed");
    let refused = confirm(&mut conn, wrong_shape.id, member(alice))
        .expect_err("task -> task parent must be refused");
    assert!(
        matches!(refused, ProposalError::Graph(GraphError::Rule(_))),
        "by the existing rule matrix: {refused}"
    );
    reject(&mut conn, wrong_shape.id, member(alice)).expect("cleaning up");

    // ---- two decisions at the same time (KAIROS-T-0538) --------------------
    // The UPDATE has `state = 'pending'`. The first decision holds the row
    // lock in an open transaction. The second passes the check before the
    // UPDATE, then waits on the lock. When the first commits, the second
    // changes no row and is refused. Without the condition, both succeed.
    let d = task(&mut conn, "D");
    let e = task(&mut conn, "E");
    let raced = propose(
        &mut conn,
        NewProposal {
            source_id: d.id,
            target_id: e.id,
            ..evidence(0.4)
        },
        agent,
    )
    .expect("proposing");
    let session = |url: &str| {
        let mut conn = PgConnection::establish(url).expect("connecting");
        sql_query("SET search_path TO org_acme, public")
            .execute(&mut conn)
            .expect("pinning");
        conn
    };
    let mut first = session(&url);
    let mut second = session(&url);
    let pid = |conn: &mut PgConnection| {
        #[derive(diesel::QueryableByName)]
        struct Pid {
            #[diesel(sql_type = diesel::sql_types::Integer)]
            pid: i32,
        }
        sql_query("SELECT pg_backend_pid() AS pid")
            .get_result::<Pid>(conn)
            .expect("pid")
            .pid
    };
    let second_pid = pid(&mut second);
    let raced_id = raced.id;
    let late = std::thread::scope(|scope| {
        first
            .transaction::<_, ProposalError, _>(|first| {
                confirm(first, raced_id, member(alice)).expect("the first decision");
                let waiter = scope.spawn(move || reject(&mut second, raced_id, member(bob)));
                wait_for_lock(&mut conn, second_pid);
                Ok(waiter)
            })
            .expect("the first decision commits")
            .join()
            .expect("the second decision")
    });
    assert!(
        matches!(late, Err(ProposalError::AlreadyDecided { ref state, .. }) if state == "confirmed"),
        "the second decision changes no row: {late:?}"
    );
    let row = pending_for_item(&mut conn, d.id).expect("pending");
    assert!(row.is_empty(), "decided once: {row:?}");

    // ---- the cap ------------------------------------------------------------
    let hub = task(&mut conn, "Hub");
    for i in 0..MAX_PENDING_PER_ITEM {
        let other = task(&mut conn, &format!("Spoke {i}"));
        propose(
            &mut conn,
            NewProposal {
                source_id: hub.id,
                target_id: other.id,
                relationship: RelationshipType::Blocks,
                claim: "possible dependency",
                why: "…",
                score: 0.1,
            },
            agent,
        )
        .unwrap_or_else(|e| panic!("proposal {i} should fit under the cap: {e}"));
    }
    let overflow = task(&mut conn, "One too many");
    let capped = propose(
        &mut conn,
        NewProposal {
            source_id: hub.id,
            target_id: overflow.id,
            relationship: RelationshipType::Blocks,
            claim: "possible dependency",
            why: "…",
            score: 0.1,
        },
        agent,
    );
    assert!(
        matches!(capped, Err(ProposalError::TooManyPending { limit, .. }) if limit == MAX_PENDING_PER_ITEM),
        "an agent loop must not bury the signal under its own output: {capped:?}"
    );
}

static DECIDERS_SCRATCH_DB: scratch::ScratchName =
    scratch::ScratchName::new("kairos_proposals_deciders_test");

/// Who decides a proposal (KAIROS-T-0659): a holder of `approve_proposals`
/// on the board of the source, or of the target, or an org admin. A service
/// account that holds the grant decides, and so does a proposer who holds
/// it. With none of these, the decision is refused, the refusal names both
/// boards, and the proposal waits.
#[test]
fn a_holder_on_either_board_or_an_org_admin_decides() {
    use kairos_db::models::{NewOrganizationMember, OrgRole};

    let admin_url = admin_database_url();
    let mut admin = PgConnection::establish(&admin_url)
        .unwrap_or_else(|e| panic!("cannot connect to compose postgres at {admin_url}: {e}"));
    sql_query(format!(
        "DROP DATABASE IF EXISTS {DECIDERS_SCRATCH_DB} WITH (FORCE)"
    ))
    .execute(&mut admin)
    .expect("dropping scratch");
    sql_query(format!("CREATE DATABASE {DECIDERS_SCRATCH_DB}"))
        .execute(&mut admin)
        .expect("creating scratch");
    let _scratch_guard = scratch::ScratchGuard::new(&DECIDERS_SCRATCH_DB);
    let url = with_database(&admin_url, &DECIDERS_SCRATCH_DB);

    let mut conn = PgConnection::establish(&url).expect("connecting");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, ORG, "Acme Inc").expect("provisioning");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning");
    let org: Uuid = schema::organizations::table
        .filter(schema::organizations::slug.eq(ORG))
        .select(schema::organizations::id)
        .first(&mut conn)
        .expect("the org row");
    let join = |conn: &mut PgConnection, user_id: Uuid, role: OrgRole| {
        diesel::insert_into(schema::organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org,
                user_id,
                role,
            })
            .execute(conn)
            .expect("membership");
    };
    let human = |conn: &mut PgConnection, name: &str| {
        diesel::insert_into(schema::users::table)
            .values(NewUser {
                external_id: format!("dex|{name}"),
                user_name: format!("dex|{name}"),
                email: format!("{name}@acme.test"),
                display_name: name.into(),
            })
            .returning(schema::users::id)
            .get_result::<Uuid>(conn)
            .expect("a person")
    };
    let robin = human(&mut conn, "robin");
    let sam = human(&mut conn, "sam");
    let jamie = human(&mut conn, "jamie");
    let alex = human(&mut conn, "alex");
    let bot = diesel::insert_into(schema::users::table)
        .values(kairos_db::models::public::NewServiceAccountUser {
            external_id: "svc|bot".into(),
            user_name: "svc|bot".into(),
            email: "bot@acme.test".into(),
            display_name: "Bot".into(),
            kind: kairos_db::models::public::USER_KIND_SERVICE_ACCOUNT.into(),
        })
        .returning(schema::users::id)
        .get_result::<Uuid>(&mut conn)
        .expect("bot");
    for member in [robin, sam, jamie, bot] {
        join(&mut conn, member, OrgRole::Member);
    }
    join(&mut conn, alex, OrgRole::Admin);

    let board = |conn: &mut PgConnection, slug: &str| {
        let team = seed_board_team(conn, slug, &format!("{slug}-team"));
        create_board(
            conn,
            BoardLevel::Delivery,
            slug,
            slug,
            kairos_db::CodePrefix::FromSlug,
            Some(team),
            Some(alex),
        )
        .expect("board")
        .id
    };
    let alpha = board(&mut conn, "alpha");
    let beta = board(&mut conn, "beta");
    let approve = kairos_core::abac::APPROVE_PROPOSALS;
    // robin and the bot hold the grant on alpha (the sources), sam on beta
    // (the targets). jamie holds every other grant on both boards.
    for (on, holder) in [(alpha, robin), (alpha, bot), (beta, sam)] {
        abac::grant_capability(&mut conn, on, holder, approve, alex).expect("the grant");
    }
    for on in [alpha, beta] {
        for other in ["manage_tasks", "transition_items", "administer_members"] {
            abac::grant_capability(&mut conn, on, jamie, other, alex).expect("a grant");
        }
    }
    let task = |conn: &mut PgConnection, on: Uuid, title: &str| {
        items::create_task(
            conn,
            items::CreateTask {
                board_id: on,
                column_id: None,
                title,
                content: "…",
                task_type: TaskType::Task,
                work_class: WorkClass::Planned,
                repository_id: None,
            },
            alex,
        )
        .expect("task")
        .id
    };
    let source = task(&mut conn, alpha, "The source on alpha");
    let proposal = |conn: &mut PgConnection, by: Uuid, title: &str| {
        let target = task(conn, beta, title);
        propose(
            conn,
            NewProposal {
                source_id: source,
                target_id: target,
                relationship: RelationshipType::Blocks,
                claim: "possible dependency",
                why: "The source writes what the target reads.",
                score: 0.3,
            },
            by,
        )
        .expect("a proposal")
        .id
    };
    let waits = |conn: &mut PgConnection, id: Uuid| {
        pending_for_item(conn, source)
            .expect("pending")
            .iter()
            .any(|p| p.id == id)
    };

    // 1. Neither board: jamie holds other grants on both, and is refused.
    //    The refusal names the grant and both boards. The proposal waits.
    let first = proposal(&mut conn, bot, "1: neither");
    for refused in [
        confirm(&mut conn, first, member(jamie)),
        reject(&mut conn, first, member(jamie)),
    ] {
        match refused {
            Err(ref e @ ProposalError::NotApprover { ref boards }) => {
                assert_eq!(boards.slugs(), vec!["alpha", "beta"], "{e}");
                let text = e.to_string();
                assert!(
                    text.contains("\"approve_proposals\"")
                        && text.contains("alpha")
                        && text.contains("beta"),
                    "the refusal names the grant and the two boards: {text}"
                );
            }
            other => panic!("jamie holds no approve_proposals: {other:?}"),
        }
    }
    assert!(
        waits(&mut conn, first),
        "a refused decision leaves it pending"
    );

    // 2. A holder on the board of the source confirms.
    let by_source = confirm(&mut conn, first, member(robin)).expect("robin holds it on alpha");
    assert_eq!(by_source.decided_by, Some(robin));

    // 3. A holder on the board of the target confirms and rejects.
    let second = proposal(&mut conn, bot, "3: the target");
    let by_target = confirm(&mut conn, second, member(sam)).expect("sam holds it on beta");
    assert_eq!(by_target.decided_by, Some(sam));
    let third = proposal(&mut conn, bot, "3: the target rejects");
    let rejected = reject(&mut conn, third, member(sam)).expect("sam rejects");
    assert_eq!(rejected.state, "rejected");

    // 4. An org admin decides with no grant. The same person as a member
    //    is refused.
    let refused = proposal(&mut conn, jamie, "4: alex as a member");
    assert!(matches!(
        confirm(&mut conn, refused, member(alex)),
        Err(ProposalError::NotApprover { .. })
    ));
    let fourth = proposal(&mut conn, jamie, "4: the org admin");
    let by_admin = confirm(&mut conn, fourth, org_admin(alex)).expect("alex is an org admin");
    assert_eq!(by_admin.decided_by, Some(alex));

    // 5. A service account that holds the grant decides: it can confirm the
    //    proposal that it made.
    let fifth = proposal(&mut conn, bot, "5: the service account");
    let by_bot = confirm(&mut conn, fifth, member(bot)).expect("the bot holds it on alpha");
    assert_eq!(by_bot.decided_by, Some(bot));

    // 6. The proposer decides their own proposal, as a holder.
    let sixth = proposal(&mut conn, robin, "6: the proposer");
    let own = reject(&mut conn, sixth, member(robin)).expect("robin decides her own");
    assert_eq!(own.decided_by, Some(robin));

    // 7. The grant gone, the holder is refused.
    abac::revoke_capability(&mut conn, beta, sam, approve, alex).expect("the revoke");
    let seventh = proposal(&mut conn, robin, "7: the revoke");
    assert!(matches!(
        confirm(&mut conn, seventh, member(sam)),
        Err(ProposalError::NotApprover { .. })
    ));
    assert!(waits(&mut conn, seventh));
}

/// Wait until the backend `pid` waits on a lock: the second decision is in
/// its UPDATE. Panics after 20 seconds.
fn wait_for_lock(conn: &mut PgConnection, pid: i32) {
    #[derive(diesel::QueryableByName)]
    struct Waiting {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        waiting: bool,
    }
    for _ in 0..400 {
        let waiting = sql_query(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                            WHERE pid = $1 AND wait_event_type = 'Lock') AS waiting",
        )
        .bind::<diesel::sql_types::Integer, _>(pid)
        .get_result::<Waiting>(conn)
        .expect("reading pg_stat_activity")
        .waiting;
        if waiting {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the second decision never waited on the row lock");
}
