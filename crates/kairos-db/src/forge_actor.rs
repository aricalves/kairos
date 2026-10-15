//! The forge system actor of a tenant (KAIROS-T-0481, COLLIERY-I-0623).
//!
//! Work that comes from a forge (an issue that becomes a task, a webhook
//! event that records a branch or a pull request) is done by one system
//! actor per tenant, named "Forge", and not by the person who connected the
//! repository. The actor is a `public.users` row with `kind = 'system'`
//! and one `organization_members` row (role `member`), so it has the right
//! of each member: it can send a request to any delivery board.
//!
//! It is not a login. The database refuses it a password
//! (`users_system_has_no_password`); the server refuses it an OIDC login
//! and an API key ([`is_system_user`]).

use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use crate::models::public::USER_KIND_SYSTEM;
use crate::models::{NewOrganizationMember, OrgRole};
use crate::schema::{organization_members, organizations, users};

/// The display name of the forge actor.
pub const FORGE_ACTOR_NAME: &str = "Forge";

/// The synthetic `external_id` of the forge actor of an organization. The
/// `system:` prefix is not an OIDC subject; the login refuses the row in
/// any case.
pub fn external_id(org_id: Uuid) -> String {
    format!("system:forge:{org_id}")
}

/// The forge actor of the organization, created with its membership when it
/// is not there. Safe to call from two deliveries at once.
pub fn ensure(conn: &mut PgConnection, org_id: Uuid) -> QueryResult<Uuid> {
    let external = external_id(org_id);
    let existing: Option<Uuid> = users::table
        .filter(users::external_id.eq(&external))
        .select(users::id)
        .first(conn)
        .optional()?;
    let id = match existing {
        Some(id) => id,
        None => {
            diesel::insert_into(users::table)
                .values((
                    users::external_id.eq(&external),
                    users::user_name.eq(&external),
                    // A non-routable placeholder (RFC 2606 `.invalid`), unique
                    // per organization.
                    users::email.eq(format!("forge+{org_id}@system.invalid")),
                    users::display_name.eq(FORGE_ACTOR_NAME),
                    users::kind.eq(USER_KIND_SYSTEM),
                ))
                .on_conflict(users::external_id)
                .do_nothing()
                .execute(conn)?;
            users::table
                .filter(users::external_id.eq(&external))
                .select(users::id)
                .first(conn)?
        }
    };
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: id,
            role: OrgRole::Member,
        })
        .on_conflict_do_nothing()
        .execute(conn)?;
    Ok(id)
}

/// The forge actor of the organization with this slug.
pub fn ensure_for_slug(conn: &mut PgConnection, slug: &str) -> QueryResult<Uuid> {
    let org_id: Uuid = organizations::table
        .filter(organizations::slug.eq(slug))
        .select(organizations::id)
        .first(conn)?;
    ensure(conn, org_id)
}

/// Whether the user is a system actor.
pub fn is_system_user(conn: &mut PgConnection, user_id: Uuid) -> QueryResult<bool> {
    let kind: Option<String> = users::table
        .filter(users::id.eq(user_id))
        .select(users::kind)
        .first(conn)
        .optional()?;
    Ok(kind.as_deref() == Some(USER_KIND_SYSTEM))
}
