//! The SCIP runners of a build, one for each language family (KAIROS-T-0473,
//! COLLIERY-I-0622).
//!
//! A runner gives the SCIP index of the files of its languages: rust-analyzer
//! for Rust ([`crate::scip`]), scip-go for Go, scip-typescript for
//! TypeScript, TSX and JavaScript, scip-python for Python. The edges and the
//! uses of a language that has a SCIP index for the build come from it, the
//! way the Rust edges always did ([`crate::edges`], [`crate::refs`]). The
//! parts that only Rust has stay Rust only: the qualified paths, the crates of
//! a workspace, the calls in the text of a macro.
//!
//! **A runner that fails.** When the tool of a runner is missing, or its
//! dependency step or its run fails, its languages get the edges by name, as
//! a build with no runner, and the build records a warning: the language and
//! the reason ([`ScipWarning`]). The build goes on. The Rust runner is the
//! exception: its errors stop the build, as before (its tools are pinned and
//! checked, and a wrong tool is an error of the deployment).
//!
//! **Paths.** A runner gives the documents of its index with paths from the
//! root of the tree, and with a known position encoding.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::scip::{Crates, proto};
use crate::{IndexError, ScipRun};

/// The languages that have a SCIP runner, as `files.language` names them.
pub const SCIP_LANGUAGES: [&str; 6] = ["rust", "go", "typescript", "tsx", "javascript", "python"];

/// A language with no SCIP index for a build, and why (KAIROS-T-0473). Its
/// files have the edges by name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScipWarning {
    /// The name of the runner: `rust`, `go`, `typescript` or `python`.
    pub language: String,
    /// What failed, as a user can read it.
    pub reason: String,
}

/// One runner that gave a SCIP index for a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageRun {
    /// The name of the runner.
    pub language: String,
    pub elapsed: Duration,
    pub documents: usize,
}

/// What the runner of a language reads.
pub struct RunnerInput<'a> {
    /// The root of the tree.
    pub root: &'a Path,
    /// The parsed files of the languages of the runner, from the root,
    /// sorted.
    pub files: &'a [&'a str],
}

/// The SCIP index of a runner.
#[derive(Debug, Default)]
pub struct ScipOutput {
    /// The documents, with paths from the root of the tree.
    pub index: proto::Index,
    /// The crates of the module files (Rust only).
    pub crates: Crates,
    /// The details of the rust-analyzer run (Rust only).
    pub rust: Option<ScipRun>,
}

/// Why a runner gave no index.
#[derive(Debug)]
pub enum RunnerFailure {
    /// The build stops with this error.
    Fatal(IndexError),
    /// The languages of the runner get the edges by name. The text is the
    /// reason of the warning.
    Fallback(String),
}

impl From<IndexError> for RunnerFailure {
    fn from(e: IndexError) -> Self {
        RunnerFailure::Fatal(e)
    }
}

/// The SCIP indexer of one language family.
pub trait ScipRunner: Send + Sync {
    /// The name of the runner in a warning: `rust`, `go`, `typescript`,
    /// `python`.
    fn name(&self) -> &'static str;
    /// The languages of the files that its index covers.
    fn languages(&self) -> &'static [&'static str];
    /// Whether the tree needs the runner. By default: it has files of the
    /// languages of the runner.
    fn wanted(&self, input: &RunnerInput<'_>) -> bool {
        !input.files.is_empty()
    }
    /// Run the indexer on the tree.
    fn run(&self, input: &RunnerInput<'_>) -> Result<ScipOutput, RunnerFailure>;
}

/// The SCIP indexes of a build, merged: the documents of each runner that
/// gave one, and the languages that they cover.
#[derive(Debug, Default)]
pub struct ScipData {
    pub index: proto::Index,
    pub crates: Crates,
    /// The file languages that have a SCIP index in this build.
    pub languages: HashSet<&'static str>,
}

