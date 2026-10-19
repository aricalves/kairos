//! The scope of an agent key (KAIROS-T-0536): a list of capabilities, and
//! an optional list of boards. Pure: no diesel, no I/O.
//!
//! A request with the key can do an action only when two things are true:
//! the person can do it, and the scope lets the key do it. So a scope never
//! adds a right. It only takes rights away.
//!
//! The rules of [`KeyScope::allows`]:
//!
//! - A capability of the scope vocabulary ([`SCOPE_CAPABILITIES`]) must be
//!   in the list of the scope.
//! - The work that only the role of an organization admin gives (accounts,
//!   keys, SCIM tokens, the tenant, templates, metadata definitions,
//!   delivery streams, forge connections, the grants of `manage_structure`)
//!   has no capability of its own. The server names it with a
//!   pseudo-capability (`manage_service_accounts`, ...). The scope treats
//!   each such name as [`MANAGE_STRUCTURE`].
//! - With a list of boards, an action on a board needs the board in the
//!   list. An action with no board is outside such a scope.
//!
//! A key with no scope (each key made before KAIROS-T-0536) has no limit:
//! the server keeps `None` for it, not a [`KeyScope`].

use std::collections::BTreeSet;

use uuid::Uuid;

use crate::abac::{CAPABILITIES, FILE_BACKLOG, MANAGE_STRUCTURE, PURGE};

/// The capabilities that a scope can name: the grantable vocabulary
/// ([`CAPABILITIES`]), [`MANAGE_STRUCTURE`] and the computed
/// [`FILE_BACKLOG`]. Each other name is refused.
pub fn scope_capabilities() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = CAPABILITIES.to_vec();
    all.push(MANAGE_STRUCTURE);
    all.push(FILE_BACKLOG);
    all
}

/// Is `name` a capability that a scope can name?
pub fn is_scope_capability(name: &str) -> bool {
    CAPABILITIES.contains(&name) || name == MANAGE_STRUCTURE || name == FILE_BACKLOG
}

/// The capabilities that a scope can name, as a constant list.
pub const SCOPE_CAPABILITIES_LEN: usize = CAPABILITIES.len() + 2;

/// The capabilities that the default scope of a new agent key leaves out.
pub const DEFAULT_SCOPE_EXCLUDES: &[&str] = &[PURGE, MANAGE_STRUCTURE];

/// The capabilities of the default scope of a new agent key: each one of
/// [`scope_capabilities`] but `purge` and `manage_structure`.
pub fn default_scope_capabilities() -> Vec<&'static str> {
    scope_capabilities()
        .into_iter()
        .filter(|c| !DEFAULT_SCOPE_EXCLUDES.contains(c))
        .collect()
}

/// The scope of an agent key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyScope {
    /// The capabilities that the key can use. Sorted, no duplicates. Empty:
    /// a read-only key.
    pub capabilities: BTreeSet<String>,
    /// The boards where the key can act. `None`: each board.
    pub boards: Option<BTreeSet<Uuid>>,
}

/// Why [`KeyScope::allows`] refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeGap {
    /// The scope does not have this capability. For the work of the admin
    /// role, this is `manage_structure`.
    Capability(&'static str),
    /// The capability is in the scope, but the board is not.
    Board,
    /// The scope has a list of boards, and the action has no board.
    NoBoard,
}

impl KeyScope {
    /// The default scope of a new agent key: [`default_scope_capabilities`]
    /// on each board.
    pub fn default_for_agent_key() -> Self {
        Self {
            capabilities: default_scope_capabilities()
                .into_iter()
                .map(str::to_string)
                .collect(),
            boards: None,
        }
    }

    /// The capability that the scope must have for `required`: `required`
    /// itself when the vocabulary has it, else [`MANAGE_STRUCTURE`] (the
    /// work of the admin role, module docs).
    pub fn needed(required: &str) -> &str {
        if is_scope_capability(required) {
            required
        } else {
            MANAGE_STRUCTURE
        }
    }

    /// `None` when the scope lets the key do `required` on `board` (`None`
    /// = the action has no board). Else the reason.
    pub fn gap(&self, required: &str, board: Option<Uuid>) -> Option<ScopeGap> {
        let needed = Self::needed(required);
        if !self.capabilities.contains(needed) {
            let named = scope_capabilities()
                .into_iter()
                .find(|c| *c == needed)
                .unwrap_or(MANAGE_STRUCTURE);
            return Some(ScopeGap::Capability(named));
        }
        match (&self.boards, board) {
            (None, _) => None,
            (Some(boards), Some(board)) if boards.contains(&board) => None,
            (Some(_), Some(_)) => Some(ScopeGap::Board),
            (Some(_), None) => Some(ScopeGap::NoBoard),
        }
    }

