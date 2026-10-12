//! The form of a slug that a caller sends for a board, a team or a
//! delivery stream (COLLIERY-T-0258, COLLIERY-T-0260). One function has
//! the rule, so the three kinds cannot go apart.
//!
//! A repository has a different rule
//! ([`crate::repositories::is_valid_slug`]), and an organization has the
//! rule of its schema name (`kairos_db::tenant::is_valid_tenant_slug`).
//! SCIM makes a team from the group `kairos-team-<slug>`, and that slug
//! has the rule of this module (COLLIERY-T-0265).

/// The form of a slug, as the refusal gives it.
pub const SLUG_RULE: &str = "^[a-z][a-z0-9_-]{1,62}$";

/// Whether `slug` has the form of a slug of a board, a team or a delivery
/// stream: [`SLUG_RULE`], the rule of an organization slug, so that
/// `<team-slug>-delivery` has only characters that the rule permits. A
/// slug with the form of a UUID does not pass: a reference to a board is
/// read as an id first (KAIROS-T-0116), so no request could reach that
/// board by its slug.
///
/// The rule is for a slug that a caller SENDS. The server does not apply
/// it to the slug that it makes for the delivery board of a team, and a
/// board, a team or a delivery stream that has a different slug from
/// before the rule stays as it is.
pub fn is_valid_slug(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    (2..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
        && !crate::repositories::looks_like_uuid(slug)
}

/// The names that the docs sites use as path segments (KAIROS-D-0013):
/// `/pages/<tenant>/docs/...`, `/pages/<tenant>/adrs/...` and
/// `/pages/<tenant>/<team>/repos/...`. A team cannot have one as its slug,
/// and a team page or folder at the top of the tree cannot either.
pub const SITE_RESERVED_SLUGS: &[&str] = &["docs", "adrs", "repos"];

/// Whether `slug` is one of [`SITE_RESERVED_SLUGS`].
pub fn is_site_reserved(slug: &str) -> bool {
    SITE_RESERVED_SLUGS.contains(&slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KAIROS-T-0403: the three names of the docs sites.
    #[test]
    fn site_reserved_slugs() {
        for slug in ["docs", "adrs", "repos"] {
            assert!(is_site_reserved(slug), "{slug:?} is reserved");
        }
        for slug in ["doc", "docs-team", "runbooks", "Docs"] {
            assert!(!is_site_reserved(slug), "{slug:?} is not reserved");
        }
    }

    /// COLLIERY-T-0258, COLLIERY-T-0260: the form of a slug.
    #[test]
    fn slug_form() {
        for slug in ["roadmap", "platform-delivery", "road_map", "ab", "a1"] {
            assert!(is_valid_slug(slug), "{slug:?} is a slug");
        }
        assert!(is_valid_slug(&"a".repeat(63)));
        for slug in [
            "",
            "a",
            "Roadmap",
            "road map",
            "9lives",
            "-roadmap",
            "_roadmap",
            "road.map",
            "road/map",
            "roadmäp",
            " roadmap",
            "roadmap ",
            "road\nmap",
        ] {
            assert!(!is_valid_slug(slug), "{slug:?} is not a slug");
        }
        assert!(!is_valid_slug(&"a".repeat(64)));
        // A reference to a board is read as an id first.
        assert!(!is_valid_slug("abcdef12-0000-7000-8000-000000000003"));
        assert!(is_valid_slug("abcdef12-0000-7000-8000-00000000000x"));
    }

    /// COLLIERY-T-0260: the rule that the refusal gives is the rule that
    /// the function applies, at the limits of the length.
    #[test]
    fn the_rule_text_gives_the_limits_of_the_function() {
        assert_eq!(SLUG_RULE, "^[a-z][a-z0-9_-]{1,62}$");
        // One first character and 1 to 62 more: 2 to 63 characters.
        assert!(!is_valid_slug("a"));
        assert!(is_valid_slug("ab"));
        assert!(is_valid_slug(&format!("a{}", "9".repeat(62))));
        assert!(!is_valid_slug(&format!("a{}", "9".repeat(63))));
    }
}
