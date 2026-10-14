//! `GET /version` (KAIROS-T-0432): the version of the server and where the
//! CLI of that version is, for `kairos self-update`. Open, like the probes.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::app::AppState;

/// The GitHub releases of Kairos, when `KAIROS_CLI_DOWNLOAD_URL` is not set.
pub const DEFAULT_DOWNLOAD_URL: &str = "https://github.com/colliery-io/kairos/releases";

/// The targets that the release workflow builds the CLI for.
pub const CLI_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
];

/// The body of `GET /version`.
#[derive(Debug, Serialize)]
pub struct VersionResponse {
    pub version: &'static str,
    /// One tarball of the CLI of this version for each target.
    pub cli: Vec<CliDownload>,
}

/// The tarball of the CLI for one target, and its checksum.
#[derive(Debug, Serialize)]
pub struct CliDownload {
    pub target: &'static str,
    pub url: String,
    pub sha256_url: String,
}

/// The response for a download base URL (pure; testable).
pub fn response(download_url: Option<&str>) -> VersionResponse {
    let version = env!("CARGO_PKG_VERSION");
    let base = download_url.unwrap_or(DEFAULT_DOWNLOAD_URL);
    let cli = CLI_TARGETS
        .iter()
        .map(|target| {
            let url = format!("{base}/download/v{version}/kairos-{version}-{target}.tar.gz");
            CliDownload {
                target,
                sha256_url: format!("{url}.sha256"),
                url,
            }
        })
        .collect();
    VersionResponse { version, cli }
}

pub(crate) async fn version(State(state): State<AppState>) -> Json<VersionResponse> {
    Json(response(state.config.cli_download_url.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_target_has_a_tarball_and_a_checksum() {
        let body = response(None);
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(body.cli.len(), 4);
        let mac = body
            .cli
            .iter()
            .find(|download| download.target == "aarch64-apple-darwin")
            .unwrap();
        assert_eq!(
            mac.url,
            format!(
                "https://github.com/colliery-io/kairos/releases/download/v{version}/kairos-{version}-aarch64-apple-darwin.tar.gz"
            )
        );
        assert_eq!(mac.sha256_url, format!("{}.sha256", mac.url));
    }

    #[test]
    fn a_mirror_replaces_the_base() {
        let body = response(Some("https://mirror.example/kairos"));
        assert!(body.cli.iter().all(|download| {
            download
                .url
                .starts_with("https://mirror.example/kairos/download/v")
        }));
    }
}
