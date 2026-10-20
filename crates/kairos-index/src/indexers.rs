//! The SCIP runners of Go, TypeScript and Python (KAIROS-T-0475,
//! KAIROS-T-0476, KAIROS-T-0477): scip-go, scip-typescript and scip-python.
//!
//! Each runner finds its tools ([`LanguageTools::prepare`], or `PATH` on a
//! local run), installs the dependencies of the repository into its cache on
//! a server build ([`LanguageTools::dependency_dir`]), runs the indexer in
//! each project of the tree, and gives the documents with paths from the
//! root. A missing tool, a failed install or a failed indexer is a
//! fallback with a warning ([`RunnerFailure::Fallback`]). A project that
//! fails while another project of the language succeeds is a warning: the
//! files of that project get the edges by name.
//!
//! A local run installs nothing: it uses the dependencies of the checkout
//! (`node_modules`, the active Python environment, the Go module cache of
//! the developer) and its own environment.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::languages::{LanguageTools, clean_env, on_path, run_step};
use crate::runners::{RunnerFailure, RunnerInput, ScipOutput, ScipRunner, decode};
use crate::scip::proto;

/// The folder of a path from the root: `""` at the root.
fn folder(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(f, _)| f)
}

/// The folders of `files` and each of their parents up to the root, nearest
/// first: the places to look for a project file.
fn ancestors(file: &str) -> impl Iterator<Item = &str> {
    let mut at = Some(folder(file));
    std::iter::from_fn(move || {
        let here = at?;
        at = if here.is_empty() {
            None
        } else {
            Some(folder(here))
        };
        Some(here)
    })
}

/// The project of each file: the nearest folder with one of `markers`.
/// Only the outer projects are kept: a project inside another is part of
/// it. Files with no project go to `default`, if given.
fn projects(
    root: &Path,
    files: &[&str],
    markers: &[&str],
    default: Option<&str>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for file in files {
        let project =
            ancestors(file).find(|dir| markers.iter().any(|m| root.join(dir).join(m).is_file()));
        match (project, default) {
            (Some(p), _) => {
                found.insert(p.to_string());
            }
            (None, Some(d)) => {
                found.insert(d.to_string());
            }
            (None, None) => {}
        }
    }
    let all: Vec<String> = found.iter().cloned().collect();
    found
        .into_iter()
        .filter(|p| {
            !all.iter()
                .any(|q| q != p && (q.is_empty() || p.starts_with(&format!("{q}/"))))
        })
        .collect()
}

/// The paths of the documents of `index`, from the root: `prefix` (the
/// folder of the project) before each. An unknown position encoding becomes
/// `encoding`, the one of the indexer.
fn rebase(mut index: proto::Index, prefix: &str, encoding: i32) -> proto::Index {
    for doc in &mut index.documents {
        let rel = doc.relative_path.trim_start_matches("./");
        doc.relative_path = if prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{prefix}/{rel}")
        };
        if doc.position_encoding == 0 {
            doc.position_encoding = encoding;
        }
    }
    index
}

fn read_index(path: &Path) -> Result<proto::Index, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("the indexer wrote no index at {}: {e}", path.display()))?;
    decode(&bytes)
}

fn fallback(reason: impl Into<String>) -> RunnerFailure {
    RunnerFailure::Fallback(reason.into())
}

/// A tool: from the tools folder, else from `PATH`.
fn tool(
    prepared: &Option<BTreeMap<&'static str, PathBuf>>,
    component: &str,
    relative: &str,
    program: &str,
) -> Result<PathBuf, RunnerFailure> {
    match prepared {
        Some(map) => map
            .get(component)
            .map(|dir| dir.join(relative))
            .filter(|p| p.exists())
            .ok_or_else(|| fallback(format!("{component} has no {relative}"))),
        None => on_path(program).ok_or_else(|| fallback(format!("{program} is not on PATH"))),
    }
}

/// The folder of `program`, for `PATH`.
fn bin_of(program: &Path) -> PathBuf {
    program.parent().unwrap_or(Path::new("/")).to_path_buf()
}

