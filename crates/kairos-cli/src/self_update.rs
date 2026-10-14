//! `kairos self-update` and the daily version check (KAIROS-T-0432).
//!
//! The server tells its version at `GET /version` (open). The CLI compares
//! it with its own. `self-update` downloads the release tarball of this
//! platform from GitHub, checks its `.sha256`, and swaps the binary.
//!
//! The swap writes the new binary to a new file next to the old one and
//! renames it over the old path. Writing over the old file in place gets
//! the binary killed at launch on macOS (exit 137): the kernel keeps the
//! code signature of that file. The old binary stays as `<name>.bak`.
//! A failed download or a bad checksum leaves the old binary in place.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::{self, Source};
use crate::credentials;
use crate::error::CliError;

/// The version of this CLI.
pub const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the release assets are. `KAIROS_RELEASES_URL` overrides it (tests,
/// a mirror).
const RELEASES_URL: &str = "https://github.com/colliery-io/kairos/releases";

/// The latest release, for `--latest`.
const LATEST_API: &str = "https://api.github.com/repos/colliery-io/kairos/releases/latest";

/// The variable that turns the daily check into an update without asking.
const AUTO_UPDATE_VAR: &str = "KAIROS_AUTO_UPDATE";

/// One check a day at most.
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// `kairos self-update`.
#[derive(clap::Args, Debug)]
pub struct SelfUpdateArgs {
    /// Deployment base URL whose version to install (defaults to KAIROS_URL,
    /// then the only cached deployment)
    #[arg(long)]
    pub url: Option<String>,
    /// Install this version instead of the version of the server (e.g. 0.12.0)
    #[arg(long, conflicts_with = "latest")]
    pub version: Option<String>,
    /// Install the latest release instead of the version of the server
    #[arg(long)]
    pub latest: bool,
    /// Only say which version the server has and which this CLI has
    #[arg(long, conflicts_with_all = ["version", "latest"])]
    pub check: bool,
}

impl SelfUpdateArgs {
    pub async fn run(self) -> Result<(), CliError> {
        let http = http()?;
        if self.check {
            let deployment = deployment(self.url.as_deref())?;
            let server = server_version(&http, &deployment).await?.version;
            match compare(CLI_VERSION, &server) {
                std::cmp::Ordering::Less => println!(
                    "The CLI is {CLI_VERSION}; the server {deployment} is {server}. \
                     Run `kairos self-update`."
                ),
                _ => println!("The CLI is {CLI_VERSION}; the server {deployment} is {server}."),
            }
            return Ok(());
        }
        let release = match (self.version, self.latest) {
            (Some(version), _) => Release::published(version.trim_start_matches('v'))?,
            (None, true) => Release::published(&latest_version(&http).await?)?,
            (None, false) => {
                let deployment = deployment(self.url.as_deref())?;
                server_version(&http, &deployment).await?.release()?
            }
        };
        update_to(&http, &release).await
    }
}

/// Download, check and install a release. The same version is no change.
async fn update_to(http: &reqwest::Client, release: &Release) -> Result<(), CliError> {
    let version = &release.version;
    if version == CLI_VERSION {
        println!("The CLI is {CLI_VERSION} already: nothing changed.");
        return Ok(());
    }
    let asset = release.url.rsplit('/').next().unwrap_or("the tarball");
    let archive = download(http, &release.url).await?;
    let checksum = download(http, &release.sha256_url).await?;
    let binary = verified_binary(&archive, &String::from_utf8_lossy(&checksum), asset)?;
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|err| CliError::Failure(format!("The CLI cannot find its own path: {err}.")))?;
    let backup = install(&exe, &binary)?;
    println!(
        "Updated the CLI from {CLI_VERSION} to {version}: {}. The old binary is {}.",
        exe.display(),
        backup.display()
    );
    Ok(())
}

/// One tarball of the CLI to install, and its checksum.
#[derive(Debug, Clone, PartialEq)]
struct Release {
    version: String,
    url: String,
    sha256_url: String,
}

