//! Pure item write-path decisions (KAIROS-A-0004/A-0001, KAIROS-T-0012):
//! the optimistic-concurrency version check and the soft-delete cascade set
//! over already-loaded `parent` edges.
//!
//! Per KAIROS-A-0009 everything here is a function over in-memory data — no
//! diesel, no I/O. `kairos-db::items` loads rows, calls these decisions,
//! and persists the outcome (the same layering as `board`/`abac`).

use uuid::Uuid;

/// A `parent` edge from the `item_relationships` graph: `parent_id` is the
/// source (owner), `child_id` the target (owned) — KAIROS-A-0001 semantics
/// (strategy → initiative, initiative → task).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentEdge {
    /// The owning item (`item_relationships.source_id`).
    pub parent_id: Uuid,
    /// The owned item (`item_relationships.target_id`).
    pub child_id: Uuid,
}

/// The soft-delete cascade set (KAIROS-A-0001 "deleting a parent cascades
/// soft-deletion to children, walked via `parent` relationships"): every
/// descendant of `root` reachable through `parent` edges, deduplicated,
/// `root` itself excluded. Cycle-safe — a malformed graph cannot loop it
/// (cycle *prevention* is API-layer validation; this computation merely
/// tolerates them).
pub fn cascade_descendants(root: Uuid, edges: &[ParentEdge]) -> Vec<Uuid> {
    let mut seen = std::collections::HashSet::from([root]);
    let mut order: Vec<Uuid> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for edge in edges.iter().filter(|e| e.parent_id == parent) {
            if seen.insert(edge.child_id) {
                order.push(edge.child_id);
                frontier.push(edge.child_id);
            }
        }
    }
    order
}

/// What an archive by one principal reaches (COLLIERY-T-0234): the
/// result of [`cascade_reach`]. Each descendant of the root is in exactly
/// one of the three lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CascadeReach {
    /// The descendants that the archive takes, in the order of
    /// [`cascade_descendants`].
    pub reached: Vec<Uuid>,
    /// The descendants where the archive stops: the principal cannot edit
    /// them.
    pub stopped: Vec<Uuid>,
    /// The descendants below a stop: `(item, stop)`. The archive does not
    /// take them, whether or not the principal can edit them.
    pub below: Vec<(Uuid, Uuid)>,
}

/// The cascade set of an archive by ONE principal (COLLIERY-T-0234): the
/// descendants of `root` that the archive takes, and those that it leaves.
///
/// THE ATTACK that this function stops. A principal creates an initiative,
/// so it can edit the initiative. The link rule lets it write a `parent`
/// edge from its initiative to a task of a different team. It then
/// archives the initiative. Until COLLIERY-T-0234 the cascade took each
/// descendant and asked nothing, so the task of the other team was
/// archived.
///
/// THE RULE. The archive takes a descendant when the principal may edit
/// it (`may_edit`, the edit rule) AND may edit each item between it and
/// the root, on each path. The archive stops at a descendant that the
/// principal cannot edit, and takes nothing below it.
///
/// WHY nothing below a stop. Archive is an operation on a subtree. An
/// archived task below a live initiative is hidden from the board of that
/// initiative with no archived parent to explain it, and a restore of the
/// root does not bring it back. So a subtree that the principal cannot
/// take is left whole.
///
/// WHY each path. An item has one parent since KAIROS-T-0572: the database
/// refuses a second one. The function still takes each path, so that a
/// graph with two parents (data from before the migration, or a test)
/// gets a safe answer: if one parent is a stop, the item is below a stop,
/// and the rule above applies. The function archives less when the graph
/// gives two answers.
///
/// `may_edit` is asked one time for each descendant. The root is not
/// asked: the caller checks the edit rule for the root before the archive.
/// Cycle-safe, as [`cascade_descendants`] is.
pub fn cascade_reach(
    root: Uuid,
    edges: &[ParentEdge],
    may_edit: impl Fn(Uuid) -> bool,
) -> CascadeReach {
    use std::collections::HashSet;

    let descendants = cascade_descendants(root, edges);
    let cannot_edit: Vec<Uuid> = descendants
        .iter()
        .copied()
        .filter(|id| !may_edit(*id))
        .collect();

    // Not taken: each item that the principal cannot edit, and each item
    // below one.
    let mut left: HashSet<Uuid> = cannot_edit.iter().copied().collect();
    for id in &cannot_edit {
        left.extend(
            cascade_descendants(*id, edges)
                .into_iter()
                .filter(|below| *below != root),
        );
    }

    let reached: Vec<Uuid> = descendants
        .iter()
        .copied()
        .filter(|id| !left.contains(id))
        .collect();
    let taken: HashSet<Uuid> = reached.iter().copied().chain([root]).collect();

    // A stop is an item that the principal cannot edit, with a parent that
    // the archive takes. Each other item that is left is below a stop.
    let mut stopped: Vec<Uuid> = cannot_edit
        .iter()
        .copied()
        .filter(|id| {
            edges
                .iter()
                .any(|e| e.child_id == *id && taken.contains(&e.parent_id))
        })
        .collect();
    let mut named: HashSet<Uuid> = stopped.iter().copied().collect();
    let mut below: Vec<(Uuid, Uuid)> = Vec::new();
    let mut name_below = |stop: Uuid, named: &mut HashSet<Uuid>| {
        for id in cascade_descendants(stop, edges) {
            if left.contains(&id) && id != root && named.insert(id) {
                below.push((id, stop));
            }
        }
    };
    for stop in &stopped {
        name_below(*stop, &mut named);
    }
    // Only a graph with a cycle leaves an item with no name. Such an item
    // that the principal cannot edit is a stop too, so that each item that
    // is left has a reason.
    for id in &cannot_edit {
        if named.insert(*id) {
            stopped.push(*id);
            name_below(*id, &mut named);
        }
    }

    CascadeReach {
        reached,
        stopped,
        below,
    }
}

