//! Turning search hits into related-work **proposals** (KAIROS-A-0021 rules 5,
//! 6 and 7; KAIROS-T-0191).
//!
//! Pure per KAIROS-A-0009: candidates in, ranked proposals out. The queries that
//! produce the candidates live in `kairos-db`; the surfaces that serve them live
//! in `kairos-server`. This is the part that decides **what to claim**.
//!
//! # The claim is a disagreement, not a similarity
//!
//! Similarity alone says nothing worth telling an agent. Two tickets about
//! billing are *supposed* to look alike. What is worth telling is where
//! similarity and the graph **disagree**:
//!
//! | similar, and… | the claim |
//! |---|---|
//! | nothing joins them | a dependency nobody drew |
//! | they share a parent | probably the same work twice |
//! | the other is finished or put away | prior art |
//!
//! # Fused by rank, never by score
//!
//! Lexical relevance is `ts_rank_cd`; vector similarity is cosine. They have no
//! common scale, and any weighted sum of them is a fiction dressed as a number.
//! So the fusion is **reciprocal rank** — position is comparable even when
//! magnitude is not.
//!
//! # There is no threshold, and that is measured
//!
//! Twice, on two corpora. Across 4,927 Metis documents from nineteen
//! repositories, pairs the graph says are related averaged 0.800 cosine while
//! unrelated pairs reached 0.817 at p99. Over Kairos's own seeded tenant, related
//! pairs averaged 0.705–0.727 against 0.580 — with unrelated reaching 0.752 and
//! real edges falling to 0.568.
//!
//! **The distributions overlap on both.** There is no value you could put
//! between them. So nothing here compares a similarity to a constant: ranking
//! within one query is meaningful, and that is all that is used.
//!
//! # Proposals, never assertions
//!
//! An agent told "this blocks you" when it does not will do wrong work
//! confidently, which is worse than being told nothing. At the measured
//! precision — roughly half, at the top of the distribution — the honest verb is
//! *might*. [`Proposal::why`] carries the evidence so a reader can disagree.
//!
//! # Text only: fewer proposals, and a bar (KAIROS-T-0562)
//!
//! With no vectors, the rank of a text match is not enough. A title of common
//! words matches most of the tenant, and the first five of those read like
//! findings. So a text-only answer uses a text score: the share of the title's
//! word weight that a candidate matches, where a rare word weighs more than a
//! common one ([`Candidate::text_score`]). A candidate under
//! [`RetrievalConfig::text_floor`] is not proposed, and the answer has at most
//! [`TEXT_ONLY_LIMIT`] proposals. No proposal over the bar gives no proposal.
//!
//! This bar is not a threshold on a similarity, which the measurements above
//! rule out. It is a share of the title's words, and it applies only when no
//! similarity exists. With vectors, nothing here changes.

use std::fmt;

/// What a proposal is claiming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Claim {
    /// Similar, and nothing in the graph joins them. The edge that should exist.
    ImplicitDependency,
    /// Similar, and they already share a parent — so probably the same work
    /// written down twice rather than two pieces of work.
    NearDuplicate,
    /// Similar, and the other item is finished or put away. *"Didn't we try
    /// this?"* — the question an agent cannot answer from memory, which
    /// KAIROS-A-0020 made answerable by keeping put-away work searchable.
    PriorArt,
}

impl Claim {
    /// The word a caller should read first.
    pub fn label(self) -> &'static str {
        match self {
            Claim::ImplicitDependency => "possible dependency",
            Claim::NearDuplicate => "possible duplicate",
            Claim::PriorArt => "prior art",
        }
    }
}

impl fmt::Display for Claim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What the graph says about a candidate, relative to the item asked about.
///
/// Deliberately **not** a path length. `item_relationships` is sparse — 18 rows
/// in the seeded tenant — so "no path at depth 4" would sound like a finding and
/// mean almost nothing. These three facts are exactly what the claim table uses,
/// and describing only what was checked is the honest version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphRelation {
    /// An edge directly joins them, in either direction.
    pub directly_linked: bool,
    /// They hang off the same parent.
    pub shared_parent: bool,
    /// Same repository — a strong prior for genuine coupling (KAIROS-A-0019).
    pub same_repository: bool,
    /// Same board.
    pub same_board: bool,
}