impl Release {
    /// The tarball of `version` for this platform on the published releases
    /// (GitHub, or `KAIROS_RELEASES_URL`).
    fn published(version: &str) -> Result<Self, CliError> {
        let base = std::env::var("KAIROS_RELEASES_URL")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| RELEASES_URL.to_string());
        let target = target_triple()?;
        let url = format!(
            "{}/download/v{version}/kairos-{version}-{target}.tar.gz",
            base.trim_end_matches('/')
        );
        Ok(Self {
            version: version.to_string(),
            sha256_url: format!("{url}.sha256"),
            url,
        })
    }
}

/// The body of `GET /version`: the version of the server and the tarballs
/// of the CLI of that version.
#[derive(Debug, Deserialize)]
struct ServerVersion {
    version: String,
    #[serde(default)]
    cli: Vec<CliDownload>,
}

#[derive(Debug, Deserialize)]
struct CliDownload {
    target: String,
    url: String,
    sha256_url: String,
}

impl ServerVersion {
    /// The tarball for this platform that the server names; else the
    /// published release of its version.
    fn release(self) -> Result<Release, CliError> {
        let target = target_triple()?;
        match self
            .cli
            .into_iter()
            .find(|download| download.target == target)
        {
            Some(download) => Ok(Release {
                version: self.version,
                url: download.url,
                sha256_url: download.sha256_url,
            }),
            None => Release::published(&self.version),
        }
    }
}

/// The daily check, run after a command that succeeded. It prints one line on stderr when
/// the server is newer, at most once a day for a deployment. With
/// `KAIROS_AUTO_UPDATE=1` it updates instead. It never fails the command:
/// a check that cannot run says nothing.
pub async fn daily_check() {
    let Ok(dir) = credentials::config_dir() else {
        return;
    };
    let Ok(deployment) = deployment(None) else {
        return;
    };
    let path = dir.join("version-check.json");
    let now = credentials::unix_now();
    let previous: Option<CheckState> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    if let Some(state) = &previous
        && state.deployment == deployment
        && now.saturating_sub(state.checked_at) < CHECK_INTERVAL_SECS
    {
        return;
    }
    // A server that does not answer is a check too: an empty version, so a
    // dead deployment does not slow each command down.
    let server = match http_with_timeout(Duration::from_secs(2)) {
        Ok(probe) => server_version(&probe, &deployment).await.ok(),
        Err(_) => None,
    };
    let version = server
        .as_ref()
        .map(|server| server.version.clone())
        .unwrap_or_default();
    let state = CheckState {
        deployment,
        checked_at: now,
        server_version: version.clone(),
    };
    if let Ok(text) = serde_json::to_string(&state) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(&path, text);
    }
    let Some(server) = server else {
        return;
    };
    if compare(CLI_VERSION, &version) != std::cmp::Ordering::Less {
        return;
    }
    if std::env::var(AUTO_UPDATE_VAR).is_ok_and(|value| value == "1") {
        let result = match (http(), server.release()) {
            (Ok(http), Ok(release)) => update_to(&http, &release).await,
            (Err(err), _) | (_, Err(err)) => Err(err),
        };
        if let Err(err) = result {
            eprintln!("warning: the CLI could not update itself to {version}: {err}");
        }
        return;
    }
    eprintln!(
        "warning: the CLI is {CLI_VERSION} and the server is {version}. \
         Run `kairos self-update`."
    );
}

/// What the daily check keeps between runs.
#[derive(Debug, Serialize, Deserialize)]
struct CheckState {
    deployment: String,
    checked_at: u64,
    server_version: String,
}

fn deployment(url: Option<&str>) -> Result<String, CliError> {
    let path = credentials::credentials_path()?;
    let source = auth::resolve(url, &auth::Env::from_process(), || credentials::load(&path))?;
    Ok(match source {
        Source::Key { deployment, .. } | Source::Cache { deployment, .. } => deployment,
    })
}