/// The optimistic-concurrency decision for a content edit (KAIROS-A-0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCheck {
    /// The submitted base version matches: the write may proceed and the
    /// item becomes `next_version`.
    Proceed {
        /// `current_version + 1`.
        next_version: i32,
    },
    /// The submitted base version is stale: reject (HTTP 409), the client
    /// must reload `current_version` and reconcile.
    Conflict {
        /// The item's actual current version.
        current_version: i32,
    },
}

/// Decide whether a content edit based on `expected_version` may be applied
/// to an item currently at `current_version` (KAIROS-A-0004 optimistic
/// concurrency).
///
/// This is the pure mirror of the atomic SQL enforcement in
/// `kairos-db::items::update_item_content` (`UPDATE … WHERE version =
/// expected_version`) — one contract, two layers, so they can never
/// disagree (the same pattern as `abac::capability_matches`).
pub fn check_version(current_version: i32, expected_version: i32) -> VersionCheck {
    if current_version == expected_version {
        VersionCheck::Proceed {
            next_version: current_version + 1,
        }
    } else {
        VersionCheck::Conflict { current_version }
    }
}

/// The children of a parent, counted apart (KAIROS-T-0080,
/// KAIROS-T-0567, KAIROS-T-0571): `done + cancelled + open = total`.
/// `removed` is apart from `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChildrenCounts {
    /// In a done column, and not cancelled. An archived child in a done
    /// column counts here.
    pub done: i64,
    /// Cancelled tasks. A cancel is "won't do", not done.
    pub cancelled: i64,
    /// Live, not in a done column, and not cancelled.
    pub open: i64,
    pub total: i64,
    /// Archived from a column that is not done (KAIROS-T-0571). They are
    /// not open work, and they are not in `total`.
    pub removed: i64,
}

/// One column of a children-by-column rollup, for
/// [`children_progress_counts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColumnChildren {
    pub is_done: bool,
    /// The children that count: the live children, and the archived
    /// children when the column is done.
    pub count: i64,
    /// The cancelled tasks among `count`.
    pub cancelled: i64,
    /// The archived children that do not count: the column is not done.
    pub removed: i64,
}

/// Sum a children-by-column rollup into [`ChildrenCounts`] (KAIROS-T-0080).
/// `done` is the sum over done-flagged columns, less the cancelled tasks
/// (KAIROS-T-0567). An archived child counts by the column that it was
/// archived from (KAIROS-T-0571): a done column gives done, another column
/// gives `removed`. Pure — the grouping itself is the single SQL query in
/// `kairos-db::graph::children_progress`.
pub fn children_progress_counts(by_column: &[ColumnChildren]) -> ChildrenCounts {
    let mut counts = ChildrenCounts::default();
    for column in by_column {
        counts.total += column.count;
        counts.cancelled += column.cancelled;
        counts.removed += column.removed;
        if column.is_done {
            counts.done += column.count - column.cancelled;
        } else {
            counts.open += column.count - column.cancelled;
        }
    }
    counts
}

/// The heading of a note that `append_note` adds (KAIROS-T-0367,
/// KAIROS-T-0504): `## Note n-3 (2026-10-11, Robin)`, with the id of the
/// note, the UTC date and the display name of the author.
pub fn note_heading(position: usize, date: chrono::NaiveDate, author: &str) -> String {
    kairos_markdown::notes::note_heading(position, &date.format("%Y-%m-%d").to_string(), author)
}