    /// Does the scope let the key do `required` on `board`?
    pub fn allows(&self, required: &str, board: Option<Uuid>) -> bool {
        self.gap(required, board).is_none()
    }
}

/// Check the capabilities of a requested scope: each name must be in
/// [`scope_capabilities`]. Returns the sorted set, or the first unknown
/// name. A glob (`*`, `manage_*`) is not a name of the scope.
pub fn parse_capabilities<S: AsRef<str>>(names: &[S]) -> Result<BTreeSet<String>, String> {
    let mut set = BTreeSet::new();
    for name in names {
        let name = name.as_ref().trim();
        if !is_scope_capability(name) {
            return Err(name.to_string());
        }
        set.insert(name.to_string());
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abac::{
        ADMINISTER_MEMBERS, APPROVE_PROPOSALS, CONFIGURE_BOARDS, MANAGE_TASKS, MEMBER, PAGES,
        TRANSITION_ITEMS,
    };

    fn scope(caps: &[&str], boards: Option<&[Uuid]>) -> KeyScope {
        KeyScope {
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            boards: boards.map(|b| b.iter().copied().collect()),
        }
    }

    #[test]
    fn the_default_scope_is_each_capability_but_purge_and_manage_structure() {
        let default = KeyScope::default_for_agent_key();
        assert_eq!(default.capabilities.len(), SCOPE_CAPABILITIES_LEN - 2);
        assert!(default.boards.is_none());
        for capability in scope_capabilities() {
            let held = default.capabilities.contains(capability);
            assert_eq!(
                held,
                capability != PURGE && capability != MANAGE_STRUCTURE,
                "{capability}"
            );
        }
        for capability in [
            MANAGE_TASKS,
            TRANSITION_ITEMS,
            CONFIGURE_BOARDS,
            ADMINISTER_MEMBERS,
            APPROVE_PROPOSALS,
            PAGES,
            MEMBER,
            FILE_BACKLOG,
        ] {
            assert!(
                default.allows(capability, Some(Uuid::nil())),
                "{capability}"
            );
        }
        assert!(!default.allows(PURGE, Some(Uuid::nil())));
        assert!(!default.allows(MANAGE_STRUCTURE, None));
        assert_eq!(default.gap(PURGE, None), Some(ScopeGap::Capability(PURGE)));
    }

    #[test]
    fn the_work_of_the_admin_role_needs_manage_structure() {
        let default = KeyScope::default_for_agent_key();
        assert_eq!(
            KeyScope::needed("manage_service_accounts"),
            MANAGE_STRUCTURE
        );
        assert_eq!(
            default.gap("manage_service_accounts", None),
            Some(ScopeGap::Capability(MANAGE_STRUCTURE))
        );
        let full = scope(&scope_capabilities(), None);
        assert!(full.allows("manage_service_accounts", None));
    }

    #[test]
    fn a_read_only_scope_allows_nothing() {
        let read_only = scope(&[], None);
        for capability in scope_capabilities() {
            assert!(
                !read_only.allows(capability, Some(Uuid::nil())),
                "{capability}"
            );
        }
        assert!(!read_only.allows("manage_org_members", None));
    }

    #[test]
    fn a_list_of_boards_limits_each_action() {
        let mine = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);
        let s = scope(&[MANAGE_TASKS, MANAGE_STRUCTURE], Some(&[mine]));
        assert!(s.allows(MANAGE_TASKS, Some(mine)));
        assert_eq!(s.gap(MANAGE_TASKS, Some(other)), Some(ScopeGap::Board));
        assert_eq!(s.gap(MANAGE_TASKS, None), Some(ScopeGap::NoBoard));
        assert_eq!(s.gap(MANAGE_STRUCTURE, None), Some(ScopeGap::NoBoard));
        assert_eq!(
            s.gap(TRANSITION_ITEMS, Some(mine)),
            Some(ScopeGap::Capability(TRANSITION_ITEMS))
        );
    }

    #[test]
    fn an_unknown_capability_or_a_glob_is_refused_and_named() {
        assert_eq!(
            parse_capabilities(&["manage_tasks", "launch_rockets"]),
            Err("launch_rockets".to_string())
        );
        assert_eq!(parse_capabilities(&["*"]), Err("*".to_string()));
        assert_eq!(
            parse_capabilities(&["manage_*"]),
            Err("manage_*".to_string())
        );
        let set = parse_capabilities(&["purge", "manage_tasks", "purge"]).expect("known");
        assert_eq!(
            set.into_iter().collect::<Vec<_>>(),
            vec!["manage_tasks".to_string(), "purge".to_string()]
        );
        assert_eq!(scope_capabilities().len(), SCOPE_CAPABILITIES_LEN);
    }
}