fn http() -> Result<reqwest::Client, CliError> {
    http_with_timeout(Duration::from_secs(300))
}

fn http_with_timeout(timeout: Duration) -> Result<reqwest::Client, CliError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("kairos-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| CliError::Failure(format!("The CLI cannot make an HTTP client: {err}.")))
}

async fn server_version(
    http: &reqwest::Client,
    deployment: &str,
) -> Result<ServerVersion, CliError> {
    let url = format!("{}/version", deployment.trim_end_matches('/'));
    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|err| CliError::Failure(format!("The CLI cannot reach {url}: {err}.")))?;
    if !response.status().is_success() {
        return Err(CliError::Failure(format!(
            "{url} answered {}. An older server does not tell its version: \
             name one with --version or use --latest.",
            response.status()
        )));
    }
    let body: ServerVersion = response.json().await.map_err(|_| {
        CliError::Failure(format!(
            "{url} sent no version. An older server does not tell its version: \
                 name one with --version or use --latest."
        ))
    })?;
    Ok(body)
}

async fn latest_version(http: &reqwest::Client) -> Result<String, CliError> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let response = http
        .get(LATEST_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|err| CliError::Failure(format!("The CLI cannot reach GitHub: {err}.")))?;
    if !response.status().is_success() {
        return Err(CliError::Failure(format!(
            "GitHub answered {} for the latest release.",
            response.status()
        )));
    }
    let release: Release = response
        .json()
        .await
        .map_err(|err| CliError::Failure(format!("GitHub sent no release: {err}.")))?;
    Ok(release.tag_name.trim_start_matches('v').to_string())
}

async fn download(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, CliError> {
    let response = http
        .get(url)
        .send()
        .await
        .map_err(|err| CliError::Failure(format!("The CLI cannot download {url}: {err}.")))?;
    if !response.status().is_success() {
        return Err(CliError::Failure(format!(
            "The download of {url} answered {}. Nothing changed.",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| CliError::Failure(format!("The download of {url} failed: {err}.")))?;
    Ok(bytes.to_vec())
}

/// The release target of this build, as the release assets name it.
fn target_triple() -> Result<&'static str, CliError> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        (arch, os) => Err(CliError::Failure(format!(
            "No release of the CLI is built for {arch} {os}."
        ))),
    }
}

/// Compare two versions `a.b.c` by number. A part that is not a number
/// counts as 0.
fn compare(left: &str, right: &str) -> std::cmp::Ordering {
    let parts = |version: &str| -> Vec<u64> {
        version
            .trim_start_matches('v')
            .split(['.', '-'])
            .take(3)
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    parts(left).cmp(&parts(right))
}

/// Check the archive against the `.sha256` text (`<hex>  <name>`, as
/// `shasum -a 256` writes it) and take the `kairos` binary out of it.
fn verified_binary(archive: &[u8], checksum: &str, asset: &str) -> Result<Vec<u8>, CliError> {
    let expected = checksum
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let actual: String = Sha256::digest(archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if expected != actual {
        return Err(CliError::Failure(format!(
            "The checksum of {asset} does not match: expected {expected}, got {actual}. \
             Nothing changed."
        )));
    }
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let entries = tar
        .entries()
        .map_err(|err| CliError::Failure(format!("{asset} is not a tarball: {err}.")))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|err| CliError::Failure(format!("{asset} is damaged: {err}.")))?;
        let is_binary = entry
            .path()
            .ok()
            .and_then(|path| path.file_name().map(|name| name == "kairos"))
            .unwrap_or(false);
        if is_binary {
            let mut binary = Vec::new();
            entry
                .read_to_end(&mut binary)
                .map_err(|err| CliError::Failure(format!("{asset} is damaged: {err}.")))?;
            return Ok(binary);
        }
    }
    Err(CliError::Failure(format!(
        "{asset} holds no `kairos` binary. Nothing changed."
    )))
}