/// The id of the note that `append_note` adds to `content`
/// (KAIROS-T-0504): one after the count of its note headings.
pub fn next_note_position(content: &str) -> usize {
    kairos_markdown::notes::notes(content).len() + 1
}

/// The id of the note at `position`: `n-3` (KAIROS-T-0504).
pub fn note_id(position: usize) -> String {
    kairos_markdown::notes::note_id(position)
}

/// The notes of `content` (KAIROS-T-0504): id, date, author, first line.
pub fn notes(content: &str) -> Vec<kairos_markdown::notes::Note> {
    kairos_markdown::notes::notes(content)
}

/// The content with `line` at the end of the note `note_id`
/// (KAIROS-T-0505: `Promoted to KAIROS-A-0029.`), after an empty line.
/// `None` when the content has no note with that id.
pub fn content_with_note_line(content: &str, note_id: &str, line: &str) -> Option<String> {
    let note = notes(content).into_iter().find(|note| note.id == note_id)?;
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<&str> = lines[..=note.last_line].to_vec();
    out.push("");
    out.push(line);
    out.extend_from_slice(&lines[note.last_line + 1..]);
    let mut joined = out.join("\n");
    if content.ends_with('\n') {
        joined.push('\n');
    }
    Some(joined)
}

/// The content after a note is appended (KAIROS-T-0367): the old content
/// with no trailing blank lines, an empty line, the heading, an empty line,
/// and the text with no leading or trailing blank lines. An empty content
/// gets only the note. `None` when the text is empty.
pub fn content_with_note(content: &str, heading: &str, text: &str) -> Option<String> {
    let text = text.trim_end().trim_start_matches(['\n', '\r']);
    if text.trim().is_empty() {
        return None;
    }
    let base = content.trim_end();
    let note = format!("{heading}\n\n{text}");
    Some(if base.is_empty() {
        note
    } else {
        format!("{base}\n\n{note}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_check_matches_a0004_contract() {
        assert_eq!(
            check_version(3, 3),
            VersionCheck::Proceed { next_version: 4 }
        );
        assert_eq!(
            check_version(4, 3),
            VersionCheck::Conflict { current_version: 4 }
        );
        // A stale-but-higher submitted version is still a conflict, not a
        // proceed: only an exact match writes.
        assert_eq!(
            check_version(2, 7),
            VersionCheck::Conflict { current_version: 2 }
        );
    }

    #[test]
    fn cascade_walks_chain() {
        let s = Uuid::new_v4();
        let i = Uuid::new_v4();
        let t = Uuid::new_v4();
        let edges = [
            ParentEdge {
                parent_id: s,
                child_id: i,
            },
            ParentEdge {
                parent_id: i,
                child_id: t,
            },
        ];
        assert_eq!(cascade_descendants(s, &edges), vec![i, t]);
        assert_eq!(cascade_descendants(i, &edges), vec![t]);
        assert_eq!(cascade_descendants(t, &edges), Vec::<Uuid>::new());
    }

    #[test]
    fn cascade_ignores_unrelated_edges_and_dedups_diamonds() {
        let root = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let shared = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let orphan = Uuid::new_v4();
        let edges = [
            ParentEdge {
                parent_id: root,
                child_id: a,
            },
            ParentEdge {
                parent_id: root,
                child_id: b,
            },
            // Diamond: both a and b claim `shared`.
            ParentEdge {
                parent_id: a,
                child_id: shared,
            },
            ParentEdge {
                parent_id: b,
                child_id: shared,
            },
            // Unrelated subtree must not be pulled in.
            ParentEdge {
                parent_id: stranger,
                child_id: orphan,
            },
        ];
        let result = cascade_descendants(root, &edges);
        assert_eq!(result.len(), 3, "shared child appears once: {result:?}");
        assert!(result.contains(&a) && result.contains(&b) && result.contains(&shared));
        assert!(!result.contains(&stranger) && !result.contains(&orphan));
    }

    fn column(is_done: bool, count: i64, cancelled: i64, removed: i64) -> ColumnChildren {
        ColumnChildren {
            is_done,
            count,
            cancelled,
            removed,
        }
    }

    /// KAIROS-T-0080: the done count is exactly the occupants of
    /// done-flagged columns; no flagged columns means done = 0 (clients
    /// then show composition only, never a percentage).
    #[test]
    fn children_progress_counts_sums_done_columns() {
        let counts = |done, cancelled, open, total| ChildrenCounts {
            done,
            cancelled,
            open,
            total,
            removed: 0,
        };
        assert_eq!(children_progress_counts(&[]), counts(0, 0, 0, 0));
        assert_eq!(
            children_progress_counts(&[
                column(false, 3, 0, 0),
                column(true, 2, 0, 0),
                column(false, 1, 0, 0),
                column(true, 1, 0, 0),
            ]),
            counts(3, 0, 4, 7)
        );
        // Zero done-flagged columns: composition only.
        assert_eq!(
            children_progress_counts(&[column(false, 4, 0, 0), column(false, 2, 0, 0)]),
            counts(0, 0, 6, 6)
        );
    }

    /// KAIROS-T-0567: a cancelled task in the done column is not done.
    #[test]
    fn children_progress_counts_cancelled_apart() {
        assert_eq!(
            children_progress_counts(&[column(false, 1, 0, 0), column(true, 4, 1, 0)]),
            ChildrenCounts {
                done: 3,
                cancelled: 1,
                open: 1,
                total: 5,
                removed: 0,
            }
        );
    }

    /// KAIROS-T-0571: a child archived from a column that is not done is
    /// removed. It is not open, and it is not in the total.
    #[test]
    fn children_progress_counts_removed_apart() {
        assert_eq!(
            children_progress_counts(&[column(false, 1, 0, 2), column(true, 8, 0, 0)]),
            ChildrenCounts {
                done: 8,
                cancelled: 0,
                open: 1,
                total: 9,
                removed: 2,
            }
        );
        // Each child archived from a column that is not done: no work.
        assert_eq!(
            children_progress_counts(&[column(false, 0, 0, 3)]),
            ChildrenCounts {
                removed: 3,
                ..ChildrenCounts::default()
            }
        );
    }

    fn edge(parent_id: Uuid, child_id: Uuid) -> ParentEdge {
        ParentEdge {
            parent_id,
            child_id,
        }
    }

    /// COLLIERY-T-0234: with each descendant editable, the reach is the
    /// cascade set of KAIROS-A-0001, in the same order.
    #[test]
    fn reach_is_the_cascade_set_when_the_principal_edits_each_item() {
        let (s, i, t, u) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let edges = [edge(s, i), edge(i, t), edge(i, u)];
        let reach = cascade_reach(s, &edges, |_| true);
        assert_eq!(reach.reached, cascade_descendants(s, &edges));
        assert!(reach.stopped.is_empty() && reach.below.is_empty());
    }

    /// COLLIERY-T-0234, the attack: a child that the principal cannot
    /// edit is not taken, and its siblings are.
    #[test]
    fn reach_leaves_a_child_that_the_principal_cannot_edit() {
        let (i, mine, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let edges = [edge(i, mine), edge(i, theirs)];
        let reach = cascade_reach(i, &edges, |id| id != theirs);
        assert_eq!(reach.reached, vec![mine]);
        assert_eq!(reach.stopped, vec![theirs]);
        assert!(reach.below.is_empty());
    }

    /// COLLIERY-T-0234: the archive stops at the initiative, and the task
    /// below it is left although the principal can edit it.
    #[test]
    fn reach_takes_nothing_below_a_stop() {
        let (s, i, t) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let edges = [edge(s, i), edge(i, t)];
        let reach = cascade_reach(s, &edges, |id| id != i);
        assert!(reach.reached.is_empty());
        assert_eq!(reach.stopped, vec![i]);
        assert_eq!(reach.below, vec![(t, i)]);
    }

    /// COLLIERY-T-0234: an item with two parents is left when ONE parent
    /// is a stop. A stop below a stop keeps its own reason.
    #[test]
    fn reach_archives_less_when_two_paths_disagree() {
        let (root, open, shut, shared, deep) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let edges = [
            edge(root, open),
            edge(root, shut),
            edge(open, shared),
            edge(shut, shared),
            edge(shared, deep),
        ];
        let reach = cascade_reach(root, &edges, |id| id != shut);
        assert_eq!(reach.reached, vec![open]);
        assert_eq!(reach.stopped, vec![shut]);
        let mut below = reach.below.clone();
        below.sort();
        let mut expected = vec![(shared, shut), (deep, shut)];
        expected.sort();
        assert_eq!(below, expected);

        // `shared` cannot be edited either: it has a parent that the
        // archive takes, so it is a stop with its own reason.
        let reach = cascade_reach(root, &edges, |id| id != shut && id != shared);
        assert_eq!(reach.reached, vec![open]);
        let mut stopped = reach.stopped.clone();
        stopped.sort();
        let mut expected = vec![shut, shared];
        expected.sort();
        assert_eq!(stopped, expected);
        assert_eq!(reach.below.len(), 1);
        assert_eq!(reach.below[0].0, deep);
    }

    /// COLLIERY-T-0234: each descendant is in exactly one list, for each
    /// answer of the edit rule, and the root is in none. A cycle does not
    /// change that.
    #[test]
    fn reach_puts_each_descendant_in_one_list() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let edges = [
            edge(ids[0], ids[1]),
            edge(ids[1], ids[2]),
            edge(ids[2], ids[1]), // a cycle below the root
            edge(ids[0], ids[3]),
            edge(ids[3], ids[4]),
            edge(ids[4], ids[0]), // a cycle through the root
        ];
        for mask in 0u8..16 {
            let may_edit = |id: Uuid| {
                let index = ids.iter().position(|x| *x == id).expect("known id");
                mask & (1 << (index - 1)) != 0
            };
            let reach = cascade_reach(ids[0], &edges, may_edit);
            let mut all: Vec<Uuid> = reach
                .reached
                .iter()
                .chain(&reach.stopped)
                .copied()
                .chain(reach.below.iter().map(|(id, _)| *id))
                .collect();
            all.sort();
            let mut expected = cascade_descendants(ids[0], &edges);
            expected.sort();
            assert_eq!(all, expected, "mask {mask:04b}: {reach:?}");
            assert!(
                reach.reached.iter().all(|id| may_edit(*id)),
                "mask {mask:04b}: the archive takes an item that the principal cannot edit"
            );
            assert!(reach.stopped.iter().all(|id| !may_edit(*id)));
        }
    }

    #[test]
    fn cascade_tolerates_cycles_and_excludes_root() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let edges = [
            ParentEdge {
                parent_id: a,
                child_id: b,
            },
            // Malformed back-edge: b claims to be a's parent.
            ParentEdge {
                parent_id: b,
                child_id: a,
            },
        ];
        // Terminates, and the root never appears in its own cascade set.
        assert_eq!(cascade_descendants(a, &edges), vec![b]);
    }

    #[test]
    fn a_note_goes_at_the_end_under_a_dated_heading() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 11).unwrap();
        let heading = note_heading(1, date, " Robin ");
        assert_eq!(heading, "## Note n-1 (2026-10-11, Robin)");
        assert_eq!(
            content_with_note("# T\n\nBody\n\n\n", &heading, "\n\nDone.\n").as_deref(),
            Some("# T\n\nBody\n\n## Note n-1 (2026-10-11, Robin)\n\nDone.")
        );
        assert_eq!(
            content_with_note("", &heading, "Done.").as_deref(),
            Some("## Note n-1 (2026-10-11, Robin)\n\nDone.")
        );
        assert_eq!(content_with_note("Body", &heading, " \n\t\n"), None);
    }

    // KAIROS-T-0504: the id of a new note is one after the count of the
    // note headings, with or without ids.
    #[test]
    fn the_next_note_follows_the_count_of_the_headings() {
        assert_eq!(next_note_position("# T\n\nBody"), 1);
        assert_eq!(
            next_note_position(
                "# T\n\n## Note (2026-10-01, Robin)\n\nx\n\n## Note n-2 (2026-10-02, Sam)\n\ny"
            ),
            3
        );
        assert_eq!(note_id(3), "n-3");
    }

    // KAIROS-T-0505: a line goes at the end of one note.
    #[test]
    fn a_line_goes_at_the_end_of_the_note() {
        let content = "# T\n\n## Note n-1 (2026-10-01, Robin)\n\nOne.\n\n\n## Note n-2 (2026-10-02, Sam)\n\nTwo.\n";
        assert_eq!(
            content_with_note_line(content, "n-1", "Promoted to ACME-A-0001.").as_deref(),
            Some(
                "# T\n\n## Note n-1 (2026-10-01, Robin)\n\nOne.\n\nPromoted to ACME-A-0001.\n\n\n## Note n-2 (2026-10-02, Sam)\n\nTwo.\n"
            )
        );
        assert_eq!(
            content_with_note_line(content, "n-2", "Promoted to ACME-D-0001.").as_deref(),
            Some(
                "# T\n\n## Note n-1 (2026-10-01, Robin)\n\nOne.\n\n\n## Note n-2 (2026-10-02, Sam)\n\nTwo.\n\nPromoted to ACME-D-0001.\n"
            )
        );
        assert_eq!(content_with_note_line(content, "n-3", "x"), None);
    }
}