impl ScipData {
    /// Add the index of a runner of `languages`.
    pub fn add(&mut self, languages: &[&'static str], output: ScipOutput) {
        self.languages.extend(languages.iter().copied());
        self.index.documents.extend(output.index.documents);
        if !output.crates.file_crate.is_empty() {
            self.crates = output.crates;
        }
    }

    /// Whether files of `language` have a SCIP index in this build.
    pub fn covers(&self, language: &str) -> bool {
        self.languages.contains(language)
    }
}

/// A runner that reads a SCIP index file that another tool made: for a CI
/// that runs the indexer, and for the tests. The paths of its documents are
/// from the root of the tree.
#[derive(Debug, Clone)]
pub struct IndexFileRunner {
    pub name: &'static str,
    pub languages: &'static [&'static str],
    pub file: std::path::PathBuf,
}

impl ScipRunner for IndexFileRunner {
    fn name(&self) -> &'static str {
        self.name
    }
    fn languages(&self) -> &'static [&'static str] {
        self.languages
    }
    fn run(&self, _input: &RunnerInput<'_>) -> Result<ScipOutput, RunnerFailure> {
        let bytes = std::fs::read(&self.file).map_err(|e| {
            RunnerFailure::Fallback(format!("cannot read {}: {e}", self.file.display()))
        })?;
        Ok(ScipOutput {
            index: decode(&bytes).map_err(RunnerFailure::Fallback)?,
            ..Default::default()
        })
    }
}

/// A runner that always fails with the same reason: for the tests, and
/// for a language whose tools a deployment does not have.
#[derive(Debug, Clone)]
pub struct MissingRunner {
    pub name: &'static str,
    pub languages: &'static [&'static str],
    pub reason: String,
}

impl ScipRunner for MissingRunner {
    fn name(&self) -> &'static str {
        self.name
    }
    fn languages(&self) -> &'static [&'static str] {
        self.languages
    }
    fn run(&self, _input: &RunnerInput<'_>) -> Result<ScipOutput, RunnerFailure> {
        Err(RunnerFailure::Fallback(self.reason.clone()))
    }
}

/// Decode a SCIP index file.
pub fn decode(bytes: &[u8]) -> Result<proto::Index, String> {
    <proto::Index as prost::Message>::decode(bytes)
        .map_err(|e| format!("the SCIP file is not valid: {e}"))
}

/// The runners of a build: the runners of `options` first, then the
/// built-in runner of each other language.
pub(crate) fn runners(options: &crate::BuildOptions) -> Vec<Arc<dyn ScipRunner>> {
    let mut out: Vec<Arc<dyn ScipRunner>> = options.scip.runners.clone();
    let named: HashSet<&str> = out.iter().map(|r| r.name()).collect();
    let builtin: Vec<Arc<dyn ScipRunner>> = vec![Arc::new(RustRunner {
        options: options.clone(),
    })];
    out.extend(builtin.into_iter().filter(|r| !named.contains(r.name())));
    out
}

/// rust-analyzer on the Cargo workspace at the root of the tree.
struct RustRunner {
    options: crate::BuildOptions,
}

impl ScipRunner for RustRunner {
    fn name(&self) -> &'static str {
        "rust"
    }
    fn languages(&self) -> &'static [&'static str] {
        &["rust"]
    }
    fn wanted(&self, input: &RunnerInput<'_>) -> bool {
        !input.files.is_empty() && input.root.join("Cargo.toml").is_file()
    }
    fn run(&self, input: &RunnerInput<'_>) -> Result<ScipOutput, RunnerFailure> {
        let (run, index, crates) = crate::scip::run(input.root, &self.options)?;
        Ok(ScipOutput {
            index,
            crates,
            rust: Some(run),
        })
    }
}

/// The options of the SCIP runners of a build, other than rust-analyzer.
#[derive(Clone, Default)]
pub struct ScipOptions {
    /// Runners that replace the built-in runner of the same name: an index
    /// file that a CI made, or a test runner.
    pub runners: Vec<Arc<dyn ScipRunner>>,
}