/// One item that might be worth telling the caller about.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Short code, for citation.
    pub short_code: String,
    /// Title.
    pub title: String,
    /// `strategy` | `initiative` | `task` | `document` | `adr`.
    pub entity_type: String,
    /// Position in the lexical results, 0-based. `None` if it did not appear.
    pub lexical_rank: Option<usize>,
    /// Position in the vector results, 0-based. `None` if it did not appear.
    pub vector_rank: Option<usize>,
    /// What the graph says.
    pub graph: GraphRelation,
    /// Whether the item is finished or put away.
    pub finished: bool,
    /// The literal heading the best-matching chunk sat under, if any.
    pub heading: Option<String>,
    /// The text score, 0 to 1, for a text-only answer: the share of the
    /// title's word weight that this candidate matches. A word weighs by how
    /// rare it is in the tenant. A word in the candidate's title counts in
    /// full, a word only in its content counts half. `None` when the answer
    /// has vectors, and then nothing reads it.
    pub text_score: Option<f32>,
}

/// A bounded, cited claim about one item.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    /// The item.
    pub short_code: String,
    /// Its title.
    pub title: String,
    /// Its type.
    pub entity_type: String,
    /// What is being claimed.
    pub claim: Claim,
    /// The fused rank score. **Comparable within one response and nowhere
    /// else** — it is a sum of reciprocal ranks, not a similarity.
    pub score: f32,
    /// Why, in a sentence a person can disagree with.
    pub why: String,
}

/// How to fuse and bound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetrievalConfig {
    /// How many proposals to return. Small on purpose: an agent handed forty
    /// related items has been given a research project, not an answer.
    pub limit: usize,
    /// The `k` in reciprocal-rank fusion. 60 is the conventional value; it
    /// flattens the difference between adjacent top ranks so that appearing in
    /// *both* lists beats appearing high in one.
    pub rrf_k: f32,
    /// Added for a shared repository. A **weight, not a filter** — filtering
    /// would hide the cross-team duplicate, which is the most valuable hit there
    /// is (KAIROS-A-0019).
    ///
    /// Its size is load-bearing and was got wrong first time. To break ties
    /// without overriding the ranking it must be **smaller than the gap between
    /// adjacent ranks**, and at `k = 60` that gap is `1/60 - 1/61 ≈ 0.00027`. A
    /// weight of 0.005 — five times the gap — silently demoted a top hit in
    /// another repository beneath a fifth-place hit in this one, which is
    /// filtering wearing a weight's clothes.
    pub repository_weight: f32,
    /// Added for a shared board. Half the repository weight, for the same
    /// reason: a board is a weaker signal of coupling than a codebase.
    pub board_weight: f32,
    /// Text only: the lowest [`Candidate::text_score`] that is proposed. A
    /// match on common words alone stays under it.
    pub text_floor: f32,
    /// Text only: added to the text score for a shared repository. It orders
    /// the candidates over the floor; it does not lift one over the floor.
    pub text_repository_weight: f32,
    /// Text only: added to the text score for a shared board. Half the
    /// repository weight, as for the hybrid answer.
    pub text_board_weight: f32,
}

/// The most proposals that a text-only answer gives, whatever the limit.
pub const TEXT_ONLY_LIMIT: usize = 3;

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            limit: 5,
            rrf_k: 60.0,
            repository_weight: 0.000_1,
            board_weight: 0.000_05,
            text_floor: 0.4,
            text_repository_weight: 0.1,
            text_board_weight: 0.05,
        }
    }
}

/// Which sources actually contributed, so a caller can tell a thin answer from a
/// complete one (rule 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sources {
    /// Lexical search ran.
    pub lexical: bool,
    /// Vector search ran and had vectors to search.
    pub vector: bool,
}

impl Sources {
    /// Whether the answer is lexical-only, which is a **degraded** answer rather
    /// than a wrong one: no embeddings yet, a model change, an unreachable
    /// provider. Rule 7 promises this keeps working; it does not promise the
    /// caller is left to guess that it happened.
    pub fn degraded(&self) -> bool {
        !self.vector
    }

    /// A sentence for the response.
    pub fn note(&self) -> &'static str {
        match (self.lexical, self.vector) {
            (true, true) => "Ranked across text and meaning.",
            (true, false) => {
                "Text only: this tenant has no usable vectors. So the search gives at \
                 most 3 proposals, and each matches the rare words of the title. The \
                 search misses items that use different words."
            }
            (false, true) => "Meaning only — no text query was given.",
            (false, false) => "Nothing was searched.",
        }
    }
}

