//! The tools of the SCIP runners of Go, TypeScript and Python, and the
//! dependency cache of each repository (KAIROS-T-0474, COLLIERY-I-0622).
//!
//! **The tools.** A server build fetches the tools of a language into its
//! tools folder (`KAIROS_CODE_INDEX_DIR/tools/`, as COLLIERY-T-2525 does for
//! rust-analyzer) when a repository first needs that language:
//!
//! | Language   | Tools                                   |
//! |------------|-----------------------------------------|
//! | go         | the Go toolchain, scip-go               |
//! | typescript | Node.js, scip-typescript (npm)          |
//! | python     | Node.js, scip-python (npm), CPython     |
//! | uv         | uv, for a Python project with `uv.lock` |
//!
//! Each archive has a pinned version and sha256 for each platform (amd64 and
//! arm64, Linux and macOS). A file with a wrong sha256 is refused, and the
//! language falls back to the edges by name with a warning. An npm package is
//! installed with `npm ci` from a lock file of this crate (`npm/`), so npm
//! checks the integrity of each package. A platform with no pin, or a fetch
//! that fails, is the same fallback. A tree with no file of a language never
//! fetches its tools.
//!
//! A local `kairos index` run fetches nothing: it uses the tools on `PATH`
//! (`go` and `scip-go`, `node` and `scip-typescript`, `scip-python` and
//! `python3`). A missing tool is the fallback with a warning.
//!
//! **The dependency cache.** The indexers resolve well only with the
//! dependencies of the repository installed. A server build keeps them in
//! `deps/<repository>/<language>/` of the cache root: the Go module cache,
//! the npm cache, a Python virtual environment. The cache has a total size
//! cap: after a run, the caches of other repositories are removed, oldest
//! first, until the total is under the cap. Before an install, when the free
//! space of the disk is under its minimum, the caches are removed first,
//! oldest first; if the space is still low, the language falls back.
//!
//! **Credentials.** A dependency step gets a clean environment: `PATH`, the
//! cache folders, and only the variables of [`PASSED_VARIABLES`] (and the
//! names that the deployment adds). So a private module can use a token that
//! the deployment puts in the environment (`GOPRIVATE` with a `.netrc` or
//! `GIT_CONFIG_*`, `NODE_AUTH_TOKEN`, `PIP_INDEX_URL`), and no other secret
//! of the server reaches a package install.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component as PathPart, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::IndexError;
use crate::tools::{Download, check_file, download, hex, io};

/// The Go release of the Go runner.
pub const GO_VERSION: &str = "1.27.2";
/// The scip-go release (github.com/sourcegraph/scip-go, Apache-2.0).
pub const SCIP_GO_VERSION: &str = "0.2.7";
/// The Node.js release (an LTS) of the TypeScript and Python runners.
pub const NODE_VERSION: &str = "24.22.0";
/// The scip-typescript release (npm `@sourcegraph/scip-typescript`, Apache-2.0).
pub const SCIP_TYPESCRIPT_VERSION: &str = "0.4.0";
/// The scip-python release (npm `@sourcegraph/scip-python`, MIT).
pub const SCIP_PYTHON_VERSION: &str = "0.6.6";
/// The CPython release of python-build-standalone (astral-sh).
pub const PYTHON_VERSION: &str = "3.13.16";
/// The python-build-standalone release tag.
pub const PYTHON_BUILD: &str = "20261013";
/// The uv release (github.com/astral-sh/uv, MIT or Apache-2.0), for a
/// Python project with `uv.lock` (KAIROS-T-0486).
pub const UV_VERSION: &str = "0.13.1";