impl std::fmt::Debug for ScipOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScipOptions")
            .field(
                "runners",
                &self.runners.iter().map(|r| r.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{BuildOptions, Index, Lookup, UpdateOptions, build_structure_with};

    /// An occurrence: line, column and length, from 0, on one line.
    pub(crate) fn occ(
        line: i32,
        col: i32,
        len: i32,
        symbol: &str,
        roles: i32,
    ) -> proto::Occurrence {
        proto::Occurrence {
            range: vec![line, col, col + len],
            symbol: symbol.into(),
            symbol_roles: roles,
        }
    }

    /// Write `index` as a SCIP file.
    pub(crate) fn write_index(path: &Path, index: &proto::Index) {
        std::fs::write(path, prost::Message::encode_to_vec(index)).unwrap();
    }

    const A: &str = "def load():\n    return 1\n";
    const B: &str = "from a import load\n\n\ndef run():\n    return load()\n";
    const C: &str = "def load():\n    return 2\n";
    const S: &str = "scip-python python t 0.1 ";

    /// A tree of 3 Python files: `run` in b.py calls the `load` of a.py,
    /// and c.py has another `load`. The SCIP index of scip-python names the
    /// one of a.py.
    fn python_tree(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), A).unwrap();
        std::fs::write(repo.join("b.py"), B).unwrap();
        std::fs::write(repo.join("c.py"), C).unwrap();
        let def = proto::ROLE_DEFINITION;
        let a_load = format!("{S}`a`/load().");
        let c_load = format!("{S}`c`/load().");
        let doc = |path: &str, occurrences| proto::Document {
            relative_path: path.into(),
            occurrences,
            position_encoding: proto::ENCODING_UTF16,
        };
        let index = proto::Index {
            documents: vec![
                doc("a.py", vec![occ(0, 4, 4, &a_load, def)]),
                doc(
                    "b.py",
                    vec![
                        occ(0, 14, 4, &a_load, 0x2),
                        occ(3, 4, 3, &format!("{S}`b`/run()."), def),
                        occ(4, 11, 4, &a_load, 0),
                    ],
                ),
                doc("c.py", vec![occ(0, 4, 4, &c_load, def)]),
            ],
        };
        let file = dir.join("python.scip");
        write_index(&file, &index);
        (repo, file)
    }

    fn with_runner(runner: Arc<dyn ScipRunner>) -> BuildOptions {
        let mut options = BuildOptions {
            download: false,
            ..Default::default()
        };
        options.scip.runners.push(runner);
        options
    }

    fn python_runner(file: &Path) -> Arc<dyn ScipRunner> {
        Arc::new(IndexFileRunner {
            name: "python",
            languages: &["python"],
            file: file.to_path_buf(),
        })
    }

    /// The (class, origin, callee file, pending) of the edges of `run`.
    fn run_edges(db: &Path) -> Vec<(String, String, Option<String>, bool)> {
        Index::open(db)
            .unwrap()
            .edges()
            .unwrap()
            .into_iter()
            .filter(|e| e.caller.name == "run")
            .map(|e| (e.class, e.origin, e.callee.map(|c| c.file), e.scip_pending))
            .collect()
    }

    fn load_of(db: &Path, file: &str) -> i64 {
        match Index::open(db)
            .unwrap()
            .lookup("load", Some(file), None)
            .unwrap()
        {
            Lookup::Found(s) => s.id,
            other => panic!("{other:?}"),
        }
    }

    /// KAIROS-T-0473: a language with a SCIP index gets its edges and its
    /// uses from it, as Rust does.
    #[test]
    fn a_python_scip_index_gives_certain_edges_and_uses() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, file) = python_tree(dir.path());
        let db = dir.path().join("index.sqlite");

        // No runner: the name class lists the 2 functions.
        build_structure_with(&repo, &db, &BuildOptions::default()).unwrap();
        assert_eq!(
            run_edges(&db),
            [("possible".to_string(), "name".to_string(), None, false)]
        );

        let report = build_structure_with(&repo, &db, &with_runner(python_runner(&file))).unwrap();
        assert!(report.scip_warnings.is_empty(), "{report:?}");
        assert_eq!(report.scip_runs.len(), 1);
        assert_eq!(report.scip_runs[0].language, "python");
        assert_eq!(
            run_edges(&db),
            [(
                "certain".to_string(),
                "scip".to_string(),
                Some("a.py".to_string()),
                false
            )]
        );
        let index = Index::open(&db).unwrap();
        let refs = index.references(load_of(&db, "a.py"), false).unwrap();
        assert_eq!(refs.coverage, crate::Coverage::Full);
        let kinds: Vec<(&str, u32)> = refs
            .uses
            .iter()
            .map(|u| (u.kind.as_str(), u.line))
            .collect();
        assert_eq!(kinds, [("import", 1), ("call", 5)]);
    }

    /// KAIROS-T-0473: a runner that fails gives the edges by name and a
    /// warning; the build goes on.
    #[test]
    fn a_failing_runner_falls_back_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, _) = python_tree(dir.path());
        let db = dir.path().join("index.sqlite");
        let failing = Arc::new(MissingRunner {
            name: "python",
            languages: &["python"],
            reason: "scip-python is not on PATH".into(),
        });
        let report = build_structure_with(&repo, &db, &with_runner(failing)).unwrap();
        assert_eq!(
            report.scip_warnings,
            [ScipWarning {
                language: "python".into(),
                reason: "scip-python is not on PATH".into()
            }]
        );
        assert!(report.scip_runs.is_empty());
        assert_eq!(
            run_edges(&db),
            [("possible".to_string(), "name".to_string(), None, false)]
        );
        let index = Index::open(&db).unwrap();
        let refs = index.references(load_of(&db, "a.py"), true).unwrap();
        assert_eq!(refs.coverage, crate::Coverage::CallsOnly);
    }

    /// A tree with no file of a runner's languages does not run it.
    #[test]
    fn a_runner_with_no_files_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("main.go"), "package main\nfunc main() {}\n").unwrap();
        let failing = Arc::new(MissingRunner {
            name: "python",
            languages: &["python"],
            reason: "never".into(),
        });
        let report =
            build_structure_with(&repo, &dir.path().join("i.sqlite"), &with_runner(failing))
                .unwrap();
        assert!(report.scip_warnings.is_empty());
    }

    /// KAIROS-T-0473: an update with no SCIP run keeps the SCIP edges of an
    /// unchanged Python function, and a changed one waits for a SCIP run.
    #[test]
    fn an_update_keeps_the_scip_edges_of_another_language() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, file) = python_tree(dir.path());
        let db = dir.path().join("index.sqlite");
        build_structure_with(&repo, &db, &with_runner(python_runner(&file))).unwrap();

        // c.py changed: run did not, so it keeps its edge and its uses.
        std::fs::write(repo.join("c.py"), "def load():\n    return 3\n").unwrap();
        crate::update_structure(&repo, &db, &UpdateOptions::default()).unwrap();
        assert_eq!(
            run_edges(&db),
            [(
                "certain".to_string(),
                "scip".to_string(),
                Some("a.py".to_string()),
                false
            )]
        );
        let refs = Index::open(&db)
            .unwrap()
            .references(load_of(&db, "a.py"), false)
            .unwrap();
        assert_eq!(refs.coverage, crate::Coverage::Pending(1));
        assert_eq!(refs.uses.len(), 2);

        // run changed: its call is by name until the next SCIP run.
        std::fs::write(repo.join("b.py"), B.replace("load()", "load() + 1")).unwrap();
        crate::update_structure(&repo, &db, &UpdateOptions::default()).unwrap();
        assert_eq!(
            run_edges(&db),
            [("possible".to_string(), "name".to_string(), None, true)]
        );
    }
}
