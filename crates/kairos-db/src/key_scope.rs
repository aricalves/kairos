//! The scope of the agent key of the request (KAIROS-T-0536), on the
//! thread that runs the queries of the request.
//!
//! The server runs each query of a request in a closure on a blocking
//! thread (`kairos_server::blocking`). It sets the scope of the key around
//! the closure with [`with_scope`], as it sets the agent key itself. The
//! checks of [`crate::abac`] and [`crate::proposals`] read it with
//! [`allows`], so no service takes the scope as a parameter.
//!
//! The scope limits the principal of the key only. A check for a
//! different user (for example: can the target of a hand-off move the
//! task?) does not see it.
//!
//! Outside [`with_scope`] there is no scope: no limit. A job, a test and
//! a request with no agent key, or with a key made before KAIROS-T-0536,
//! see none.

use std::cell::RefCell;
use std::sync::Arc;

use kairos_core::key_scope::KeyScope;
use uuid::Uuid;

/// The principal of the key, and the scope of the key.
pub type RequestScope = (Uuid, Arc<KeyScope>);

thread_local! {
    static SCOPE: RefCell<Option<RequestScope>> = const { RefCell::new(None) };
}

/// Run `f` with `scope` as the scope of the key on this thread. The scope
/// that was there before comes back after `f`, also when `f` panics.
pub fn with_scope<T>(scope: Option<RequestScope>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<RequestScope>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let before = self.0.take();
            SCOPE.with(|s| *s.borrow_mut() = before);
        }
    }
    let before = SCOPE.with(|s| s.replace(scope));
    let _restore = Restore(before);
    f()
}

/// The principal and the scope of the key on this thread, or `None`.
pub fn current() -> Option<RequestScope> {
    SCOPE.with(|s| s.borrow().clone())
}

/// The scope that limits `user` on this thread: the scope of the key when
/// `user` is its principal, else `None`.
pub fn scope_of(user: Uuid) -> Option<Arc<KeyScope>> {
    SCOPE.with(|s| {
        s.borrow()
            .as_ref()
            .filter(|(principal, _)| *principal == user)
            .map(|(_, scope)| scope.clone())
    })
}

/// Does the scope on this thread let `user` do `required` on `board`? True
/// when there is no scope, or when `user` is not the principal of the key.
pub fn allows(user: Uuid, required: &str, board: Option<Uuid>) -> bool {
    scope_of(user).is_none_or(|scope| scope.allows(required, board))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scope_holds_inside_with_scope_and_for_its_principal_only() {
        let alice = Uuid::from_u128(1);
        let bob = Uuid::from_u128(2);
        assert!(current().is_none());
        assert!(allows(alice, "purge", None));
        let scope = Arc::new(KeyScope::default_for_agent_key());
        with_scope(Some((alice, scope)), || {
            assert!(!allows(alice, "purge", Some(Uuid::nil())));
            assert!(allows(alice, "manage_tasks", Some(Uuid::nil())));
            assert!(
                allows(bob, "purge", Some(Uuid::nil())),
                "bob is not the principal"
            );
            with_scope(None, || assert!(allows(alice, "purge", None)));
            assert!(!allows(alice, "purge", None));
        });
        assert!(current().is_none());
        assert!(allows(alice, "purge", None));
    }
}