/// The variables of the environment that a dependency step gets, with
/// `PATH` and the cache folders. Each name that ends with `_` is a prefix.
pub const PASSED_VARIABLES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "LANG",
    "LC_ALL",
    "TZ",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_",
    "GIT_CONFIG_VALUE_",
    "GIT_ASKPASS",
    "GOPRIVATE",
    "GONOPROXY",
    "GONOSUMDB",
    "GONOSUMCHECK",
    "GOPROXY",
    "GOSUMDB",
    "GOINSECURE",
    "GOAUTH",
    "NETRC",
    "NODE_AUTH_TOKEN",
    "NPM_TOKEN",
    "NPM_CONFIG_",
    "npm_config_",
    "PIP_INDEX_URL",
    "PIP_EXTRA_INDEX_URL",
    "PIP_TRUSTED_HOST",
    "PIP_CERT",
    "UV_INDEX_",
    "UV_DEFAULT_INDEX",
    "UV_EXTRA_INDEX_URL",
    "GITHUB_TOKEN",
    "GH_TOKEN",
];

/// How a component is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A `.tar.gz` archive, unpacked into the folder of the component with
    /// the first `strip` parts of each path removed.
    Archive { download: Download, strip: usize },
    /// An npm package, installed with `npm ci` from these `package.json`
    /// and `package-lock.json` texts, with the Node.js of the language.
    Npm {
        package_json: &'static str,
        lock: &'static str,
    },
}

/// One tool of a language, pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// `go`, `scip-go`, `node`, `scip-typescript`, `scip-python`, `python`.
    pub name: &'static str,
    pub version: &'static str,
    /// `None` when the platform has no pin.
    pub source: Option<Source>,
}

impl Component {
    /// The folder of the component in the tools folder.
    pub fn folder(&self) -> String {
        format!("{}-{}", self.name, self.version)
    }

    /// The text of the mark of a complete install: the sha256 of what made
    /// it.
    fn mark(&self) -> Option<String> {
        match self.source.as_ref()? {
            Source::Archive { download, .. } => Some(download.sha256.clone()),
            Source::Npm { lock, .. } => Some(hex(&Sha256::digest(lock.as_bytes()))),
        }
    }
}

/// The platform names of the archives of this computer: (Go and Node.js
/// OS, Go arch, Node.js arch, target triple).
fn platform() -> (&'static str, &'static str, &'static str, String) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let (go_arch, node_arch) = match std::env::consts::ARCH {
        "x86_64" => ("amd64", "x64"),
        "aarch64" => ("arm64", "arm64"),
        other => (other, other),
    };
    (os, go_arch, node_arch, crate::rust_analyzer::this_target())
}

/// The sha256 of the Go archives, by `os-arch`, from go.dev/dl.
const GO_PINS: &[(&str, &str)] = &[
    (
        "darwin-amd64",
        "587b59182488b23aa6e5fc25110405a3e0e5b38ed2f5b2f46ed13c32aee356fe",
    ),
    (
        "darwin-arm64",
        "76812b213b1b2302c978d28fa52fa92d541704b9e7d9d5db8002c50e4018c4c5",
    ),
    (
        "linux-amd64",
        "ecbadb99091a3f46e31f5f934b068b1864eafa7995211b39eaddf76996045fe5",
    ),
    (
        "linux-arm64",
        "94f3e30b8e374bc285e7dadc11e0865726b9bc6e85b841ccceaabc0214c6b7c8",
    ),
];

/// The sha256 of the scip-go archives, by `os-arch`, from the `.sha256`
/// files of the release. The release has no macOS amd64 build.
const SCIP_GO_PINS: &[(&str, &str)] = &[
    (
        "darwin-arm64",
        "f21b505452a8dbb270c18ec66690e583801f9e96cff1d72e84c004b7374ec672",
    ),
    (
        "linux-amd64",
        "5bfe39016ca04f5b3b1cce41d1b63ea120a7d7e93b55407bfb17a6b02d18135a",
    ),
    (
        "linux-arm64",
        "6b93476c7578c5aeb5acacb41f8234c20130168271adea0db8d8ae63d1355acb",
    ),
];

/// The sha256 of the Node.js archives, by `os-arch`, from SHASUMS256.txt.
const NODE_PINS: &[(&str, &str)] = &[
    (
        "darwin-arm64",
        "6a9724d9b50a5f22b4816cded441b5a8d9ddae546191a5e5b7efa5a5e2ee26d5",
    ),
    (
        "darwin-x64",
        "495e3f273c645f3c4677d438304cdfc701729ea5dec5728f5c8e76a2dece4b91",
    ),
    (
        "linux-arm64",
        "b7e7991d5407815adfa74a2a9cd4db683552c8ce32ff4a295c22e972849c3f5d",
    ),
    (
        "linux-x64",
        "28ea0a438de9a13bd7891fd758eee6c7819ee8af5442dfd8d45bd65e56d4228c",
    ),
];