/// Set the environment of a command: on a server build (`deps` is set), a
/// clean one with `bins` first on `PATH`; on a local run, the environment
/// of the developer with `bins` first on `PATH`.
fn environment(
    command: &mut Command,
    tools: &LanguageTools,
    deps: Option<&Path>,
    bins: &[PathBuf],
) {
    match deps {
        Some(deps) => clean_env(command, bins, &tools.pass_variables, &deps.join("home")),
        None => {
            let mut paths = bins.to_vec();
            if let Some(path) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&path));
            }
            if let Ok(path) = std::env::join_paths(paths) {
                command.env("PATH", path);
            }
        }
    }
}

/// Run the indexer of each project, and merge the documents. The projects
/// that fail are warnings; if each fails, the runner falls back.
fn each_project(
    projects: &BTreeSet<String>,
    mut run: impl FnMut(&str) -> Result<proto::Index, String>,
) -> Result<ScipOutput, RunnerFailure> {
    let mut output = ScipOutput::default();
    let mut failures = Vec::new();
    for project in projects {
        match run(project) {
            Ok(index) => output.index.documents.extend(index.documents),
            Err(reason) => {
                let name = if project.is_empty() { "." } else { project };
                failures.push(format!("{name}: {reason}"));
            }
        }
    }
    if output.index.documents.is_empty() && !failures.is_empty() {
        return Err(fallback(failures.join("; ")));
    }
    output.warnings = failures;
    Ok(output)
}

/// The Go files that the indexer hashes and that a Go command can rewrite
/// (`GOFLAGS=-mod=mod` adds requirements to `go.mod` and `go.sum`).
const GO_FILES: [&str; 4] = ["go.mod", "go.sum", "go.work", "go.work.sum"];

/// The Go files of a folder as they were before a Go command ran. The
/// structure build hashed them, and the summary run fails when they change
/// after it (KAIROS-T-0667), so a step that rewrote one is undone.
struct GoFilesGuard {
    saved: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl GoFilesGuard {
    fn new(dir: &Path) -> Self {
        let saved = GO_FILES
            .iter()
            .map(|name| dir.join(name))
            .map(|path| {
                let bytes = std::fs::read(&path).ok();
                (path, bytes)
            })
            .collect();
        GoFilesGuard { saved }
    }

    /// Write back each file that `step` changed, created or removed. The
    /// result names the files, for a warning.
    fn restore(&self, step: &str) -> Option<String> {
        let mut changed = Vec::new();
        for (path, before) in &self.saved {
            let now = std::fs::read(path).ok();
            if now == *before {
                continue;
            }
            let done = match before {
                Some(bytes) => std::fs::write(path, bytes),
                None => std::fs::remove_file(path),
            };
            let name = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            changed.push(if done.is_ok() {
                name
            } else {
                format!("{name} (not restored)")
            });
        }
        (!changed.is_empty()).then(|| {
            format!(
                "{step} changed {}; the files are as before",
                changed.join(", ")
            )
        })
    }
}

/// scip-go on each Go module of the tree (KAIROS-T-0475).
pub struct GoRunner {
    tools: LanguageTools,
}

impl GoRunner {
    pub fn new(options: &crate::runners::ScipOptions) -> Self {
        GoRunner {
            tools: options.tools.clone(),
        }
    }
}

impl ScipRunner for GoRunner {
    fn name(&self) -> &'static str {
        "go"
    }
    fn languages(&self) -> &'static [&'static str] {
        &["go"]
    }
    fn run(&self, input: &RunnerInput<'_>) -> Result<ScipOutput, RunnerFailure> {
        let modules = projects(input.root, input.files, &["go.mod"], None);
        if modules.is_empty() {
            return Err(fallback("the Go files have no go.mod"));
        }
        let prepared = self.tools.prepare("go").map_err(fallback)?;
        let go = tool(&prepared, "go", "bin/go", "go")?;
        let scip_go = tool(&prepared, "scip-go", "scip-go", "scip-go")?;
        let deps = self.tools.dependency_dir("go").map_err(fallback)?;
        let work = tempfile::tempdir().map_err(|e| fallback(e.to_string()))?;
        let bins = [bin_of(&go)];
        let go_env = |command: &mut Command| {
            environment(command, &self.tools, deps.as_deref(), &bins);
            if let Some(deps) = &deps {
                command
                    .env("GOMODCACHE", deps.join("mod"))
                    .env("GOCACHE", deps.join("build"))
                    .env("GOPATH", deps.join("path"))
                    .env("GOTOOLCHAIN", "local")
                    .env("GOFLAGS", "-mod=mod")
                    .env("CGO_ENABLED", "0");
            }
        };
        let notes = std::cell::RefCell::new(Vec::new());
        let output = each_project(&modules, |module| {
            let dir = input.root.join(module);
            let files = GoFilesGuard::new(&dir);
            let step = |command: &mut Command, name: &str| {
                let result = run_step(command, name);
                if let Some(note) = files.restore(name) {
                    let at = if module.is_empty() { "." } else { module };
                    notes.borrow_mut().push(format!("{at}: {note}"));
                }
                result
            };
            if deps.is_some() {
                let mut download = Command::new(&go);
                download.args(["mod", "download"]).current_dir(&dir);
                go_env(&mut download);
                step(&mut download, "go mod download")?;
            }
            let out = work
                .path()
                .join(format!("{}.scip", module.replace('/', "_")));
            let mut index = Command::new(&scip_go);
            index
                .args(["index", "--quiet", "--module-root", "."])
                .arg("--output")
                .arg(&out)
                .current_dir(&dir);
            go_env(&mut index);
            step(&mut index, "scip-go")?;
            Ok(rebase(read_index(&out)?, module, proto::ENCODING_UTF8))
        });
        let output = output.map(|mut output| {
            output.warnings.extend(notes.take());
            output
        });
        if let Some(cache) = &self.tools.cache {
            cache.enforce_cap("go");
        }
        output
    }
}