/// Put `binary` at `exe`: write a new file next to it, keep the old one as
/// `<exe>.bak`, and rename the new file over the path. Returns the backup.
fn install(exe: &Path, binary: &[u8]) -> Result<PathBuf, CliError> {
    let failure = |what: &str, err: std::io::Error| {
        CliError::Failure(format!("The CLI cannot {what}: {err}. Nothing changed."))
    };
    let name = exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "kairos".to_string());
    let staged = exe.with_file_name(format!(".{name}.new-{}", std::process::id()));
    let backup = exe.with_file_name(format!("{name}.bak"));
    std::fs::write(&staged, binary).map_err(|err| failure("write the new binary", err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        {
            let _ = std::fs::remove_file(&staged);
            return Err(failure("make the new binary executable", err));
        }
    }
    if let Err(err) = std::fs::copy(exe, &backup) {
        let _ = std::fs::remove_file(&staged);
        return Err(failure("keep a backup of the old binary", err));
    }
    if let Err(err) = std::fs::rename(&staged, exe) {
        let _ = std::fs::remove_file(&staged);
        return Err(failure("put the new binary in place", err));
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn sha(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn versions_compare_by_number() {
        use std::cmp::Ordering::*;
        assert_eq!(compare("0.9.0", "0.12.0"), Less);
        assert_eq!(compare("0.12.0", "0.12.0"), Equal);
        assert_eq!(compare("v0.12.1", "0.12.0"), Greater);
        assert_eq!(compare("1.0.0", "0.99.99"), Greater);
    }

    #[test]
    fn a_good_archive_gives_the_binary() {
        let archive = tarball(&[
            ("./LICENSE", b"license"),
            ("./kairos", b"new binary"),
            ("./NOTICE", b"notice"),
        ]);
        let checksum = format!("{}  kairos-0.13.0-x.tar.gz\n", sha(&archive));
        let binary = verified_binary(&archive, &checksum, "kairos-0.13.0-x.tar.gz").unwrap();
        assert_eq!(binary, b"new binary");
    }

    #[test]
    fn a_bad_checksum_is_refused() {
        let archive = tarball(&[("./kairos", b"new binary")]);
        let err = verified_binary(&archive, &"0".repeat(64), "a.tar.gz").unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn an_archive_with_no_binary_is_refused() {
        let archive = tarball(&[("./LICENSE", b"license")]);
        let err = verified_binary(&archive, &sha(&archive), "a.tar.gz").unwrap_err();
        assert!(err.to_string().contains("no `kairos` binary"), "{err}");
    }

    #[test]
    fn install_renames_the_new_binary_over_the_old_and_keeps_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("kairos");
        std::fs::write(&exe, b"old").unwrap();
        let backup = install(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old");
        assert_eq!(backup, dir.path().join("kairos.bak"));
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 2, "no staged file is left: {left:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn the_server_names_the_tarball_of_this_platform() {
        let target = target_triple().unwrap();
        let server: ServerVersion = serde_json::from_value(serde_json::json!({
            "version": "9.9.9",
            "cli": [
                {"target": "other", "url": "https://x/other", "sha256_url": "https://x/other.sha256"},
                {"target": target, "url": "https://mirror/k.tar.gz", "sha256_url": "https://mirror/k.tar.gz.sha256"},
            ],
        }))
        .unwrap();
        assert_eq!(
            server.release().unwrap(),
            Release {
                version: "9.9.9".into(),
                url: "https://mirror/k.tar.gz".into(),
                sha256_url: "https://mirror/k.tar.gz.sha256".into(),
            }
        );
    }

    #[test]
    fn a_server_with_no_links_gives_the_published_release() {
        let server: ServerVersion =
            serde_json::from_value(serde_json::json!({"version": "9.9.9"})).unwrap();
        let release = server.release().unwrap();
        assert!(
            release.url.contains("/download/v9.9.9/kairos-9.9.9-"),
            "{release:?}"
        );
        assert_eq!(release.sha256_url, format!("{}.sha256", release.url));
    }

    #[test]
    fn this_platform_has_a_release_target() {
        assert!(target_triple().is_ok());
    }
}