/// The sha256 of the CPython archives, by target triple, from SHA256SUMS.
const PYTHON_PINS: &[(&str, &str)] = &[
    (
        "aarch64-apple-darwin",
        "ca0e6a70fb75a0984751bd75493e3de77680aef7fbd0fea9b903c299537b3423",
    ),
    (
        "aarch64-unknown-linux-gnu",
        "0fce0605da7b6532d5bdaec185336ffaaddd02ec04e27382014952ca4857ed6d",
    ),
    (
        "x86_64-apple-darwin",
        "d1f2e7b5f0cfa25fc856fea819890bbe722f47a692e5ce2552636b5240930b38",
    ),
    (
        "x86_64-unknown-linux-gnu",
        "e588e50db882fa6a89f926d9a9ad64e445183af0eda5bda226a2c151b17db8a6",
    ),
];

/// The sha256 of the uv archives, by target triple, from the `.sha256`
/// files of the release.
const UV_PINS: &[(&str, &str)] = &[
    (
        "aarch64-apple-darwin",
        "cfe454573412ff7380d15fc7dae5997ab53ffce72fce0a78e91ddbc84001acd2",
    ),
    (
        "aarch64-unknown-linux-gnu",
        "9041c73c99fdc5892cf128140beb354eca9231d502068cc36e6cb53a8e59ad14",
    ),
    (
        "x86_64-apple-darwin",
        "bb7b805bdcf3d0bc8c74ceb94569e6b3e8aa35bd492acdbb9d46bf46d9d4ff6b",
    ),
    (
        "x86_64-unknown-linux-gnu",
        "eadd9d27c134ab5ac1ffd4473b97a741407f3ea238288f03121fc4deddf90220",
    ),
];

