//! The state of the search page in the URL query (KAIROS-T-0583): the
//! query, each filter of `POST /api/search`, the traversal, the sort and
//! the page. A reload, the Back button and a shared link give the same
//! search, because the page reads its state from the URL only.
//!
//! The keys are the names of the API fields, with three exceptions:
//! `meta.<slug>=<value>` is one entry of `filter.metadata`; `from`,
//! `relationships`, `direction` and `depth` are the traversal; `sort` and
//! `order` are the sort clause. A list is one value with commas
//! (`entity_type=task,adr`): a vocabulary word and a slug have no comma.
//!
//! Pure and host-tested: [`SearchState::to_query`] writes the query,
//! [`SearchState::from_pairs`] reads the decoded pairs back, and
//! [`SearchState::to_request`] makes the request body.

use std::collections::BTreeMap;

use super::data;
use crate::api::encode_component;

/// The prefix of a metadata key in the URL: `meta.priority=high`.
const META_PREFIX: &str = "meta.";

/// The traversal part of the state.
#[derive(Clone, Debug, PartialEq)]
pub struct TraverseState {
    /// The short code of the start item.
    pub from: String,
    /// The relationship types to follow.
    pub relationships: Vec<String>,
    /// `outbound|inbound|both`.
    pub direction: String,
    /// 1 to 10.
    pub depth: u32,
}

impl Default for TraverseState {
    fn default() -> Self {
        Self {
            from: String::new(),
            relationships: vec!["parent".to_string()],
            direction: "outbound".to_string(),
            depth: 3,
        }
    }
}

/// The full state of one search, as the URL holds it. An empty text is
/// "not set".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchState {
    pub q: String,
    pub entity_type: Vec<String>,
    pub task_type: Vec<String>,
    pub work_class: Vec<String>,
    /// A board slug.
    pub board: String,
    /// A column name (with `board`).
    pub column: String,
    /// A team slug.
    pub team: String,
    /// A repository slug.
    pub repository: String,
    /// A person: `me`, an email, a user name or a user id.
    pub claimed_by: String,
    pub owned_by: String,
    pub created_by: String,
    pub is_bucket: Option<bool>,
    /// `(slug, value)` in the order of the URL.
    pub metadata: Vec<(String, String)>,
    /// Metadata slugs with no value.
    pub metadata_missing: Vec<String>,
    /// A day (`YYYY-MM-DD`) or an RFC 3339 instant.
    pub created_after: String,
    pub created_before: String,
    pub updated_after: String,
    pub updated_before: String,
    pub include_deleted: bool,
    pub traverse: Option<TraverseState>,
    /// `(field, order)`; `None` is the default of the server.
    pub sort: Option<(String, String)>,
    pub limit: Option<i64>,
    pub offset: i64,
}

/// What [`SearchState::from_pairs`] reads: the state, and a sentence for
/// each part of the URL that it cannot use.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedQuery {
    pub state: SearchState,
    pub problems: Vec<String>,
}

