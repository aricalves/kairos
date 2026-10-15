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
        let output = each_project(&modules, |module| {
            let dir = input.root.join(module);
            if deps.is_some() {
                let mut download = Command::new(&go);
                download.args(["mod", "download"]).current_dir(&dir);
                go_env(&mut download);
                run_step(&mut download, "go mod download")?;
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
            run_step(&mut index, "scip-go")?;
            Ok(rebase(read_index(&out)?, module, proto::ENCODING_UTF8))
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

/// scip-python on the tree (KAIROS-T-0477). On a server build, the
/// dependencies of the project go into a virtual environment of the cache,
/// from its `requirements*.txt` files, else its `pyproject.toml` (or
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
        let mut make = Command::new(python);
        make.args(["-m", "venv"]).arg(&venv);
        environment(&mut make, &self.tools, Some(deps), &bins);
        run_step(&mut make, "python -m venv")?;
        let pip = venv.join("bin/pip");
        let requirements: Vec<&String> = files
            .iter()
            .map(|(n, _)| n)
            .filter(|n| n.starts_with("requirements"))
            .collect();
        let mut install = Command::new(&pip);
        install.args(["install", "--disable-pip-version-check", "--no-input"]);
        if !requirements.is_empty() {
            for r in requirements {
                install.arg("-r").arg(root.join(r));
            }
        } else if files
            .iter()
            .any(|(n, _)| matches!(n.as_str(), "pyproject.toml" | "setup.py"))
        {
            install.arg("-e").arg(root);
        } else {
            // No dependency file: an empty environment.
            std::fs::write(&done, b"").map_err(|e| e.to_string())?;
            return Ok(venv.join("bin"));
        }
        install.current_dir(root);
        environment(&mut install, &self.tools, Some(deps), &[venv.join("bin")]);
        install.env("PIP_CACHE_DIR", deps.join("pip"));
        run_step(&mut install, "pip install")?;
        std::fs::write(&done, b"").map_err(|e| e.to_string())?;
        Ok(venv.join("bin"))
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
}