fn pin(pins: &[(&str, &'static str)], key: &str) -> Option<&'static str> {
    pins.iter().find(|(k, _)| *k == key).map(|(_, sha)| *sha)
}

/// The pinned components of `language` (`go`, `typescript`, `python`, and
/// `uv` for a Python project with `uv.lock`) for this platform. A component with no pin for the platform has no source.
pub fn pinned(language: &str) -> Vec<Component> {
    let (os, go_arch, node_arch, triple) = platform();
    let archive = |url: String, sha: Option<&str>, strip| {
        sha.map(|sha| Source::Archive {
            download: Download::new(url, sha),
            strip,
        })
    };
    let node = Component {
        name: "node",
        version: NODE_VERSION,
        source: archive(
            format!(
                "https://nodejs.org/dist/v{NODE_VERSION}/node-v{NODE_VERSION}-{os}-{node_arch}.tar.gz"
            ),
            pin(NODE_PINS, &format!("{os}-{node_arch}")),
            1,
        ),
    };
    match language {
        "go" => vec![
            Component {
                name: "go",
                version: GO_VERSION,
                source: archive(
                    format!("https://go.dev/dl/go{GO_VERSION}.{os}-{go_arch}.tar.gz"),
                    pin(GO_PINS, &format!("{os}-{go_arch}")),
                    1,
                ),
            },
            Component {
                name: "scip-go",
                version: SCIP_GO_VERSION,
                source: archive(
                    format!(
                        "https://github.com/sourcegraph/scip-go/releases/download/v{SCIP_GO_VERSION}/scip-go-{os}-{go_arch}.tar.gz"
                    ),
                    pin(SCIP_GO_PINS, &format!("{os}-{go_arch}")),
                    0,
                ),
            },
        ],
        "typescript" => vec![
            node,
            Component {
                name: "scip-typescript",
                version: SCIP_TYPESCRIPT_VERSION,
                source: Some(Source::Npm {
                    package_json: include_str!("../npm/scip-typescript/package.json"),
                    lock: include_str!("../npm/scip-typescript/package-lock.json"),
                }),
            },
        ],
        "python" => vec![
            node,
            Component {
                name: "scip-python",
                version: SCIP_PYTHON_VERSION,
                source: Some(Source::Npm {
                    package_json: include_str!("../npm/scip-python/package.json"),
                    lock: include_str!("../npm/scip-python/package-lock.json"),
                }),
            },
            Component {
                name: "python",
                version: PYTHON_VERSION,
                source: archive(
                    format!(
                        "https://github.com/astral-sh/python-build-standalone/releases/download/{PYTHON_BUILD}/cpython-{PYTHON_VERSION}%2B{PYTHON_BUILD}-{triple}-install_only.tar.gz"
                    ),
                    pin(PYTHON_PINS, &triple),
                    1,
                ),
            },
        ],
        "uv" => vec![Component {
            name: "uv",
            version: UV_VERSION,
            source: archive(
                format!(
                    "https://github.com/astral-sh/uv/releases/download/{UV_VERSION}/uv-{triple}.tar.gz"
                ),
                pin(UV_PINS, &triple),
                1,
            ),
        }],
        _ => Vec::new(),
    }
}

/// Where the tools of the languages are, and the dependency cache of the
/// repository of a build.
#[derive(Debug, Clone, Default)]
pub struct LanguageTools {
    /// The tools folder. `None`: the tools on `PATH` (a local run).
    pub dir: Option<PathBuf>,
    /// Fetch a missing tool into `dir` on first need (a server build).
    pub fetch: bool,
    /// Components that replace the pinned component of the same name: for
    /// the tests.
    pub overrides: Vec<Component>,
    /// The dependency cache of the repository of the build. `None`: no
    /// dependency step (a local run uses what the checkout has).
    pub cache: Option<DependencyCache>,
    /// The names of more variables of the environment that a dependency
    /// step gets (`KAIROS_CODE_INDEX_DEPS_ENV`).
    pub pass_variables: Vec<String>,
    /// The tool folders that this process fetched, for the log.
    pub fetched: Arc<Mutex<Vec<String>>>,
}

/// One fetch at a time for each tools folder: 2 builds that need the same
/// language wait for one fetch.
static FETCH: Mutex<()> = Mutex::new(());

impl LanguageTools {
    /// The components of `language`, with the overrides.
    pub fn components(&self, language: &str) -> Vec<Component> {
        pinned(language)
            .into_iter()
            .map(|c| {
                self.overrides
                    .iter()
                    .find(|o| o.name == c.name)
                    .cloned()
                    .unwrap_or(c)
            })
            .collect()
    }

    /// The folders of the tools of `language`, by component name: fetched
    /// into the tools folder if needed. The error is the reason of the
    /// warning of the fallback. With no tools folder, `Ok(None)`: the
    /// runner uses the tools on `PATH`.
    pub fn prepare(
        &self,
        language: &str,
    ) -> Result<Option<BTreeMap<&'static str, PathBuf>>, String> {
        let Some(dir) = &self.dir else {
            return Ok(None);
        };
        let _one = FETCH.lock().unwrap_or_else(|e| e.into_inner());
        fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        let mut out = BTreeMap::new();
        for component in self.components(language) {
            let folder = self.ensure(dir, &component, &out)?;
            out.insert(component.name, folder);
        }
        Ok(Some(out))
    }

    /// The folder of `component`, installed if it is not.
    fn ensure(
        &self,
        dir: &Path,
        component: &Component,
        done: &BTreeMap<&'static str, PathBuf>,
    ) -> Result<PathBuf, String> {
        let dest = dir.join(component.folder());
        let (Some(source), Some(mark)) = (&component.source, component.mark()) else {
            return Err(format!(
                "{} {} has no pinned build for {}",
                component.name,
                component.version,
                crate::rust_analyzer::this_target()
            ));
        };
        if fs::read_to_string(dest.join(MARK)).is_ok_and(|m| m.trim() == mark) {
            return Ok(dest);
        }
        if !self.fetch {
            return Err(format!(
                "{} {} is not in {}",
                component.name,
                component.version,
                dir.display()
            ));
        }
        let partial = tempfile::Builder::new()
            .prefix(".tool-")
            .tempdir_in(dir)
            .map_err(|e| format!("cannot make a folder in {}: {e}", dir.display()))?;
        match source {
            Source::Archive { download: d, strip } => {
                let archive = dir.join(&d.file);
                if archive.exists() {
                    check_file(&archive, &d.sha256).map_err(|e| e.to_string())?;
                } else {
                    download(d, &archive).map_err(|e| e.to_string())?;
                }
                unpack(&archive, partial.path(), *strip).map_err(|e| e.to_string())?;
            }
            Source::Npm { package_json, lock } => {
                let node = done
                    .get("node")
                    .ok_or_else(|| format!("{} needs node", component.name))?;
                fs::write(partial.path().join("package.json"), package_json)
                    .map_err(|e| e.to_string())?;
                fs::write(partial.path().join("package-lock.json"), lock)
                    .map_err(|e| e.to_string())?;
                let mut command = Command::new(node.join("bin/npm"));
                command
                    .args(["ci", "--ignore-scripts", "--no-audit", "--no-fund"])
                    .current_dir(partial.path());
                clean_env(
                    &mut command,
                    &[node.join("bin")],
                    &self.pass_variables,
                    &dir.join(".home"),
                );
                command.env("npm_config_cache", dir.join(".npm-cache"));
                run_step(&mut command, "npm ci")?;
            }
        }
        fs::write(partial.path().join(MARK), &mark).map_err(|e| e.to_string())?;
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .map_err(|e| format!("cannot remove {}: {e}", dest.display()))?;
        }
        let kept = partial.keep();
        fs::rename(&kept, &dest).map_err(|e| format!("cannot move {}: {e}", dest.display()))?;
        if let Ok(mut fetched) = self.fetched.lock() {
            fetched.push(component.folder());
        }
        Ok(dest)
    }

    /// The dependency folder of `language` for the repository of the
    /// build, after the check of the free space. `Ok(None)` with no cache.
    pub fn dependency_dir(&self, language: &str) -> Result<Option<PathBuf>, String> {
        match &self.cache {
            None => Ok(None),
            Some(cache) => cache.prepare(language).map(Some),
        }
    }
}

/// The mark of a complete install, with the sha256 of what made it.
const MARK: &str = ".kairos-index-unpacked";

/// Unpack the `.tar.gz` `archive` into `to`, with the first `strip` parts
/// of each path removed. A path that leaves the folder is refused.
fn unpack(archive: &Path, to: &Path, strip: usize) -> Result<(), IndexError> {
    let bad = io(archive);
    let file = fs::File::open(archive).map_err(&bad)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar.entries().map_err(&bad)? {
        let mut entry = entry.map_err(&bad)?;
        let path = entry.path().map_err(&bad)?.into_owned();
        let parts: Vec<PathPart> = path
            .components()
            .filter(|c| !matches!(c, PathPart::CurDir))
            .collect();
        if parts.len() <= strip || !parts.iter().all(|c| matches!(c, PathPart::Normal(_))) {
            continue;
        }
        let target = to.join(parts[strip..].iter().collect::<PathBuf>());
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target).map_err(&bad)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(&bad)?;
        }
        entry.unpack(&target).map_err(&bad)?;
    }
    Ok(())
}