/// Fuse candidates into bounded proposals, best first.
///
/// Candidates already joined by an edge are dropped: there is no edge to propose
/// and no discovery to report, and spending one of a handful of slots restating
/// a link the caller can already see is the opposite of useful.
///
/// With no vectors (`sources.degraded()`), the order is the text score plus the
/// text weights, a candidate under the floor is dropped, and at most
/// [`TEXT_ONLY_LIMIT`] are returned (KAIROS-T-0562).
pub fn propose(
    candidates: &[Candidate],
    sources: Sources,
    config: &RetrievalConfig,
) -> Vec<Proposal> {
    let text_only = sources.degraded();
    let mut scored: Vec<Proposal> = candidates
        .iter()
        .filter(|c| !c.graph.directly_linked)
        .filter(|c| !text_only || c.text_score.is_some_and(|s| s >= config.text_floor))
        .map(|c| {
            let score = if text_only {
                text_only_score(c, config)
            } else {
                fused_score(c, config)
            };
            let claim = classify(c);
            Proposal {
                short_code: c.short_code.clone(),
                title: c.title.clone(),
                entity_type: c.entity_type.clone(),
                claim,
                score,
                why: explain(c, claim),
            }
        })
        .collect();

    // Descending score, tie-broken by short code so the order is total and a
    // caller asking twice gets the same answer twice.
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.short_code.cmp(&b.short_code))
    });
    let limit = if text_only {
        config.limit.min(TEXT_ONLY_LIMIT)
    } else {
        config.limit
    };
    scored.truncate(limit);
    scored
}

/// The reciprocal-rank score of a candidate, with the two priors.
fn fused_score(c: &Candidate, config: &RetrievalConfig) -> f32 {
    let mut score = 0.0;
    if let Some(r) = c.lexical_rank {
        score += 1.0 / (config.rrf_k + r as f32);
    }
    if let Some(r) = c.vector_rank {
        score += 1.0 / (config.rrf_k + r as f32);
    }
    if c.graph.same_repository {
        score += config.repository_weight;
    }
    if c.graph.same_board {
        score += config.board_weight;
    }
    score
}

/// The text score of a candidate, with the text weights for the same
/// repository and the same board.
fn text_only_score(c: &Candidate, config: &RetrievalConfig) -> f32 {
    let mut score = c.text_score.unwrap_or(0.0);
    if c.graph.same_repository {
        score += config.text_repository_weight;
    }
    if c.graph.same_board {
        score += config.text_board_weight;
    }
    score
}

/// Which claim a candidate supports.
///
/// Prior art first: an item being finished is the most specific thing that can
/// be true of it, and it is the claim a caller is least able to reach on their
/// own.
fn classify(c: &Candidate) -> Claim {
    if c.finished {
        Claim::PriorArt
    } else if c.graph.shared_parent {
        Claim::NearDuplicate
    } else {
        Claim::ImplicitDependency
    }
}

/// The sentence behind a claim.
///
/// It names what matched, where, and — the part that keeps this honest — what
/// the graph check actually was. "Nothing links them" would imply a traversal;
/// what was really checked is a direct edge and a shared parent, in a graph
/// sparse enough that absence is weak evidence.
///
/// It does not say "Text only". The response says that once, at the top
/// (KAIROS-T-0562): five copies of one warning read as five findings.
fn explain(c: &Candidate, claim: Claim) -> String {
    let mut why = String::new();
    match (c.lexical_rank.is_some(), c.vector_rank.is_some()) {
        (true, true) => why.push_str("Matches both the words and the meaning"),
        (true, false) => why.push_str("Matches the words"),
        (false, true) => why.push_str("Reads as being about the same thing"),
        (false, false) => why.push_str("Surfaced"),
    }
    if let Some(heading) = &c.heading {
        why.push_str(&format!(" (under \"{heading}\")"));
    }
    match claim {
        Claim::PriorArt => why.push_str(", and it is already finished or put away"),
        Claim::NearDuplicate => why.push_str(", and it hangs off the same parent"),
        // The caveat that belongs with this claim is said ONCE per response, by
        // `graph_caveat`, not once per proposal. Repeating it five times — which
        // is what the first version did, and what reading real output made
        // obvious — buries the part of each sentence that differs.
        Claim::ImplicitDependency => why.push_str(", and nothing in the graph joins them"),
    }
    if c.graph.same_repository {
        why.push_str(". Same repository");
    }
    why.push('.');
    why
}