/// A comma list, without empty parts.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// `Some(text)` for a text that is not empty after the trim.
fn some_text(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A day is the start of that day in UTC; an instant stays as it is.
fn day_start(text: &str) -> Option<String> {
    some_text(text).map(|t| {
        if is_day(&t) {
            format!("{t}T00:00:00Z")
        } else {
            t
        }
    })
}

/// A day is the end of that day in UTC, so the end date is in the range;
/// an instant stays as it is.
fn day_end(text: &str) -> Option<String> {
    some_text(text).map(|t| {
        if is_day(&t) {
            format!("{t}T23:59:59Z")
        } else {
            t
        }
    })
}

/// `YYYY-MM-DD`.
fn is_day(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

impl SearchState {
    /// The query of the URL, with the leading `?`, or an empty text for an
    /// empty state. The order of the keys is fixed, so one state gives one
    /// URL.
    pub fn to_query(&self) -> String {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut text = |key: &str, value: &str| {
            if !value.is_empty() {
                pairs.push((key.to_string(), value.to_string()));
            }
        };
        text("q", &self.q);
        text("entity_type", &self.entity_type.join(","));
        text("task_type", &self.task_type.join(","));
        text("work_class", &self.work_class.join(","));
        text("board", &self.board);
        text("column", &self.column);
        text("team", &self.team);
        text("repository", &self.repository);
        text("claimed_by", &self.claimed_by);
        text("owned_by", &self.owned_by);
        text("created_by", &self.created_by);
        if let Some(flag) = self.is_bucket {
            text("is_bucket", if flag { "true" } else { "false" });
        }
        for (slug, value) in &self.metadata {
            if !slug.is_empty() {
                pairs.push((format!("{META_PREFIX}{slug}"), value.clone()));
            }
        }
        let mut text = |key: &str, value: &str| {
            if !value.is_empty() {
                pairs.push((key.to_string(), value.to_string()));
            }
        };
        text("metadata_missing", &self.metadata_missing.join(","));
        text("created_after", &self.created_after);
        text("created_before", &self.created_before);
        text("updated_after", &self.updated_after);
        text("updated_before", &self.updated_before);
        if self.include_deleted {
            text("include_deleted", "true");
        }
        if let Some(traverse) = &self.traverse {
            pairs.push(("from".to_string(), traverse.from.clone()));
            pairs.push((
                "relationships".to_string(),
                traverse.relationships.join(","),
            ));
            pairs.push(("direction".to_string(), traverse.direction.clone()));
            pairs.push(("depth".to_string(), traverse.depth.to_string()));
        }
        if let Some((field, order)) = &self.sort {
            pairs.push(("sort".to_string(), field.clone()));
            pairs.push(("order".to_string(), order.clone()));
        }
        if let Some(limit) = self.limit {
            pairs.push(("limit".to_string(), limit.to_string()));
        }
        if self.offset > 0 {
            pairs.push(("offset".to_string(), self.offset.to_string()));
        }
        if pairs.is_empty() {
            return String::new();
        }
        let body = pairs
            .iter()
            .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("?{body}")
    }

    /// Reads the decoded `(key, value)` pairs of a URL query. An unknown
    /// key or a bad value is not used, and gives a sentence in `problems`.
    pub fn from_pairs<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> ParsedQuery
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut state = SearchState::default();
        let mut problems = Vec::new();
        let mut traverse = TraverseState::default();
        let mut has_from = false;
        let mut traverse_keys = false;
        let mut sort_field = None::<String>;
        let mut sort_order = None::<String>;
        for (key, value) in pairs {
            let (key, value) = (key.as_ref(), value.as_ref());
            let bad = |what: &str| format!("The link has a bad value for \"{key}\": {what}.");
            match key {
                "q" => state.q = value.to_string(),
                "entity_type" => state.entity_type = split_list(value),
                "task_type" => state.task_type = split_list(value),
                "work_class" => state.work_class = split_list(value),
                "board" => state.board = value.to_string(),
                "column" => state.column = value.to_string(),
                "team" => state.team = value.to_string(),
                "repository" => state.repository = value.to_string(),
                "claimed_by" => state.claimed_by = value.to_string(),
                "owned_by" => state.owned_by = value.to_string(),
                "created_by" => state.created_by = value.to_string(),
                "is_bucket" => match value {
                    "true" => state.is_bucket = Some(true),
                    "false" => state.is_bucket = Some(false),
                    _ => problems.push(bad("use true or false")),
                },
                "metadata_missing" => state.metadata_missing = split_list(value),
                "created_after" => state.created_after = value.to_string(),
                "created_before" => state.created_before = value.to_string(),
                "updated_after" => state.updated_after = value.to_string(),
                "updated_before" => state.updated_before = value.to_string(),
                "include_deleted" => match value {
                    "true" => state.include_deleted = true,
                    "false" => state.include_deleted = false,
                    _ => problems.push(bad("use true or false")),
                },
                "from" => {
                    traverse.from = value.to_string();
                    has_from = true;
                }
                "relationships" => {
                    traverse.relationships = split_list(value);
                    traverse_keys = true;
                }
                "direction" => {
                    traverse.direction = value.to_string();
                    traverse_keys = true;
                }
                "depth" => {
                    traverse_keys = true;
                    match value.parse::<u32>() {
                        Ok(depth) if (1..=10).contains(&depth) => traverse.depth = depth,
                        _ => problems.push(bad("use a number from 1 to 10")),
                    }
                }
                "sort" => sort_field = Some(value.to_string()),
                "order" => sort_order = Some(value.to_string()),
                "limit" => match value.parse::<i64>() {
                    Ok(limit) if (1..=100).contains(&limit) => state.limit = Some(limit),
                    _ => problems.push(bad("use a number from 1 to 100")),
                },
                "offset" => match value.parse::<i64>() {
                    Ok(offset) if offset >= 0 => state.offset = offset,
                    _ => problems.push(bad("use a number that is 0 or more")),
                },
                other => match other.strip_prefix(META_PREFIX) {
                    Some(slug) if !slug.is_empty() => {
                        state.metadata.push((slug.to_string(), value.to_string()));
                    }
                    _ => problems.push(format!("The link has an unknown parameter \"{other}\".")),
                },
            }
        }
        if has_from {
            state.traverse = Some(traverse);
        } else if traverse_keys {
            problems.push(
                "The link has traversal parameters but no \"from\". The page does not use them."
                    .to_string(),
            );
        }
        match (sort_field, sort_order) {
            (Some(field), order) => {
                state.sort = Some((field, order.unwrap_or_else(|| "desc".to_string())));
            }
            (None, Some(_)) => problems.push(
                "The link has an \"order\" but no \"sort\". The page does not use it.".to_string(),
            ),
            (None, None) => {}
        }
        ParsedQuery { state, problems }
    }

    /// True when the state asks for nothing: no text, no filter and no
    /// traversal. The page then shows its start text, not an error.
    pub fn asks_nothing(&self) -> bool {
        let filter = self.filter();
        self.q.trim().is_empty() && filter.is_empty() && self.traverse.is_none()
    }

    /// The `filter` of the request.
    fn filter(&self) -> data::SearchFilter {
        let list = |values: &[String]| (!values.is_empty()).then(|| values.to_vec());
        let metadata: BTreeMap<String, String> = self
            .metadata
            .iter()
            .filter_map(|(slug, value)| Some((some_text(slug)?, some_text(value)?)))
            .collect();
        data::SearchFilter {
            entity_type: list(&self.entity_type),
            board: some_text(&self.board),
            column: some_text(&self.column),
            team: some_text(&self.team),
            repository: some_text(&self.repository),
            task_type: list(&self.task_type),
            work_class: list(&self.work_class),
            is_bucket: self.is_bucket,
            metadata: (!metadata.is_empty()).then_some(metadata),
            created_after: day_start(&self.created_after),
            created_before: day_end(&self.created_before),
            updated_after: day_start(&self.updated_after),
            updated_before: day_end(&self.updated_before),
            metadata_missing: list(&self.metadata_missing),
            claimed_by: some_text(&self.claimed_by),
            owned_by: some_text(&self.owned_by),
            created_by: some_text(&self.created_by),
            include_deleted: self.include_deleted,
        }
    }

    /// The body of `POST /api/search`, or the sentence that tells the
    /// person what to change.
    pub fn to_request(&self) -> Result<data::SearchRequest, String> {
        let q = some_text(&self.q);
        let filter = self.filter();
        let traverse = match &self.traverse {
            None => None,
            Some(traverse) => {
                let from = traverse.from.trim().to_uppercase();
                if from.is_empty() {
                    return Err("Type the short code of the start item for the traversal.".into());
                }
                if traverse.relationships.is_empty() {
                    return Err("Select one or more relationship types for the traversal.".into());
                }
                Some(data::SearchTraverse {
                    from: data::SearchTraverseFrom { short_code: from },
                    relationships: traverse.relationships.clone(),
                    direction: traverse.direction.clone(),
                    depth: traverse.depth.clamp(1, 10),
                })
            }
        };
        if q.is_none() && filter.is_empty() && traverse.is_none() {
            return Err("Type a text query, or add a filter or a traversal.".into());
        }
        let sort = match &self.sort {
            Some((field, _)) if field == "relevance" && q.is_none() => {
                return Err("Sort by best match needs a text query.".into());
            }
            Some((field, order)) => Some(data::SearchSort {
                field: field.clone(),
                order: order.clone(),
            }),
            None => None,
        };
        Ok(data::SearchRequest {
            q,
            filter: (!filter.is_empty()).then_some(filter),
            traverse,
            sort,
            limit: self.limit,
            offset: Some(self.offset),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splits a query and decodes each percent code once, as the browser
    /// does. Host only: the page reads the pairs that the router decoded.
    fn parse_query(query: &str) -> Vec<(String, String)> {
        fn decode(text: &str) -> String {
            let bytes = text.as_bytes();
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("ascii");
                    out.push(u8::from_str_radix(hex, 16).expect("hex"));
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).expect("utf-8")
        }
        query
            .trim_start_matches('?')
            .split('&')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let (key, value) = part.split_once('=').unwrap_or((part, ""));
                (decode(key), decode(value))
            })
            .collect()
    }

    fn round_trip(state: &SearchState) -> ParsedQuery {
        SearchState::from_pairs(parse_query(&state.to_query()))
    }

    /// A state with each field set, and text that needs percent codes.
    fn full_state() -> SearchState {
        SearchState {
            q: "\"magic link\" auth OR billing & 100% -draft".into(),
            entity_type: vec!["task".into(), "adr".into()],
            task_type: vec!["bug".into(), "support".into()],
            work_class: vec!["planned".into(), "support".into()],
            board: "kairos".into(),
            column: "In Review".into(),
            team: "core-team".into(),
            repository: "kairos".into(),
            claimed_by: "me".into(),
            owned_by: "robin@example.com".into(),
            created_by: "Sam Jones".into(),
            is_bucket: Some(false),
            metadata: vec![
                ("priority".into(), "high".into()),
                ("component".into(), "auth*".into()),
            ],
            metadata_missing: vec!["estimate".into(), "risk".into()],
            created_after: "2026-01-01".into(),
            created_before: "2026-03-01".into(),
            updated_after: "2026-02-01T10:00:00+02:00".into(),
            updated_before: "2026-09-30".into(),
            include_deleted: true,
            traverse: Some(TraverseState {
                from: "DEMO-S-0001".into(),
                relationships: vec!["parent".into(), "blocks".into()],
                direction: "both".into(),
                depth: 4,
            }),
            sort: Some(("updated_at".into(), "asc".into())),
            limit: Some(50),
            offset: 100,
        }
    }

    /// Each field survives the URL: encode, decode, and the same state.
    #[test]
    fn every_field_round_trips() {
        let state = full_state();
        let parsed = round_trip(&state);
        assert_eq!(parsed.problems, Vec::<String>::new());
        assert_eq!(parsed.state, state);
    }

    /// Each field alone also round-trips, so no field depends on another.
    #[test]
    fn each_field_alone_round_trips() {
        let full = full_state();
        let singles = vec![
            SearchState {
                q: full.q.clone(),
                ..Default::default()
            },
            SearchState {
                entity_type: full.entity_type.clone(),
                ..Default::default()
            },
            SearchState {
                task_type: full.task_type.clone(),
                ..Default::default()
            },
            SearchState {
                work_class: full.work_class.clone(),
                ..Default::default()
            },
            SearchState {
                board: full.board.clone(),
                ..Default::default()
            },
            SearchState {
                column: full.column.clone(),
                ..Default::default()
            },
            SearchState {
                team: full.team.clone(),
                ..Default::default()
            },
            SearchState {
                repository: full.repository.clone(),
                ..Default::default()
            },
            SearchState {
                claimed_by: full.claimed_by.clone(),
                ..Default::default()
            },
            SearchState {
                owned_by: full.owned_by.clone(),
                ..Default::default()
            },
            SearchState {
                created_by: full.created_by.clone(),
                ..Default::default()
            },
            SearchState {
                is_bucket: Some(true),
                ..Default::default()
            },
            SearchState {
                is_bucket: Some(false),
                ..Default::default()
            },
            SearchState {
                metadata: full.metadata.clone(),
                ..Default::default()
            },
            SearchState {
                metadata_missing: full.metadata_missing.clone(),
                ..Default::default()
            },
            SearchState {
                created_after: full.created_after.clone(),
                ..Default::default()
            },
            SearchState {
                created_before: full.created_before.clone(),
                ..Default::default()
            },
            SearchState {
                updated_after: full.updated_after.clone(),
                ..Default::default()
            },
            SearchState {
                updated_before: full.updated_before.clone(),
                ..Default::default()
            },
            SearchState {
                include_deleted: true,
                ..Default::default()
            },
            SearchState {
                traverse: full.traverse.clone(),
                ..Default::default()
            },
            SearchState {
                sort: full.sort.clone(),
                ..Default::default()
            },
            SearchState {
                limit: Some(5),
                ..Default::default()
            },
            SearchState {
                offset: 25,
                ..Default::default()
            },
        ];
        for state in singles {
            let parsed = round_trip(&state);
            assert_eq!(parsed.problems, Vec::<String>::new(), "{state:?}");
            assert_eq!(parsed.state, state, "query {}", state.to_query());
        }
    }

    /// The empty state is the bare `/search`: the nav link stays as it is.
    #[test]
    fn the_empty_state_has_no_query() {
        assert_eq!(SearchState::default().to_query(), "");
        assert!(SearchState::default().asks_nothing());
        assert_eq!(
            SearchState::from_pairs(Vec::<(&str, &str)>::new()).state,
            SearchState::default()
        );
    }

    /// The query is readable: API names as keys, lists with commas.
    #[test]
    fn the_query_uses_the_api_names() {
        let state = SearchState {
            q: "sign up".into(),
            entity_type: vec!["task".into(), "adr".into()],
            claimed_by: "me".into(),
            metadata: vec![("priority".into(), "high".into())],
            ..Default::default()
        };
        assert_eq!(
            state.to_query(),
            "?q=sign%20up&entity_type=task%2Cadr&claimed_by=me&meta.priority=high"
        );
    }

    /// An unknown key and a bad value give a sentence; the rest is used.
    #[test]
    fn unknown_and_bad_parts_are_named() {
        let parsed = SearchState::from_pairs([
            ("q", "auth"),
            ("colour", "red"),
            ("depth", "99"),
            ("is_bucket", "maybe"),
            ("limit", "0"),
            ("order", "asc"),
        ]);
        assert_eq!(parsed.state.q, "auth");
        assert_eq!(parsed.state.is_bucket, None);
        assert_eq!(parsed.state.traverse, None);
        assert_eq!(parsed.state.sort, None);
        assert_eq!(parsed.problems.len(), 6, "{:?}", parsed.problems);
        assert!(parsed.problems.iter().any(|p| p.contains("\"colour\"")));
        assert!(parsed.problems.iter().any(|p| p.contains("\"depth\"")));
        assert!(parsed.problems.iter().any(|p| p.contains("no \"from\"")));
        assert!(parsed.problems.iter().any(|p| p.contains("no \"sort\"")));
    }

    /// A sort with no order is newest first.
    #[test]
    fn a_sort_without_an_order_is_descending() {
        let parsed = SearchState::from_pairs([("q", "x"), ("sort", "title")]);
        assert_eq!(parsed.state.sort, Some(("title".into(), "desc".into())));
    }

    /// The request has each filter of the state, with the API names; days
    /// become instants, and an end day is in the range.
    #[test]
    fn the_request_has_every_filter() {
        let request = full_state().to_request().expect("a valid state");
        let value = serde_json::to_value(&request).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "q": "\"magic link\" auth OR billing & 100% -draft",
                "filter": {
                    "entity_type": ["task", "adr"],
                    "board": "kairos",
                    "column": "In Review",
                    "team": "core-team",
                    "repository": "kairos",
                    "task_type": ["bug", "support"],
                    "work_class": ["planned", "support"],
                    "is_bucket": false,
                    "metadata": {"component": "auth*", "priority": "high"},
                    "created_after": "2026-01-01T00:00:00Z",
                    "created_before": "2026-03-01T23:59:59Z",
                    "updated_after": "2026-02-01T10:00:00+02:00",
                    "updated_before": "2026-09-30T23:59:59Z",
                    "metadata_missing": ["estimate", "risk"],
                    "claimed_by": "me",
                    "owned_by": "robin@example.com",
                    "created_by": "Sam Jones",
                    "include_deleted": true
                },
                "traverse": {
                    "from": {"short_code": "DEMO-S-0001"},
                    "relationships": ["parent", "blocks"],
                    "direction": "both",
                    "depth": 4
                },
                "sort": {"field": "updated_at", "order": "asc"},
                "limit": 50,
                "offset": 100
            })
        );
    }

    /// The request refuses what the server would refuse, in words.
    #[test]
    fn the_request_names_what_is_missing() {
        assert!(SearchState::default().to_request().is_err());
        let no_text = SearchState {
            board: "kairos".into(),
            sort: Some(("relevance".into(), "desc".into())),
            ..Default::default()
        };
        assert_eq!(
            no_text.to_request().unwrap_err(),
            "Sort by best match needs a text query."
        );
        let no_start = SearchState {
            traverse: Some(TraverseState::default()),
            ..Default::default()
        };
        assert!(no_start.to_request().unwrap_err().contains("start item"));
        let no_relationship = SearchState {
            traverse: Some(TraverseState {
                from: "demo-s-0001".into(),
                relationships: Vec::new(),
                ..TraverseState::default()
            }),
            ..Default::default()
        };
        assert!(
            no_relationship
                .to_request()
                .unwrap_err()
                .contains("relationship")
        );
        // Only the flag for put-away work is a full search.
        let put_away = SearchState {
            include_deleted: true,
            ..Default::default()
        };
        assert!(put_away.to_request().is_ok());
        assert!(!put_away.asks_nothing());
    }
}