/// Give `command` a clean environment: `PATH` with `bins` first, `HOME`,
/// and the variables of [`PASSED_VARIABLES`] and `more` that are set.
pub(crate) fn clean_env(command: &mut Command, bins: &[PathBuf], more: &[String], home: &Path) {
    let passed: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| {
            PASSED_VARIABLES.iter().any(|p| {
                if p.ends_with('_') {
                    name.starts_with(p)
                } else {
                    name == p
                }
            }) || more.iter().any(|m| m == name)
        })
        .collect();
    let mut paths: Vec<PathBuf> = bins.to_vec();
    paths.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
    command.env_clear();
    command.envs(passed);
    if let Ok(path) = std::env::join_paths(paths) {
        command.env("PATH", path);
    }
    let _ = fs::create_dir_all(home);
    command.env("HOME", home);
}

/// Run a step; the error is its name and the end of its output.
pub(crate) fn run_step(command: &mut Command, what: &str) -> Result<std::process::Output, String> {
    let output = command
        .output()
        .map_err(|e| format!("{what} did not start: {e}"))?;
    if output.status.success() {
        return Ok(output);
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        text.into_owned()
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" ");
    Err(format!("{what} failed ({}): {tail}", output.status))
}

/// The dependency caches of the repositories, with their cap.
#[derive(Debug, Clone)]
pub struct DependencyCache {
    /// The folder of all the caches: `KAIROS_CODE_INDEX_DIR/deps`.
    pub root: PathBuf,
    /// The folder of the repository of the build, under `root`.
    pub key: String,
    /// The cap of the total size of the caches, in bytes.
    pub cap_bytes: u64,
    /// The free space of the disk under which caches are removed before an
    /// install, in bytes.
    pub min_free_bytes: u64,
}

