//! The auth cache (KAIROS-T-0609): the lookups of the auth and tenant
//! layers, kept for a short time.
//!
//! # Design
//!
//! Five maps, one TTL (`KAIROS_AUTH_CACHE_SECS`, 60 s by default, 0 = off):
//!
//! | map | key | value | what it saves |
//! |---|---|---|---|
//! | `keys` | SHA-256 of the API key | principal, tenant, key id, expiry | the throttle read, the org, the key and the user reads |
//! | `subjects` | OIDC `sub` | the user row | the JIT read |
//! | `sessions` | SHA-256 of the session token | principal, user id, expiry | the session and the user reads |
//! | `orgs` | tenant slug | org id | the org read |
//! | `roles` | (org id, user id) | the org role | the membership read |
//!
//! No map holds a refusal: an unknown key, a missing membership or an unknown
//! slug is read again on each request. So an added member works at once, and
//! a bad key is never cached.
//!
//! An entry is dropped by the write that makes it wrong, after the commit:
//!
//! - a key revoke or rotation drops the key;
//! - a role change drops the (org, user) role;
//! - a member removal, a service-account delete and a revoke of every
//!   session drop each entry of the user;
//! - a logout drops the session;
//! - a tenant delete drops the slug.
//!
//! The cached role is the role of the tenant layer (`TenantContext::role`).
//! A capability check reads the role from the database (KAIROS-A-0006), so
//! it sees a change at once in each case.
//!
//! So on the server that made the write, the next request sees it. On a
//! deployment with several servers, the other servers keep the entry until it
//! expires: at most `KAIROS_AUTH_CACHE_SECS`. An operator who cannot accept
//! that lag sets it lower, or 0.
//!
//! A cached key skips the throttle read (`crate::rate_limit`). The throttle
//! stops guessing; a key in the cache was accepted, not guessed. A locked-out
//! source can therefore use a key that was accepted in the last minute.
//!
//! A cached key or session still checks its expiry at the hit, so a key that
//! expires inside the TTL stops on time. The `last_used_at` stamps
//! (KAIROS-T-0610) are not written on a hit: they lag by at most one TTL
//! more.
//!
//! Each map holds at most [`CAPACITY`] entries. A full map drops its expired
//! entries, then everything: a miss is the correct answer, a bigger map is
//! not.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use kairos_db::models::{OrgRole, User};
use uuid::Uuid;

use crate::middleware::auth::AuthContext;

/// The most entries of one map.
pub const CAPACITY: usize = 10_000;

struct Entry<V> {
    value: V,
    expires: Instant,
}

/// A map whose entries expire, behind a mutex. The critical sections are a
/// hash lookup or a scan of at most [`CAPACITY`] entries.
struct TtlMap<K, V> {
    inner: Mutex<HashMap<K, Entry<V>>>,
    ttl: Duration,
}

impl<K: Eq + Hash, V: Clone> TtlMap<K, V> {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    fn get(&self, key: &K) -> Option<V> {
        if self.ttl.is_zero() {
            return None;
        }
        let mut map = self.inner.lock().expect("the auth cache lock");
        match map.get(key) {
            Some(entry) if entry.expires > Instant::now() => Some(entry.value.clone()),
            Some(_) => {
                map.remove(key);
                None
            }
            None => None,
        }
    }

    fn insert(&self, key: K, value: V) {
        if self.ttl.is_zero() {
            return;
        }
        let now = Instant::now();
        let mut map = self.inner.lock().expect("the auth cache lock");
        if map.len() >= CAPACITY && !map.contains_key(&key) {
            map.retain(|_, entry| entry.expires > now);
            if map.len() >= CAPACITY {
                map.clear();
            }
        }
        map.insert(
            key,
            Entry {
                value,
                expires: now + self.ttl,
            },
        );
    }

    fn remove(&self, key: &K) {
        self.inner.lock().expect("the auth cache lock").remove(key);
    }

    fn retain(&self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.inner
            .lock()
            .expect("the auth cache lock")
            .retain(|key, entry| keep(key, &entry.value));
    }

    fn clear(&self) {
        self.inner.lock().expect("the auth cache lock").clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().expect("the auth cache lock").len()
    }
}