/// The lock files that choose how the dependencies of a JavaScript project
/// are installed.
fn npm_install_args(dir: &Path) -> Option<&'static [&'static str]> {
    if !dir.join("package.json").is_file() {
        return None;
    }
    Some(
        if dir.join("package-lock.json").is_file() || dir.join("npm-shrinkwrap.json").is_file() {
            &["ci", "--ignore-scripts", "--no-audit", "--no-fund"]
        } else {
            &["install", "--ignore-scripts", "--no-audit", "--no-fund"]
        },
    )
}

/// scip-typescript on each TypeScript or JavaScript project of the tree
/// (KAIROS-T-0476). A project with no `tsconfig.json` (a JavaScript
/// project) is indexed with `--infer-tsconfig`.
pub struct TypeScriptRunner {
    tools: LanguageTools,
}

impl TypeScriptRunner {
    pub fn new(options: &crate::runners::ScipOptions) -> Self {
        TypeScriptRunner {
            tools: options.tools.clone(),
        }
    }
}

impl ScipRunner for TypeScriptRunner {
    fn name(&self) -> &'static str {
        "typescript"
    }
    fn languages(&self) -> &'static [&'static str] {
        &["typescript", "tsx", "javascript"]
    }
    fn run(&self, input: &RunnerInput<'_>) -> Result<ScipOutput, RunnerFailure> {
        let projects = projects(
            input.root,
            input.files,
            &["tsconfig.json", "jsconfig.json", "package.json"],
            Some(""),
        );
        let prepared = self.tools.prepare("typescript").map_err(fallback)?;
        let node = tool(&prepared, "node", "bin/node", "node")?;
        let scip = tool(
            &prepared,
            "scip-typescript",
            "node_modules/@sourcegraph/scip-typescript/dist/src/main.js",
            "scip-typescript",
        )?;
        let deps = self.tools.dependency_dir("typescript").map_err(fallback)?;
        let work = tempfile::tempdir().map_err(|e| fallback(e.to_string()))?;
        let bins = [bin_of(&node)];
        let output = each_project(&projects, |project| {
            let dir = input.root.join(project);
            if let (Some(deps), Some(args)) = (&deps, npm_install_args(&dir)) {
                let mut install = Command::new(bin_of(&node).join("npm"));
                install.args(args).current_dir(&dir);
                environment(&mut install, &self.tools, Some(deps), &bins);
                install.env("npm_config_cache", deps.join("npm"));
                run_step(&mut install, &format!("npm {}", args[0]))?;
            }
            let out = work
                .path()
                .join(format!("{}.scip", project.replace('/', "_")));
            // From the tools folder, the script runs with that node; from
            // PATH, `scip-typescript` is the command.
            let mut index = if prepared.is_some() {
                let mut c = Command::new(&node);
                c.arg(&scip);
                c
            } else {
                Command::new(&scip)
            };
            index
                .arg("index")
                .arg("--output")
                .arg(&out)
                .current_dir(&dir);
            // `--infer-tsconfig` writes a tsconfig.json into the project:
            // it is removed after the run, so a local run changes no file
            // of the checkout.
            let tsconfig = dir.join("tsconfig.json");
            let inferred = !tsconfig.exists();
            if inferred {
                index.arg("--infer-tsconfig");
            }
            environment(&mut index, &self.tools, deps.as_deref(), &bins);
            let ran = run_step(&mut index, "scip-typescript");
            if inferred {
                let _ = std::fs::remove_file(&tsconfig);
            }
            ran?;
            Ok(rebase(read_index(&out)?, project, proto::ENCODING_UTF16))
        });
        if let Some(cache) = &self.tools.cache {
            cache.enforce_cap("typescript");
        }
        output
    }
}