/// The file whose modified time is the last use of a cache.
const LAST_USED: &str = ".kairos-last-used";

/// One cache of a repository and a language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// `<repository>/<language>`, from the root.
    pub path: String,
    pub bytes: u64,
    pub last_used: Option<SystemTime>,
}

impl DependencyCache {
    /// The cache of the repository of the build for `language`, marked as
    /// used. When the free space is under the minimum, the caches are
    /// removed first, oldest first; if the space is still low, an error.
    pub fn prepare(&self, language: &str) -> Result<PathBuf, String> {
        let rel = format!("{}/{language}", self.key);
        if free_bytes(&self.root).is_some_and(|free| free < self.min_free_bytes) {
            for entry in caches(&self.root) {
                let _ = fs::remove_dir_all(self.root.join(&entry.path));
                if free_bytes(&self.root).is_none_or(|free| free >= self.min_free_bytes) {
                    break;
                }
            }
        }
        if let Some(free) = free_bytes(&self.root).filter(|f| *f < self.min_free_bytes) {
            return Err(format!(
                "the free disk space is low ({} MB, the minimum is {} MB), so the dependencies \
                 are not installed",
                free / MB,
                self.min_free_bytes / MB
            ));
        }
        let dir = self.root.join(&rel);
        fs::create_dir_all(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        fs::write(dir.join(LAST_USED), b"").map_err(|e| e.to_string())?;
        Ok(dir)
    }

    /// Remove the caches of other repositories and languages, oldest first,
    /// until the total is under the cap. The cache of `keep` stays.
    pub fn enforce_cap(&self, keep: &str) -> Vec<String> {
        let keep = format!("{}/{keep}", self.key);
        let entries = caches(&self.root);
        let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
        let mut removed = Vec::new();
        for entry in entries {
            if total <= self.cap_bytes {
                break;
            }
            if entry.path == keep {
                continue;
            }
            if fs::remove_dir_all(self.root.join(&entry.path)).is_ok() {
                total = total.saturating_sub(entry.bytes);
                removed.push(entry.path);
            }
        }
        removed
    }
}

const MB: u64 = 1024 * 1024;

/// Each cache under `root` (`<repository…>/<language>` folders with a mark
/// of use), oldest first.
pub fn caches(root: &Path) -> Vec<CacheEntry> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        if dir != root && dir.join(LAST_USED).is_file() {
            out.push(CacheEntry {
                path: dir
                    .strip_prefix(root)
                    .unwrap_or(&dir)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
                bytes: size_of(&dir),
                last_used: fs::metadata(dir.join(LAST_USED))
                    .and_then(|m| m.modified())
                    .ok(),
            });
            continue;
        }
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    out.sort_by(|a, b| (a.last_used, &a.path).cmp(&(b.last_used, &b.path)));
    out
}

/// The size of the files under `path`, in bytes. Links are not followed.
pub fn size_of(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| size_of(&e.path()))
        .sum()
}