/// A cached API key: what `authenticate_api_key` found.
#[derive(Debug, Clone)]
pub struct CachedKey {
    /// The principal of the key.
    pub auth: AuthContext,
    /// The tenant of the key: its slug and the org id.
    pub slug: String,
    pub org_id: Uuid,
    /// `api_keys.id`, for a drop on revoke.
    pub key_id: Uuid,
    /// The expiry of the key, checked at each hit.
    pub expires_at: Option<DateTime<Utc>>,
}

/// A cached session: what `authenticate_session` found.
#[derive(Debug, Clone)]
pub struct CachedSession {
    pub auth: AuthContext,
    /// The expiry of the session, checked at each hit.
    pub expires_at: DateTime<Utc>,
}

/// The cache of the auth and tenant layers. One per [`crate::app::AppState`].
pub struct AuthCache {
    keys: TtlMap<String, CachedKey>,
    subjects: TtlMap<String, User>,
    sessions: TtlMap<String, CachedSession>,
    orgs: TtlMap<String, Uuid>,
    roles: TtlMap<(Uuid, Uuid), OrgRole>,
    ttl: Duration,
}

impl std::fmt::Debug for AuthCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthCache")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl AuthCache {
    /// A cache whose entries live `ttl`. A zero `ttl` caches nothing.
    pub fn new(ttl: Duration) -> Self {
        Self {
            keys: TtlMap::new(ttl),
            subjects: TtlMap::new(ttl),
            sessions: TtlMap::new(ttl),
            orgs: TtlMap::new(ttl),
            roles: TtlMap::new(ttl),
            ttl,
        }
    }

    /// Whether the cache holds anything at all.
    pub fn is_enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    /// The key with this hash, when it is cached and not expired.
    pub fn key(&self, hash: &str) -> Option<CachedKey> {
        let key = self.keys.get(&hash.to_string())?;
        if key.expires_at.is_some_and(|at| at <= Utc::now()) {
            self.keys.remove(&hash.to_string());
            return None;
        }
        Some(key)
    }

    pub fn put_key(&self, hash: String, key: CachedKey) {
        self.keys.insert(hash, key);
    }

    /// The user of this OIDC subject.
    pub fn subject(&self, sub: &str) -> Option<User> {
        self.subjects.get(&sub.to_string())
    }

    pub fn put_subject(&self, sub: String, user: User) {
        self.subjects.insert(sub, user);
    }

    /// The session with this token hash, when it is cached and not expired.
    pub fn session(&self, hash: &str) -> Option<CachedSession> {
        let session = self.sessions.get(&hash.to_string())?;
        if session.expires_at <= Utc::now() {
            self.sessions.remove(&hash.to_string());
            return None;
        }
        Some(session)
    }

    pub fn put_session(&self, hash: String, session: CachedSession) {
        self.sessions.insert(hash, session);
    }

    /// The org id of this slug.
    pub fn org(&self, slug: &str) -> Option<Uuid> {
        self.orgs.get(&slug.to_string())
    }

    pub fn put_org(&self, slug: String, org_id: Uuid) {
        self.orgs.insert(slug, org_id);
    }

    /// The role of the user in the org.
    pub fn role(&self, org_id: Uuid, user_id: Uuid) -> Option<OrgRole> {
        self.roles.get(&(org_id, user_id))
    }

    pub fn put_role(&self, org_id: Uuid, user_id: Uuid, role: OrgRole) {
        self.roles.insert((org_id, user_id), role);
    }

    /// A key is revoked or rotated: drop it.
    pub fn drop_key(&self, key_id: Uuid) {
        self.keys.retain(|_, key| key.key_id != key_id);
    }

    /// A session is revoked: drop it.
    pub fn drop_session(&self, hash: &str) {
        self.sessions.remove(&hash.to_string());
    }

    /// The role of the user in the org changed: drop it.
    pub fn drop_role(&self, org_id: Uuid, user_id: Uuid) {
        self.roles.remove(&(org_id, user_id));
    }

    /// Roles of the org changed in bulk (a SCIM group write): drop them all.
    pub fn drop_org_roles(&self, org_id: Uuid) {
        self.roles.retain(|(org, _), _| *org != org_id);
    }

    /// Every session of the user is revoked (a password change, an admin's
    /// revoke): drop them.
    pub fn drop_sessions(&self, user_id: Uuid) {
        self.sessions
            .retain(|_, session| session.auth.user_id != user_id);
    }

    /// The user left, was deleted, or lost every session: drop each entry
    /// that names the user.
    pub fn drop_user(&self, user_id: Uuid) {
        self.keys.retain(|_, key| key.auth.user_id != user_id);
        self.sessions
            .retain(|_, session| session.auth.user_id != user_id);
        self.subjects.retain(|_, user| user.id != user_id);
        self.roles.retain(|(_, user), _| *user != user_id);
    }

    /// The tenant is gone: drop its slug. Its roles expire; a new tenant
    /// with the slug has a new id.
    pub fn drop_org(&self, slug: &str) {
        self.orgs.remove(&slug.to_string());
    }

    /// Drop everything. For tests and for an operator's restart-free reset.
    pub fn clear(&self) {
        self.keys.clear();
        self.subjects.clear();
        self.sessions.clear();
        self.orgs.clear();
        self.roles.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(user_id: Uuid) -> AuthContext {
        AuthContext {
            user_id,
            external_id: "sub".into(),
            email: "robin@example.test".into(),
            display_name: "Robin".into(),
            email_verified: false,
            agent_key: None,
        }
    }

    fn key(user_id: Uuid, key_id: Uuid, expires_at: Option<DateTime<Utc>>) -> CachedKey {
        CachedKey {
            auth: auth(user_id),
            slug: "acme".into(),
            org_id: Uuid::new_v4(),
            key_id,
            expires_at,
        }
    }

    #[test]
    fn an_entry_expires_with_the_ttl() {
        let map: TtlMap<&str, u32> = TtlMap::new(Duration::from_millis(20));
        map.insert("a", 1);
        assert_eq!(map.get(&"a"), Some(1));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(map.get(&"a"), None);
        assert_eq!(map.len(), 0, "an expired entry is removed on the read");
    }

    #[test]
    fn a_zero_ttl_caches_nothing() {
        let cache = AuthCache::new(Duration::ZERO);
        assert!(!cache.is_enabled());
        cache.put_org("acme".into(), Uuid::new_v4());
        assert_eq!(cache.org("acme"), None);
    }

    #[test]
    fn a_full_map_drops_its_expired_entries_then_everything() {
        let map: TtlMap<usize, usize> = TtlMap::new(Duration::from_secs(60));
        for i in 0..CAPACITY {
            map.insert(i, i);
        }
        assert_eq!(map.len(), CAPACITY);
        map.insert(CAPACITY, CAPACITY);
        assert_eq!(map.len(), 1, "nothing had expired, so the map was cleared");
        assert_eq!(map.get(&CAPACITY), Some(CAPACITY));
    }

    #[test]
    fn a_key_past_its_expiry_is_a_miss() {
        let cache = AuthCache::new(Duration::from_secs(60));
        let id = Uuid::new_v4();
        cache.put_key(
            "h".into(),
            key(id, id, Some(Utc::now() - chrono::Duration::seconds(1))),
        );
        assert!(cache.key("h").is_none());
    }

    #[test]
    fn drops_remove_what_they_name_and_nothing_else() {
        let cache = AuthCache::new(Duration::from_secs(60));
        let robin = Uuid::new_v4();
        let sam = Uuid::new_v4();
        let (k1, k2, k3) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let org = Uuid::new_v4();
        cache.put_key("k1".into(), key(robin, k1, None));
        cache.put_key("k2".into(), key(robin, k2, None));
        cache.put_key("k3".into(), key(sam, k3, None));
        cache.put_role(org, robin, OrgRole::Admin);
        cache.put_role(org, sam, OrgRole::Member);
        cache.put_session(
            "s1".into(),
            CachedSession {
                auth: auth(robin),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            },
        );

        cache.drop_key(k1);
        assert!(cache.key("k1").is_none());
        assert!(cache.key("k2").is_some());

        cache.drop_role(org, robin);
        assert_eq!(cache.role(org, robin), None);
        assert_eq!(cache.role(org, sam), Some(OrgRole::Member));

        cache.drop_session("s1");
        assert!(cache.session("s1").is_none());

        cache.put_role(org, robin, OrgRole::Admin);
        cache.drop_user(robin);
        assert!(cache.key("k2").is_none());
        assert_eq!(cache.role(org, robin), None);
        assert!(cache.key("k3").is_some(), "Sam keeps the entries of Sam");
        assert_eq!(cache.role(org, sam), Some(OrgRole::Member));

        cache.drop_org_roles(org);
        assert_eq!(cache.role(org, sam), None);
    }
}