/// The dependency files of a Python project at the root, with their texts:
/// `requirements*.txt`, `pyproject.toml`, `uv.lock`, `setup.py`, `setup.cfg`.
fn python_dependency_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let wanted = (name.starts_with("requirements") && name.ends_with(".txt"))
                || matches!(
                    name.as_str(),
                    "pyproject.toml" | "uv.lock" | "setup.py" | "setup.cfg"
                );
            (wanted && e.path().is_file())
                .then(|| Some((name, std::fs::read(e.path()).ok()?)))
                .flatten()
        })
        .collect();
    out.sort();
    out
}

/// How the dependencies of a Python project at the root are installed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PythonInstall {
    /// `uv sync --frozen`: the project has `uv.lock` and `pyproject.toml`
    /// (KAIROS-T-0486).
    Uv,
    /// `pip install -r` each of these `requirements*.txt` files.
    Requirements(Vec<String>),
    /// `pip install -e .`: the project has `pyproject.toml` or `setup.py`.
    Editable,
    /// No dependency file: an empty environment.
    Nothing,
}

/// The install of the project of `files` ([`python_dependency_files`]).
fn python_install(files: &[(String, Vec<u8>)]) -> PythonInstall {
    let has = |name: &str| files.iter().any(|(n, _)| n == name);
    if has("uv.lock") && has("pyproject.toml") {
        return PythonInstall::Uv;
    }
    let requirements: Vec<String> = files
        .iter()
        .map(|(n, _)| n)
        .filter(|n| n.starts_with("requirements"))
        .cloned()
        .collect();
    if !requirements.is_empty() {
        PythonInstall::Requirements(requirements)
    } else if has("pyproject.toml") || has("setup.py") {
        PythonInstall::Editable
    } else {
        PythonInstall::Nothing
    }
}

/// scip-python on the tree (KAIROS-T-0477). On a server build, the
/// dependencies of the project go into a virtual environment of the cache:
/// with `uv sync --frozen` when the project has `uv.lock` (KAIROS-T-0486),
/// else from its `requirements*.txt` files, else its `pyproject.toml` (or
/// `setup.py`) as an editable install. With no dependency file, scip-python
/// reads the sources only.
pub struct PythonRunner {
    tools: LanguageTools,
}

impl PythonRunner {
    pub fn new(options: &crate::runners::ScipOptions) -> Self {
        PythonRunner {
            tools: options.tools.clone(),
        }
    }