/// The disk use of the tools folder, by component folder (`go-1.27.2`),
/// with the archives and the other files as `other`.
pub fn tool_usage(dir: &Path) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = size_of(&entry.path());
        let key = if entry.path().join(MARK).is_file() {
            name
        } else {
            "other".to_string()
        };
        *out.entry(key).or_default() += bytes;
    }
    out
}

/// The free space of the disk of `path`, in bytes. `None` when it is not
/// known.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut at = path.to_path_buf();
        while !at.exists() {
            at = at.parent()?.to_path_buf();
        }
        let c = std::ffi::CString::new(at.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid C string and `stat` is a valid buffer.
        if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The cache folder name of a repository: its id, with only safe
/// characters.
pub fn cache_key(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Find `program` on `PATH`.
pub fn on_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(program))
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn each_platform_has_the_pins_of_the_runners() {
        for (key, sha) in GO_PINS.iter().chain(NODE_PINS).chain(SCIP_GO_PINS) {
            assert_eq!(sha.len(), 64, "{key}");
        }
        for target in [
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        ] {
            assert!(pin(PYTHON_PINS, target).is_some(), "{target}");
        }
        for (target, sha) in UV_PINS {
            assert_eq!(sha.len(), 64, "{target}");
            assert!(pin(PYTHON_PINS, target).is_some(), "{target}");
        }
        for lang in ["go", "typescript", "python", "uv"] {
            let components = pinned(lang);
            assert!(!components.is_empty());
            for c in components {
                if let Some(Source::Archive { download, .. }) = &c.source {
                    assert!(download.url.starts_with("https://"), "{download:?}");
                    assert!(download.url.contains(c.version), "{download:?}");
                }
            }
        }
        assert!(pinned("terraform").is_empty());
    }

    pub(crate) fn fake_archive(
        dir: &Path,
        name: &str,
        files: &[(&str, &str)],
    ) -> (PathBuf, String) {
        let path = dir.join(name);
        let file = fs::File::create(&path).unwrap();
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::fast(),
        ));
        for (p, text) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, p, text.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        let sha = crate::tools::sha256_of(&path).unwrap();
        (path, sha)
    }

    fn tools_with(dir: &Path, component: Component) -> LanguageTools {
        LanguageTools {
            dir: Some(dir.to_path_buf()),
            fetch: true,
            overrides: vec![component],
            ..Default::default()
        }
    }

    /// KAIROS-T-0474: a tool archive in the folder (as an operator with no
    /// network puts it) is checked and unpacked once.
    #[test]
    fn an_archive_in_the_folder_is_checked_and_unpacked() {
        let dir = tempfile::tempdir().unwrap();
        let (_, sha) = fake_archive(
            dir.path(),
            "scip-go.tar.gz",
            &[("scip-go", "#!/bin/sh\n"), ("LICENSE", "x")],
        );
        let component = Component {
            name: "scip-go",
            version: "9.9.9",
            source: Some(Source::Archive {
                download: Download::new("http://127.0.0.1:1/scip-go.tar.gz", sha),
                strip: 0,
            }),
        };
        let tools = tools_with(dir.path(), component.clone());
        let mut ensured = tools
            .ensure(dir.path(), &component, &BTreeMap::new())
            .unwrap();
        assert!(ensured.join("scip-go").is_file());
        // A second call finds the mark.
        fs::remove_file(dir.path().join("scip-go.tar.gz")).unwrap();
        ensured = tools
            .ensure(dir.path(), &component, &BTreeMap::new())
            .unwrap();
        assert_eq!(ensured, dir.path().join("scip-go-9.9.9"));
        assert_eq!(*tools.fetched.lock().unwrap(), ["scip-go-9.9.9"]);
    }

    /// KAIROS-T-0474: a file with a wrong sha256 is refused; the reason
    /// names the file and the pinned value.
    #[test]
    fn a_bad_checksum_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fake_archive(dir.path(), "go.tar.gz", &[("go/bin/go", "x")]);
        let component = Component {
            name: "go",
            version: "0.0.1",
            source: Some(Source::Archive {
                download: Download::new("http://127.0.0.1:1/go.tar.gz", "0".repeat(64)),
                strip: 1,
            }),
        };
        let tools = tools_with(dir.path(), component.clone());
        let reason = tools
            .ensure(dir.path(), &component, &BTreeMap::new())
            .unwrap_err();
        assert!(reason.contains("go.tar.gz"), "{reason}");
        assert!(reason.contains(&"0".repeat(64)), "{reason}");
        assert!(!dir.path().join("go-0.0.1").exists());
    }

    /// With no fetch, a missing tool is a reason, and nothing is downloaded.
    #[test]
    fn no_fetch_means_no_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = LanguageTools {
            dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        tools.fetch = false;
        let reason = tools.prepare("go").unwrap_err();
        assert!(reason.contains("is not in"), "{reason}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_platform_with_no_pin_is_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let component = Component {
            name: "scip-go",
            version: SCIP_GO_VERSION,
            source: None,
        };
        let tools = tools_with(dir.path(), component.clone());
        let reason = tools
            .ensure(dir.path(), &component, &BTreeMap::new())
            .unwrap_err();
        assert!(reason.contains("no pinned build"), "{reason}");
    }

    fn fill(dir: &Path, bytes: usize) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("blob"), vec![0u8; bytes]).unwrap();
    }

    /// KAIROS-T-0474: the caches stay under their cap, oldest first, and the
    /// cache of the build stays.
    #[test]
    fn the_cache_stays_under_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cache = |key: &str| DependencyCache {
            root: dir.path().to_path_buf(),
            key: key.to_string(),
            cap_bytes: 2500,
            min_free_bytes: 0,
        };
        for key in ["t/a", "t/b", "t/c"] {
            let d = cache(key).prepare("go").unwrap();
            fill(&d, 1000);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // a is the oldest; c is the cache of this build.
        let removed = cache("t/c").enforce_cap("go");
        assert_eq!(removed, ["t/a/go"]);
        let left: Vec<String> = caches(dir.path()).into_iter().map(|e| e.path).collect();
        assert_eq!(left, ["t/b/go", "t/c/go"]);
        // A cache used again is the newest.
        cache("t/b").prepare("go").unwrap();
        fill(&dir.path().join("t/c/go"), 2000);
        assert_eq!(cache("t/b").enforce_cap("go"), ["t/c/go"]);
    }

    /// KAIROS-T-0474: when the free space is under the minimum, each cache
    /// is removed, and the install is refused if the space is still low.
    #[test]
    fn low_space_clears_the_caches() {
        let dir = tempfile::tempdir().unwrap();
        let old = DependencyCache {
            root: dir.path().to_path_buf(),
            key: "t/old".into(),
            cap_bytes: u64::MAX,
            min_free_bytes: 0,
        };
        fill(&old.prepare("python").unwrap(), 10);
        let low = DependencyCache {
            key: "t/new".into(),
            min_free_bytes: u64::MAX,
            ..old.clone()
        };
        let reason = low.prepare("python").unwrap_err();
        assert!(reason.contains("free disk space is low"), "{reason}");
        assert!(caches(dir.path()).is_empty());
    }

    #[test]
    fn a_dependency_step_gets_a_clean_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("/usr/bin/env");
        // SAFETY: the test sets variables that no other test reads.
        unsafe {
            std::env::set_var("KAIROS_TEST_SECRET_0474", "s");
            std::env::set_var("GOPRIVATE", "example.com/private");
            std::env::set_var("NPM_CONFIG_REGISTRY_0474", "r");
        }
        clean_env(&mut command, &[dir.path().join("bin")], &[], dir.path());
        let out = String::from_utf8(command.output().unwrap().stdout).unwrap();
        assert!(!out.contains("KAIROS_TEST_SECRET_0474"), "{out}");
        assert!(out.contains("GOPRIVATE=example.com/private"), "{out}");
        assert!(out.contains("NPM_CONFIG_REGISTRY_0474=r"), "{out}");
        assert!(
            out.contains(&format!("PATH={}", dir.path().join("bin").display())),
            "{out}"
        );
    }

    #[test]
    fn a_cache_key_has_safe_characters() {
        assert_eq!(cache_key(&["acme", "a/b c"]), "acme/a_b_c");
    }
}
