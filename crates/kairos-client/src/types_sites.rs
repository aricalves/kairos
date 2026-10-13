//! The published site of a repository (KAIROS-T-0406, KAIROS-D-0012).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The site of a repository, on [`crate::types_repositories::Repository`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RepositorySite {
    /// The site switch. Off: a publish is refused, and the site is not
    /// served.
    pub enabled: bool,
    /// The path where Kairos serves the site, and the base path that its
    /// build must use, e.g. `/pages/acme/platform/repos/payments-api/`.
    pub base_path: String,
    /// The live publish, or `null`.
    #[serde(default)]
    pub published: Option<SitePublish>,
    /// True when the live publish was built for a different base path (the
    /// repository moved to another team, or a slug changed): publish again.
    #[serde(default)]
    pub needs_republish: bool,
}

/// One publish of a site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SitePublish {
    /// Publish id (UUID).
    pub publish_id: String,
    /// The slug of the repository at the time of the publish.
    pub repository_slug: String,
    /// The base path the site was published for.
    pub base_path: String,
    /// MD5 of the uploaded archive, lower-case hex.
    pub md5: String,
    /// Bytes of the archive.
    pub size: i64,
    /// Bytes of the files.
    pub unpacked_size: i64,
    /// Entries of the archive (files and folders).
    pub files: i32,
    /// The user or service account that sent it (UUID).
    pub principal_id: String,
    /// The agent key that sent it (UUID), if one did.
    #[serde(default)]
    pub agent_key_id: Option<String>,
    /// RFC 3339.
    pub published_at: String,
    /// `live`, `replaced` or `unpublished`.
    pub state: String,
}