/// The sentence that keeps "nothing joins them" honest, said once per response.
///
/// `item_relationships` is sparse — 18 rows in the seeded tenant — so absence of
/// an edge is weak evidence, and phrasing it as a finding would be the kind of
/// confident wrong answer rule 6 exists to prevent. Returned only when at least
/// one proposal actually rests on it.
pub fn graph_caveat(proposals: &[Proposal]) -> Option<&'static str> {
    proposals
        .iter()
        .any(|p| p.claim == Claim::ImplicitDependency)
        .then_some(
            "\"Nothing joins them\" means no direct edge and no shared parent — \
             not that a path was searched for and missed. The graph is sparse, so \
             absence is weak evidence.",
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(code: &str) -> Candidate {
        Candidate {
            short_code: code.into(),
            title: format!("Title of {code}"),
            entity_type: "task".into(),
            lexical_rank: None,
            vector_rank: None,
            graph: GraphRelation::default(),
            finished: false,
            heading: None,
            text_score: None,
        }
    }

    fn text_only() -> Sources {
        Sources {
            lexical: true,
            vector: false,
        }
    }

    /// A text-only candidate with a text score, at a lexical rank.
    fn text(code: &str, rank: usize, score: f32) -> Candidate {
        Candidate {
            lexical_rank: Some(rank),
            text_score: Some(score),
            ..candidate(code)
        }
    }

    fn both() -> Sources {
        Sources {
            lexical: true,
            vector: true,
        }
    }

    /// Appearing in both lists must beat appearing at the top of one. That is
    /// the whole reason for fusing by rank.
    #[test]
    fn agreement_between_sources_beats_a_single_strong_hit() {
        let agreed = Candidate {
            lexical_rank: Some(3),
            vector_rank: Some(3),
            ..candidate("A-1")
        };
        let one_source = Candidate {
            lexical_rank: Some(0),
            ..candidate("A-2")
        };
        let out = propose(&[one_source, agreed], both(), &RetrievalConfig::default());
        assert_eq!(out[0].short_code, "A-1", "both lists wins");
    }

    #[test]
    fn already_linked_items_are_not_proposed() {
        let linked = Candidate {
            lexical_rank: Some(0),
            vector_rank: Some(0),
            graph: GraphRelation {
                directly_linked: true,
                ..Default::default()
            },
            ..candidate("A-1")
        };
        assert!(
            propose(&[linked], both(), &RetrievalConfig::default()).is_empty(),
            "there is no edge to propose and nothing to discover"
        );
    }

    #[test]
    fn the_three_claims_are_classified_by_the_disagreement() {
        let base = Candidate {
            vector_rank: Some(0),
            ..candidate("A-1")
        };
        let finished = Candidate {
            finished: true,
            ..base.clone()
        };
        let sibling = Candidate {
            graph: GraphRelation {
                shared_parent: true,
                ..Default::default()
            },
            ..base.clone()
        };
        for (c, expected) in [
            (base, Claim::ImplicitDependency),
            (sibling, Claim::NearDuplicate),
            (finished, Claim::PriorArt),
        ] {
            let out = propose(&[c], both(), &RetrievalConfig::default());
            assert_eq!(out[0].claim, expected);
        }
    }

    /// A finished sibling is prior art, not a duplicate: being already done is
    /// the more specific and more useful thing to say.
    #[test]
    fn prior_art_outranks_near_duplicate_when_both_apply() {
        let c = Candidate {
            vector_rank: Some(0),
            finished: true,
            graph: GraphRelation {
                shared_parent: true,
                ..Default::default()
            },
            ..candidate("A-1")
        };
        assert_eq!(
            propose(&[c], both(), &RetrievalConfig::default())[0].claim,
            Claim::PriorArt
        );
    }

    /// Repository is a weight. A same-repo candidate wins a tie, but a
    /// cross-repo candidate ranked higher by the search still wins outright —
    /// which is what keeps the cross-team duplicate findable.
    #[test]
    fn repository_breaks_ties_without_filtering() {
        let same_repo = Candidate {
            vector_rank: Some(4),
            graph: GraphRelation {
                same_repository: true,
                ..Default::default()
            },
            ..candidate("A-1")
        };
        let other_repo_better = Candidate {
            vector_rank: Some(0),
            ..candidate("A-2")
        };
        let out = propose(
            &[same_repo.clone(), other_repo_better],
            both(),
            &RetrievalConfig::default(),
        );
        assert_eq!(
            out[0].short_code, "A-2",
            "a better hit elsewhere still wins"
        );

        let tie = Candidate {
            vector_rank: Some(4),
            ..candidate("A-3")
        };
        let out = propose(&[tie, same_repo], both(), &RetrievalConfig::default());
        assert_eq!(out[0].short_code, "A-1", "same repository breaks the tie");
    }

    /// The calibration itself, pinned. A weight larger than the gap between
    /// adjacent ranks stops being a weight and starts being a filter, and that
    /// is exactly the bug this had on its first run.
    #[test]
    fn the_priors_are_smaller_than_the_gap_between_adjacent_ranks() {
        let c = RetrievalConfig::default();
        let adjacent_gap = 1.0 / c.rrf_k - 1.0 / (c.rrf_k + 1.0);
        assert!(
            c.repository_weight < adjacent_gap,
            "repository weight {} must not outrank a position: gap is {adjacent_gap}",
            c.repository_weight
        );
        assert!(
            c.board_weight < c.repository_weight,
            "a board is weaker than a codebase"
        );
    }

    #[test]
    fn results_are_bounded_and_ordered_deterministically() {
        let many: Vec<Candidate> = (0..40)
            .map(|i| Candidate {
                vector_rank: Some(i),
                ..candidate(&format!("A-{i:04}"))
            })
            .collect();
        let out = propose(&many, both(), &RetrievalConfig::default());
        assert_eq!(out.len(), 5, "a handful, never a page");
        assert_eq!(out, propose(&many, both(), &RetrievalConfig::default()));
    }

    /// Identical scores must still produce one order, or the same question gets
    /// two answers.
    #[test]
    fn ties_fall_back_to_short_code() {
        let a = Candidate {
            vector_rank: Some(0),
            ..candidate("Z-1")
        };
        let b = Candidate {
            vector_rank: Some(0),
            ..candidate("A-1")
        };
        let out = propose(&[a, b], both(), &RetrievalConfig::default());
        assert_eq!(out[0].short_code, "A-1");
    }

    /// Rule 7: a lexical-only answer says so, ONCE, in the note of the
    /// response, and not in each reason (KAIROS-T-0562).
    #[test]
    fn a_degraded_answer_admits_it_once() {
        assert!(text_only().degraded());
        assert!(!both().degraded());
        let note = text_only().note();
        assert!(note.starts_with("Text only"), "{note}");
        assert!(
            note.contains(&format!("at most {TEXT_ONLY_LIMIT} proposals")),
            "the note names the limit that applies: {note}"
        );
        let out = propose(
            &[text("A-1", 0, 0.9), text("A-2", 1, 0.8)],
            text_only(),
            &RetrievalConfig::default(),
        );
        assert_eq!(out.len(), 2);
        for p in &out {
            assert!(!p.why.contains("Text only"), "{}", p.why);
            assert!(!p.why.contains(note), "{}", p.why);
        }
    }

    /// Text only: a match on common words scores low, and under the floor it is
    /// not proposed. No candidate over the floor gives no proposal.
    #[test]
    fn text_only_drops_candidates_under_the_floor() {
        let config = RetrievalConfig::default();
        let weak = [
            text("A-1", 0, config.text_floor - 0.01),
            text("A-2", 1, 0.05),
            // No text score at all: nothing to hold it to, so it is dropped.
            Candidate {
                lexical_rank: Some(2),
                ..candidate("A-3")
            },
        ];
        assert!(propose(&weak, text_only(), &config).is_empty());

        let out = propose(
            &[text("A-1", 1, config.text_floor), text("A-2", 0, 0.1)],
            text_only(),
            &config,
        );
        let codes: Vec<&str> = out.iter().map(|p| p.short_code.as_str()).collect();
        assert_eq!(codes, ["A-1"], "the floor itself is enough");
    }

    /// Text only: the same repository or board does not lift a candidate over
    /// the floor. It is a weight on the order, not a reason to propose.
    #[test]
    fn text_only_locality_does_not_lift_a_candidate_over_the_floor() {
        let config = RetrievalConfig::default();
        let near_but_weak = Candidate {
            graph: GraphRelation {
                same_repository: true,
                same_board: true,
                ..Default::default()
            },
            ..text("A-1", 0, config.text_floor - 0.05)
        };
        assert!(propose(&[near_but_weak], text_only(), &config).is_empty());
    }

    /// Text only: the order is the text score, and the same board or
    /// repository orders the candidates that are close. A lexical rank does
    /// not decide the order.
    #[test]
    fn text_only_orders_by_text_score_then_locality() {
        let config = RetrievalConfig::default();
        let same_board = Candidate {
            graph: GraphRelation {
                same_board: true,
                ..Default::default()
            },
            ..text("A-2", 3, 0.7)
        };
        let out = propose(
            &[text("A-1", 0, 0.72), same_board, text("A-3", 1, 0.95)],
            text_only(),
            &config,
        );
        let codes: Vec<&str> = out.iter().map(|p| p.short_code.as_str()).collect();
        assert_eq!(codes, ["A-3", "A-2", "A-1"]);
        assert!(out[0].score > out[1].score);
    }

    /// Text only: at most three proposals, whatever the limit.
    #[test]
    fn text_only_gives_at_most_three() {
        let many: Vec<Candidate> = (0..20)
            .map(|i| text(&format!("A-{i:04}"), i, 0.99 - i as f32 * 0.01))
            .collect();
        for limit in [5, 10] {
            let config = RetrievalConfig {
                limit,
                ..RetrievalConfig::default()
            };
            assert_eq!(propose(&many, text_only(), &config).len(), TEXT_ONLY_LIMIT);
        }
        let one = RetrievalConfig {
            limit: 1,
            ..RetrievalConfig::default()
        };
        assert_eq!(propose(&many, text_only(), &one).len(), 1);
    }

    /// With vectors, the text score is not read: no floor, the fused rank and
    /// the old limit, and the reason is the same as before.
    #[test]
    fn with_vectors_the_text_score_changes_nothing() {
        let config = RetrievalConfig::default();
        let many: Vec<Candidate> = (0..10)
            .map(|i| Candidate {
                vector_rank: Some(i),
                ..text(&format!("A-{i:04}"), 9 - i, 0.01)
            })
            .collect();
        let without: Vec<Candidate> = many
            .iter()
            .cloned()
            .map(|c| Candidate {
                text_score: None,
                ..c
            })
            .collect();
        let out = propose(&many, both(), &config);
        assert_eq!(out.len(), config.limit, "no floor and no text limit");
        assert_eq!(out, propose(&without, both(), &config));
        assert_eq!(
            out[0].why,
            "Matches both the words and the meaning, and nothing in the graph joins them."
        );
    }

    /// The honesty requirement: "no edge" must not read as "the graph was
    /// traversed and found nothing". Said once per response rather than once per
    /// proposal — five copies of the same caveat buries what differs.
    #[test]
    fn a_response_resting_on_absent_edges_carries_the_caveat_once() {
        let c = Candidate {
            vector_rank: Some(0),
            ..candidate("A-1")
        };
        let out = propose(&[c], both(), &RetrievalConfig::default());
        assert!(
            out[0].why.contains("nothing in the graph joins them"),
            "{}",
            out[0].why
        );
        let caveat = graph_caveat(&out).expect("the claim rests on an absent edge");
        assert!(caveat.contains("sparse"), "{caveat}");
        assert!(caveat.contains("weak evidence"), "{caveat}");
    }

    /// And it is absent when nothing rests on it, so a response of pure prior art
    /// does not carry a disclaimer about something it never claimed.
    #[test]
    fn the_caveat_is_absent_when_no_claim_rests_on_an_absent_edge() {
        let finished = Candidate {
            vector_rank: Some(0),
            finished: true,
            ..candidate("A-1")
        };
        let out = propose(&[finished], both(), &RetrievalConfig::default());
        assert_eq!(out[0].claim, Claim::PriorArt);
        assert_eq!(graph_caveat(&out), None);
    }

    #[test]
    fn the_matched_heading_is_cited_verbatim() {
        let c = Candidate {
            vector_rank: Some(0),
            heading: Some("Acceptance Criteria **[REQUIRED]**".into()),
            ..candidate("A-1")
        };
        let why = &propose(&[c], both(), &RetrievalConfig::default())[0].why;
        assert!(
            why.contains("under \"Acceptance Criteria **[REQUIRED]**\""),
            "echoed, not recognised: {why}"
        );
    }

    #[test]
    fn no_candidates_is_an_empty_answer_not_an_error() {
        assert!(propose(&[], both(), &RetrievalConfig::default()).is_empty());
    }

    /// Nothing anywhere compares a similarity to a constant. The measurements
    /// say no such constant exists, so this is a structural check that none crept
    /// back in: a candidate with only a rank and no similarity value at all still
    /// produces a full proposal.
    #[test]
    fn scoring_needs_no_similarity_value_at_all() {
        let c = Candidate {
            vector_rank: Some(2),
            ..candidate("A-1")
        };
        let out = propose(&[c], both(), &RetrievalConfig::default());
        assert!(out[0].score > 0.0);
        assert_eq!(out[0].claim, Claim::ImplicitDependency);
    }
}