    /// The virtual environment of the dependencies of `root`, made or
    /// reused in `deps`: its `bin` folder.
    fn environment(&self, root: &Path, python: &Path, deps: &Path) -> Result<PathBuf, String> {
        let files = python_dependency_files(root);
        let mut hasher = Sha256::new();
        for (name, text) in &files {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update(text);
        }
        let key = crate::tools::hex(&hasher.finalize());
        let venv = deps.join(format!("venv-{}", &key[..16]));
        let done = venv.join(".kairos-installed");
        if done.is_file() {
            return Ok(venv.join("bin"));
        }
        // An environment of other dependency files is left.
        for entry in std::fs::read_dir(deps).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with("venv-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let bins = [bin_of(python)];
        let install = python_install(&files);
        if install == PythonInstall::Uv {
            self.uv_sync(root, python, deps, &venv)?;
            std::fs::write(&done, b"").map_err(|e| e.to_string())?;
            return Ok(venv.join("bin"));
        }
        let mut make = Command::new(python);
        make.args(["-m", "venv"]).arg(&venv);
        environment(&mut make, &self.tools, Some(deps), &bins);
        run_step(&mut make, "python -m venv")?;
        let pip = venv.join("bin/pip");
        let mut command = Command::new(&pip);
        command.args(["install", "--disable-pip-version-check", "--no-input"]);
        match install {
            PythonInstall::Requirements(requirements) => {
                for r in requirements {
                    command.arg("-r").arg(root.join(r));
                }
            }
            PythonInstall::Editable => {
                command.arg("-e").arg(root);
            }
            PythonInstall::Nothing | PythonInstall::Uv => {
                std::fs::write(&done, b"").map_err(|e| e.to_string())?;
                return Ok(venv.join("bin"));
            }
        }
        command.current_dir(root);
        environment(&mut command, &self.tools, Some(deps), &[venv.join("bin")]);
        command.env("PIP_CACHE_DIR", deps.join("pip"));
        run_step(&mut command, "pip install")?;
        std::fs::write(&done, b"").map_err(|e| e.to_string())?;
        Ok(venv.join("bin"))
    }

    /// `uv sync --frozen` of the project at `root` into `venv`, with the
    /// Python of the runner (KAIROS-T-0486). uv is fetched on first need. It
    /// installs the locked versions only, downloads no Python, and keeps its
    /// cache in `deps/uv`.
    fn uv_sync(&self, root: &Path, python: &Path, deps: &Path, venv: &Path) -> Result<(), String> {
        let uv = match self.tools.prepare("uv")? {
            Some(map) => map
                .get("uv")
                .map(|dir| dir.join("uv"))
                .filter(|p| p.exists())
                .ok_or_else(|| "uv has no uv".to_string())?,
            None => on_path("uv").ok_or_else(|| "uv is not on PATH".to_string())?,
        };
        let mut sync = Command::new(&uv);
        sync.args(["sync", "--frozen", "--no-progress", "--python"])
            .arg(python)
            .current_dir(root);
        environment(&mut sync, &self.tools, Some(deps), &[bin_of(python)]);
        sync.env("UV_PROJECT_ENVIRONMENT", venv)
            .env("UV_PYTHON_DOWNLOADS", "never")
            .env("UV_CACHE_DIR", deps.join("uv"));
        run_step(&mut sync, "uv sync").map(|_| ())
    }
}

impl ScipRunner for PythonRunner {
    fn name(&self) -> &'static str {
        "python"
    }
    fn languages(&self) -> &'static [&'static str] {
        &["python"]
    }
    fn run(&self, input: &RunnerInput<'_>) -> Result<ScipOutput, RunnerFailure> {
        let prepared = self.tools.prepare("python").map_err(fallback)?;
        let node = tool(&prepared, "node", "bin/node", "node")?;
        let scip = tool(
            &prepared,
            "scip-python",
            "node_modules/@sourcegraph/scip-python/index.js",
            "scip-python",
        )?;
        let python = tool(&prepared, "python", "bin/python3", "python3")?;
        let deps = self.tools.dependency_dir("python").map_err(fallback)?;
        let mut bins = vec![bin_of(&node), bin_of(&python)];
        if let Some(deps) = &deps {
            let venv = self
                .environment(input.root, &python, deps)
                .map_err(fallback)?;
            bins.insert(0, venv);
        }
        let work = tempfile::tempdir().map_err(|e| fallback(e.to_string()))?;
        let out = work.path().join("index.scip");
        let name = input
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "repository".to_string());
        let mut index = if prepared.is_some() {
            let mut c = Command::new(&node);
            c.arg(&scip);
            c
        } else {
            Command::new(&scip)
        };
        index
            .args(["index", ".", "--quiet", "--project-name"])
            .arg(&name)
            // With no version, scip-python 0.6.6 stops on the first module.
            .args(["--project-version", "0"])
            .arg("--output")
            .arg(&out)
            .current_dir(input.root);
        environment(&mut index, &self.tools, deps.as_deref(), &bins);
        if let Some(venv) = bins.first().filter(|_| deps.is_some()) {
            index.env("VIRTUAL_ENV", venv.parent().unwrap_or(venv));
        }
        let result = run_step(&mut index, "scip-python")
            .and_then(|_| read_index(&out))
            .map(|index| rebase(index, "", proto::ENCODING_UTF16));
        if let Some(cache) = &self.tools.cache {
            cache.enforce_cap("python");
        }
        Ok(ScipOutput {
            index: result.map_err(fallback)?,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages::{Component, DependencyCache, Source};
    use crate::tools::Download;

    #[test]
    fn a_go_step_that_rewrites_a_go_file_is_undone_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module x\n").unwrap();
        let guard = GoFilesGuard::new(root);
        assert_eq!(guard.restore("go mod download"), None);
        std::fs::write(root.join("go.mod"), "module x\n\ngo 1.22\n").unwrap();
        std::fs::write(root.join("go.sum"), "x v1 h1:a\n").unwrap();
        let note = guard.restore("go mod download").unwrap();
        assert_eq!(
            note,
            "go mod download changed go.mod, go.sum; the files are as before"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("go.mod")).unwrap(),
            "module x\n"
        );
        assert!(!root.join("go.sum").exists());
    }

    #[test]
    fn the_projects_of_a_tree_are_the_outer_folders_with_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for f in ["svc/go.mod", "svc/inner/go.mod", "tools/go.mod"] {
            std::fs::create_dir_all(root.join(f).parent().unwrap()).unwrap();
            std::fs::write(root.join(f), "module x\n").unwrap();
        }
        let files = [
            "svc/main.go",
            "svc/inner/a.go",
            "tools/gen/gen.go",
            "loose/x.go",
        ];
        assert_eq!(
            projects(root, &files, &["go.mod"], None),
            BTreeSet::from(["svc".to_string(), "tools".to_string()])
        );
        // With a default, a file with no project goes to the root, and the
        // root holds the rest.
        assert_eq!(
            projects(root, &files, &["go.mod"], Some("")),
            BTreeSet::from([String::new()])
        );
    }

    #[test]
    fn documents_get_paths_from_the_root() {
        let index = proto::Index {
            documents: vec![proto::Document {
                relative_path: "./greet/greet.go".into(),
                occurrences: vec![],
                position_encoding: 0,
            }],
        };
        let index = rebase(index, "go", proto::ENCODING_UTF8);
        assert_eq!(index.documents[0].relative_path, "go/greet/greet.go");
        assert_eq!(index.documents[0].position_encoding, proto::ENCODING_UTF8);
    }

    #[test]
    fn a_tool_that_is_not_there_is_a_fallback() {
        let prepared = Some(BTreeMap::from([("go", PathBuf::from("/nowhere"))]));
        let Err(RunnerFailure::Fallback(reason)) = tool(&prepared, "go", "bin/go", "go") else {
            panic!("not a fallback");
        };
        assert!(reason.contains("go has no bin/go"), "{reason}");
        let Err(RunnerFailure::Fallback(reason)) =
            tool(&None, "x", "x", "kairos-no-such-program-0475")
        else {
            panic!("not a fallback");
        };
        assert_eq!(reason, "kairos-no-such-program-0475 is not on PATH");
    }

    fn dependency_files(names: &[&str]) -> Vec<(String, Vec<u8>)> {
        names.iter().map(|n| (n.to_string(), Vec::new())).collect()
    }

    /// KAIROS-T-0486: a project with `uv.lock` and `pyproject.toml` installs
    /// with uv; the others keep the pip install of KAIROS-T-0477.
    #[test]
    fn the_python_install_follows_the_dependency_files() {
        let plan = |files: &[&str]| python_install(&dependency_files(files));
        assert_eq!(plan(&["pyproject.toml", "uv.lock"]), PythonInstall::Uv);
        assert_eq!(
            plan(&["pyproject.toml", "requirements.txt", "uv.lock"]),
            PythonInstall::Uv
        );
        assert_eq!(
            plan(&["requirements-dev.txt", "requirements.txt"]),
            PythonInstall::Requirements(vec![
                "requirements-dev.txt".into(),
                "requirements.txt".into()
            ])
        );
        assert_eq!(plan(&["pyproject.toml"]), PythonInstall::Editable);
        assert_eq!(plan(&["setup.py"]), PythonInstall::Editable);
        // uv needs the pyproject.toml of the lock.
        assert_eq!(plan(&["uv.lock"]), PythonInstall::Nothing);
        assert_eq!(plan(&[]), PythonInstall::Nothing);
    }

    /// A fake `python3`: `-m venv <dir>` makes `<dir>/bin/pip`, which
    /// writes its arguments to `<dir>/pip.args`.
    const FAKE_PYTHON: &str = "#!/bin/sh\n\
        if [ \"$1\" = \"-m\" ] && [ \"$2\" = \"venv\" ]; then\n\
          mkdir -p \"$3/bin\"\n\
          printf '#!/bin/sh\\necho \"$@\" > \"%s/pip.args\"\\n' \"$3\" > \"$3/bin/pip\"\n\
          chmod +x \"$3/bin/pip\"\n\
        fi\n";

    /// A fake `uv` that writes its arguments and the environment folder,
    /// then makes the environment.
    const FAKE_UV: &str = "#!/bin/sh\n\
        mkdir -p \"$UV_PROJECT_ENVIRONMENT/bin\"\n\
        echo \"$@\" > \"$UV_PROJECT_ENVIRONMENT/uv.args\"\n\
        echo \"$UV_PYTHON_DOWNLOADS $UV_CACHE_DIR\" > \"$UV_PROJECT_ENVIRONMENT/uv.env\"\n";

    const FAILING_UV: &str = "#!/bin/sh\necho 'the lock file needs to be updated' >&2\nexit 2\n";

    /// A component of `name` from a fake archive in the tools folder.
    fn fake_component(dir: &Path, name: &'static str, files: &[(&str, &str)]) -> Component {
        let file = format!("{name}-fake.tar.gz");
        let (_, sha) = crate::languages::tests::fake_archive(dir, &file, files);
        Component {
            name,
            version: "0.0.0",
            source: Some(Source::Archive {
                download: Download::new(format!("http://127.0.0.1:1/{file}"), sha),
                strip: 0,
            }),
        }
    }

    /// The fake tools of the Python runner, and a dependency cache, in
    /// `dir`. node fails, so scip-python fails after the install.
    fn fake_python_tools(dir: &Path, uv: &str) -> LanguageTools {
        let tools = dir.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        LanguageTools {
            dir: Some(tools.clone()),
            fetch: true,
            overrides: vec![
                fake_component(&tools, "node", &[("bin/node", "#!/bin/sh\nexit 1\n")]),
                fake_component(
                    &tools,
                    "scip-python",
                    &[("node_modules/@sourcegraph/scip-python/index.js", "")],
                ),
                fake_component(&tools, "python", &[("bin/python3", FAKE_PYTHON)]),
                fake_component(&tools, "uv", &[("uv", uv)]),
            ],
            cache: Some(DependencyCache {
                root: dir.join("deps"),
                key: "t/r".into(),
                cap_bytes: u64::MAX,
                min_free_bytes: 0,
            }),
            ..Default::default()
        }
    }

    fn python_project(dir: &Path, files: &[&str]) -> PathBuf {
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("app.py"), "def run():\n    return 1\n").unwrap();
        for f in files {
            std::fs::write(root.join(f), "").unwrap();
        }
        root
    }

    /// The one venv of the cache.
    fn venv_of(dir: &Path) -> PathBuf {
        let python = dir.join("deps/t/r/python");
        std::fs::read_dir(&python)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("venv-")
            })
            .unwrap_or_else(|| panic!("no venv in {}", python.display()))
    }

    fn tool_folders(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir.join("tools"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        out
    }

    fn run_python(dir: &Path, root: &Path, tools: LanguageTools) -> crate::BuildReport {
        let mut options = crate::BuildOptions {
            download: false,
            ..Default::default()
        };
        options.scip.tools = tools;
        crate::build_structure_with(root, &dir.join("index.sqlite"), &options).unwrap()
    }

    /// KAIROS-T-0486: with `uv.lock`, the server build fetches uv and runs
    /// `uv sync --frozen` into the venv of the cache, with the fetched
    /// Python and no Python download.
    #[test]
    fn a_uv_lock_installs_with_uv() {
        let dir = tempfile::tempdir().unwrap();
        let root = python_project(dir.path(), &["pyproject.toml", "uv.lock"]);
        run_python(dir.path(), &root, fake_python_tools(dir.path(), FAKE_UV));
        let venv = venv_of(dir.path());
        let args = std::fs::read_to_string(venv.join("uv.args")).unwrap();
        assert!(args.starts_with("sync --frozen"), "{args}");
        assert!(
            args.contains(&format!(
                "--python {}",
                dir.path().join("tools/python-0.0.0/bin/python3").display()
            )),
            "{args}"
        );
        let env = std::fs::read_to_string(venv.join("uv.env")).unwrap();
        assert!(env.starts_with("never "), "{env}");
        assert!(env.contains("deps/t/r/python/uv"), "{env}");
        assert!(!venv.join("pip.args").exists());
        assert!(venv.join(".kairos-installed").is_file());
        assert_eq!(
            tool_folders(dir.path()),
            [
                "node-0.0.0",
                "python-0.0.0",
                "scip-python-0.0.0",
                "uv-0.0.0"
            ]
        );
    }

    /// KAIROS-T-0486: a failed `uv sync` is the fallback with a warning.
    #[test]
    fn a_failed_uv_sync_falls_back_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let root = python_project(dir.path(), &["pyproject.toml", "uv.lock"]);
        let report = run_python(dir.path(), &root, fake_python_tools(dir.path(), FAILING_UV));
        let python: Vec<_> = report
            .scip_warnings
            .iter()
            .filter(|w| w.language == "python")
            .collect();
        assert_eq!(python.len(), 1, "{:?}", report.scip_warnings);
        assert!(
            python[0].reason.starts_with("uv sync failed"),
            "{}",
            python[0].reason
        );
        assert!(
            python[0]
                .reason
                .contains("the lock file needs to be updated"),
            "{}",
            python[0].reason
        );
        let python = dir.path().join("deps/t/r/python");
        assert!(
            std::fs::read_dir(&python)
                .unwrap()
                .flatten()
                .all(|e| !e.path().join(".kairos-installed").exists()),
            "an environment is marked as installed"
        );
    }

    /// KAIROS-T-0486: with only `requirements.txt`, pip installs, and uv is
    /// never fetched.
    #[test]
    fn requirements_alone_install_with_pip() {
        let dir = tempfile::tempdir().unwrap();
        let root = python_project(dir.path(), &["requirements.txt"]);
        run_python(dir.path(), &root, fake_python_tools(dir.path(), FAKE_UV));
        let venv = venv_of(dir.path());
        let args = std::fs::read_to_string(venv.join("pip.args")).unwrap();
        assert!(args.starts_with("install "), "{args}");
        assert!(
            args.contains(&format!("-r {}", root.join("requirements.txt").display())),
            "{args}"
        );
        assert!(!venv.join("uv.args").exists());
        assert_eq!(
            tool_folders(dir.path()),
            ["node-0.0.0", "python-0.0.0", "scip-python-0.0.0"]
        );
    }
}
