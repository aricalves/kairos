//! The SCIP edges of Go, TypeScript, JavaScript and Python (KAIROS-T-0475,
//! KAIROS-T-0476, KAIROS-T-0477).
//!
//! Each language has 2 tests on its fixture in `tests/fixtures/scip-*`:
//!
//! - with the SCIP index that the real indexer made of the fixture, kept in
//!   `tests/fixtures/scip/` (no tool and no network);
//! - with the real indexer, in the 2 modes: a server build (the tools in a
//!   tools folder, fetched on first need, with a dependency cache) and a
//!   local run (the tools on `PATH`). The test does nothing when the tools
//!   are not there. `KAIROS_INDEX_SCIP_TOOLS=<folder>` gives a tools folder:
//!   the test fetches the pinned tools into it on first need.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kairos_index::languages::{DependencyCache, LanguageTools};
use kairos_index::{
    BuildOptions, BuildReport, Index, IndexFileRunner, Lookup, ScipRunner, build_structure_with,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A copy of the fixture `name` in a temporary folder.
fn copy(name: &str, to: &Path) -> PathBuf {
    fn walk(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, &to.join(entry.file_name()));
            } else {
                std::fs::copy(&path, to.join(entry.file_name())).unwrap();
            }
        }
    }
    let root = to.join(name);
    walk(&fixture(name), &root);
    root
}

/// Build the index of a copy of the fixture with `options`.
fn build(name: &str, options: &BuildOptions) -> (tempfile::TempDir, PathBuf, BuildReport) {
    let dir = tempfile::tempdir().unwrap();
    let root = copy(name, dir.path());
    let db = dir.path().join("index.sqlite");
    let report = build_structure_with(&root, &db, options).unwrap();
    (dir, db, report)
}

fn quiet() -> BuildOptions {
    BuildOptions {
        download: false,
        ..Default::default()
    }
}

/// The options with the recorded index of `language` for its fixture.
fn recorded(name: &'static str, languages: &'static [&'static str], file: &str) -> BuildOptions {
    let mut options = quiet();
    let runner: Arc<dyn ScipRunner> = Arc::new(IndexFileRunner {
        name,
        languages,
        file: fixture("scip").join(file),
    });
    options.scip.runners.push(runner);
    options
}

/// The edges of the function `caller`: (written name, class, origin, the
/// callee as `file:Container.name`).
fn edges_of(db: &Path, caller: &str) -> Vec<(String, String, String, Option<String>)> {
    Index::open(db)
        .unwrap()
        .edges()
        .unwrap()
        .into_iter()
        .filter(|e| e.caller.name == caller)
        .map(|e| {
            (
                e.callee_name,
                e.class,
                e.origin,
                e.callee.map(|c| match c.container {
                    Some(k) => format!("{}:{k}.{}", c.file, c.name),
                    None => format!("{}:{}", c.file, c.name),
                }),
            )
        })
        .collect()
}

fn id_of(db: &Path, name: &str, file: &str) -> i64 {
    match Index::open(db)
        .unwrap()
        .lookup(name, Some(file), None)
        .unwrap()
    {
        Lookup::Found(s) => s.id,
        other => panic!("{name}: {other:?}"),
    }
}

/// (kind, file, line) of each use of the symbol.
fn uses_of(
    db: &Path,
    name: &str,
    file: &str,
) -> (kairos_index::Coverage, Vec<(String, String, u32)>) {
    let refs = Index::open(db)
        .unwrap()
        .references(id_of(db, name, file), false)
        .unwrap();
    (
        refs.coverage,
        refs.uses
            .into_iter()
            .map(|u| (u.kind, u.file, u.line))
            .collect(),
    )
}

/// The real tools in the 2 modes, when they are there: the server mode (a
/// tools folder from `KAIROS_INDEX_SCIP_TOOLS`, fetched on first need, and
/// a dependency cache) and the local mode (the tools on `PATH`).
fn real_modes(programs: &[&str], cache: &Path) -> Vec<(&'static str, BuildOptions)> {
    let mut modes = Vec::new();
    if let Some(dir) = std::env::var_os("KAIROS_INDEX_SCIP_TOOLS") {
        let mut options = quiet();
        options.scip.tools = LanguageTools {
            dir: Some(PathBuf::from(dir)),
            fetch: true,
            cache: Some(DependencyCache {
                root: cache.to_path_buf(),
                key: "test/repository".into(),
                cap_bytes: 4 * 1024 * 1024 * 1024,
                min_free_bytes: 1024 * 1024 * 1024,
            }),
            ..Default::default()
        };
        modes.push(("server", options));
    }
    if programs
        .iter()
        .all(|p| kairos_index::languages::on_path(p).is_some())
    {
        modes.push(("local", quiet()));
    }
    if modes.is_empty() {
        eprintln!(
            "no {} on PATH and no KAIROS_INDEX_SCIP_TOOLS: the test with the real indexer does nothing",
            programs.join(", ")
        );
    }
    modes
}

fn s(text: &str) -> String {
    text.to_string()
}

// ---------------------------------------------------------------------------
// Go (KAIROS-T-0475)

fn check_go(db: &Path, report: &BuildReport) {
    assert!(
        report.scip_warnings.iter().all(|w| w.language != "go"),
        "{:?}",
        report.scip_warnings
    );
    // `sq.Area()` is the Area of Square, not of Circle; `shapes.Total` is
    // the function of the package.
    assert_eq!(
        edges_of(db, "Big"),
        [
            (
                s("Square.Area"),
                s("certain"),
                s("scip"),
                Some(s("shapes/shapes.go:Square.Area"))
            ),
            (
                s("shapes.Total"),
                s("certain"),
                s("scip"),
                Some(s("shapes/shapes.go:Total"))
            ),
        ]
    );
    // A call through the interface goes to the method of the interface: it
    // has no symbol, so the edge is certain with no callee.
    assert_eq!(
        edges_of(db, "Total"),
        [(s("Shape.Area"), s("certain"), s("scip"), None)]
    );
    let (coverage, uses) = uses_of(db, "Square", "shapes/shapes.go");
    assert_eq!(coverage, kairos_index::Coverage::Full);
    // The type and its field `Side`, in the other package and in the
    // methods of Square.
    assert_eq!(
        uses,
        [
            (s("type"), s("report/report.go"), 7),
            (s("field"), s("report/report.go"), 7),
            (s("type"), s("shapes/shapes.go"), 12),
            (s("field"), s("shapes/shapes.go"), 12),
            (s("field"), s("shapes/shapes.go"), 12),
        ]
    );
    let (_, uses) = uses_of(db, "Shape", "shapes/shapes.go");
    assert_eq!(
        uses,
        [
            (s("type"), s("report/report.go"), 8),
            (s("type"), s("shapes/shapes.go"), 21),
            // The call of its method.
            (s("call"), s("shapes/shapes.go"), 24),
        ]
    );
}

#[test]
fn go_from_the_recorded_index_of_scip_go() {
    let (_dir, db, report) = build("scip-go", &recorded("go", &["go"], "go.scip"));
    check_go(&db, &report);
}

#[test]
fn go_with_no_index_has_name_edges() {
    let mut options = quiet();
    options
        .scip
        .runners
        .push(Arc::new(kairos_index::MissingRunner {
            name: "go",
            languages: &["go"],
            reason: "scip-go is not on PATH".into(),
        }));
    let (_dir, db, report) = build("scip-go", &options);
    assert_eq!(report.scip_warnings.len(), 1);
    // By name, the 2 Area methods are candidates.
    assert_eq!(
        edges_of(&db, "Big")[0],
        (s("sq.Area"), s("possible"), s("name"), None)
    );
}

#[test]
fn go_with_scip_go() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, options) in real_modes(&["go", "scip-go"], cache.path()) {
        eprintln!("Go, {mode}");
        let (_dir, db, report) = build("scip-go", &options);
        check_go(&db, &report);
    }
}

/// KAIROS-T-0667: `scip-go` with `GOFLAGS=-mod=mod` rewrites `go.mod` of a
/// module that imports a package it does not require (it raised `go` to the
/// version of the toolchain, added the requirement and wrote `go.sum`).
/// The summary run then stopped with `Changed("go.mod")`. With the real
/// tools, the build leaves both files as they were and the summaries run.
/// Needs the network, for the module proxy.
#[test]
fn go_leaves_go_mod_as_it_was_when_scip_go_would_rewrite_it() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, options) in real_modes(&["go", "scip-go"], cache.path()) {
        if mode != "server" {
            continue;
        }
        eprintln!("Go with a missing requirement, {mode}");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        std::fs::create_dir_all(&root).unwrap();
        let go_mod = "module example.com/app\n\ngo 1.22\n";
        std::fs::write(root.join("go.mod"), go_mod).unwrap();
        std::fs::write(
            root.join("main.go"),
            "package main\n\nimport (\n\t\"fmt\"\n\n\t\"golang.org/x/text/cases\"\n\t\"golang.org/x/text/language\"\n)\n\nfunc main() {\n\tfmt.Println(cases.Title(language.English).String(\"hi\"))\n}\n",
        )
        .unwrap();
        let db = dir.path().join("index.sqlite");
        let report = build_structure_with(&root, &db, &options).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("go.mod")).unwrap(),
            go_mod
        );
        assert!(!root.join("go.sum").exists());
        // The run says which step rewrote the files.
        assert!(
            report
                .scip_warnings
                .iter()
                .any(|w| w.language == "go" && w.reason.contains("changed go.mod")),
            "{:?}",
            report.scip_warnings
        );
        // The summaries run: no file changed after the build of the structure.
        kairos_index::summarize(
            &root,
            &db,
            &mut kairos_index::FakeSummarizer::default(),
            &kairos_embed::DeterministicProvider::default(),
            &kairos_index::SummarizeOptions::default(),
        )
        .expect("the summary run finds no changed file");
    }
}

/// KAIROS-T-0474: the tools of a language are fetched only for a tree with
/// files of that language. The pins point to a closed port, so a fetch
/// fails at once, with no network.
#[test]
fn a_tree_with_no_go_never_fetches_the_go_tools() {
    use kairos_index::languages::{Component, Source};
    use kairos_index::tools::Download;
    let tools = tempfile::tempdir().unwrap();
    let unreachable = |name: &'static str| Component {
        name,
        version: "0.0.0",
        source: Some(Source::Archive {
            download: Download::new(format!("http://127.0.0.1:1/{name}.tar.gz"), "0".repeat(64)),
            strip: 0,
        }),
    };
    let mut options = quiet();
    options.scip.tools = LanguageTools {
        dir: Some(tools.path().to_path_buf()),
        fetch: true,
        overrides: vec![unreachable("go"), unreachable("scip-go")],
        ..Default::default()
    };

    // A tree of Terraform and Markdown: no runner, no fetch.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.tf"), "variable \"x\" {}\n").unwrap();
    std::fs::write(root.join("README.md"), "# Notes\n").unwrap();
    let report = build_structure_with(&root, &dir.path().join("i.sqlite"), &options).unwrap();
    assert!(
        report.scip_warnings.is_empty(),
        "{:?}",
        report.scip_warnings
    );
    assert_eq!(std::fs::read_dir(tools.path()).unwrap().count(), 0);

    // The Go fixture: the fetch is tried, fails, and Go falls back.
    let go_warning = |options: &BuildOptions| {
        let (_dir, _db, report) = build("scip-go", options);
        let go: Vec<String> = report
            .scip_warnings
            .iter()
            .filter(|w| w.language == "go")
            .map(|w| w.reason.clone())
            .collect();
        assert_eq!(go.len(), 1, "{:?}", report.scip_warnings);
        go[0].clone()
    };
    let reason = go_warning(&options);
    assert!(reason.contains("127.0.0.1:1/go.tar.gz"), "{reason}");

    // A file in the tools folder with a wrong sha256 is refused and named in
    // the warning, and it stays: the operator removes it.
    std::fs::write(tools.path().join("go.tar.gz"), b"not go").unwrap();
    let reason = go_warning(&options);
    assert!(
        reason.contains("go.tar.gz") && reason.contains("sha256"),
        "{reason}"
    );
    assert!(reason.contains(&"0".repeat(64)), "{reason}");
    assert_eq!(
        std::fs::read(tools.path().join("go.tar.gz")).unwrap(),
        b"not go"
    );
}

// ---------------------------------------------------------------------------
// TypeScript and JavaScript (KAIROS-T-0476)

fn check_typescript(db: &Path, report: &BuildReport) {
    assert!(
        report
            .scip_warnings
            .iter()
            .all(|w| w.language != "typescript"),
        "{:?}",
        report.scip_warnings
    );
    // 2 modules have a `format`; the import of main.ts names the one of b.ts.
    assert_eq!(
        edges_of(db, "run"),
        [
            (
                s("Queue"),
                s("certain"),
                s("scip"),
                Some(s("src/queue.ts:Queue"))
            ),
            (
                s("Queue.push"),
                s("certain"),
                s("scip"),
                Some(s("src/queue.ts:Queue.push"))
            ),
            (
                s("b.format"),
                s("certain"),
                s("scip"),
                Some(s("src/b.ts:format"))
            ),
        ]
    );
    let (coverage, uses) = uses_of(db, "format", "src/b.ts");
    assert_eq!(coverage, kairos_index::Coverage::Full);
    assert_eq!(
        uses,
        [
            (s("import"), s("src/main.ts"), 1),
            (s("call"), s("src/main.ts"), 7)
        ]
    );
}

fn check_javascript(db: &Path, report: &BuildReport) {
    assert!(
        report
            .scip_warnings
            .iter()
            .all(|w| w.language != "typescript"),
        "{:?}",
        report.scip_warnings
    );
    assert_eq!(
        edges_of(db, "main"),
        [(
            s("lib.helper"),
            s("certain"),
            s("scip"),
            Some(s("lib.js:helper"))
        )]
    );
}

#[test]
fn typescript_from_the_recorded_index_of_scip_typescript() {
    let languages = &["typescript", "tsx", "javascript"];
    let (_dir, db, report) = build("scip-ts", &recorded("typescript", languages, "ts.scip"));
    check_typescript(&db, &report);
    let (_dir, db, report) = build("scip-js", &recorded("typescript", languages, "js.scip"));
    check_javascript(&db, &report);
}

#[test]
fn typescript_with_no_index_has_name_edges() {
    let mut options = quiet();
    options
        .scip
        .runners
        .push(Arc::new(kairos_index::MissingRunner {
            name: "typescript",
            languages: &["typescript", "tsx", "javascript"],
            reason: "npm ci failed".into(),
        }));
    let (_dir, db, report) = build("scip-ts", &options);
    assert_eq!(report.scip_warnings[0].reason, "npm ci failed");
    assert!(
        edges_of(&db, "run").contains(&(s("format"), s("possible"), s("name"), None)),
        "{:?}",
        edges_of(&db, "run")
    );
}

#[test]
fn typescript_with_scip_typescript() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, options) in real_modes(&["node", "scip-typescript"], cache.path()) {
        eprintln!("TypeScript, {mode}");
        let (_dir, db, report) = build("scip-ts", &options);
        check_typescript(&db, &report);
        let (dir, db, report) = build("scip-js", &options);
        check_javascript(&db, &report);
        // The tsconfig of Kairos is removed after the run.
        assert!(!dir.path().join("scip-js/tsconfig.json").exists());
        assert!(!dir.path().join("scip-js/tsconfig.kairos.json").exists());
    }
}

/// KAIROS-T-0669: a JavaScript project with `.mjs` files only, and one whose
/// `tsconfig.json` has no `allowJs`, get SCIP edges. scip-typescript alone
/// stops on both with "no files got indexed". No file of the project changes.
#[test]
fn javascript_that_scip_typescript_alone_does_not_take() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, options) in real_modes(&["node", "scip-typescript"], cache.path()) {
        eprintln!("JavaScript variants, {mode}");
        // `.mjs` files only, with no config file.
        let dir = tempfile::tempdir().unwrap();
        let root = copy("scip-js", dir.path());
        for name in ["lib", "main", "other"] {
            std::fs::rename(
                root.join(format!("{name}.js")),
                root.join(format!("{name}.mjs")),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("main.mjs"),
            "import { helper } from \"./lib.mjs\";\n\nexport function main() {\n  return helper();\n}\n",
        )
        .unwrap();
        let db = dir.path().join("index.sqlite");
        let report = build_structure_with(&root, &db, &options).unwrap();
        assert!(
            report
                .scip_warnings
                .iter()
                .all(|w| w.language != "typescript"),
            "{:?}",
            report.scip_warnings
        );
        assert_eq!(
            edges_of(&db, "main"),
            [(
                s("lib.helper"),
                s("certain"),
                s("scip"),
                Some(s("lib.mjs:helper"))
            )]
        );
        let mut left: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        // A server build runs `npm install`, which writes `package-lock.json`;
        // the runner leaves no tsconfig of its own.
        left.retain(|name| name != "package-lock.json");
        assert_eq!(left, ["lib.mjs", "main.mjs", "other.mjs", "package.json"]);

        // `.js` files and a `tsconfig.json` with no `allowJs`.
        let dir = tempfile::tempdir().unwrap();
        let root = copy("scip-js", dir.path());
        let tsconfig = "{\"compilerOptions\":{\"strict\":true}}\n";
        std::fs::write(root.join("tsconfig.json"), tsconfig).unwrap();
        let db = dir.path().join("index.sqlite");
        let report = build_structure_with(&root, &db, &options).unwrap();
        check_javascript(&db, &report);
        assert_eq!(
            std::fs::read_to_string(root.join("tsconfig.json")).unwrap(),
            tsconfig
        );
        assert!(!root.join("tsconfig.kairos.json").exists());
    }
}

/// KAIROS-T-0476: a server build whose `npm install` fails gets the edges by
/// name and a warning. The dependency is a missing local archive, so npm
/// fails with no network.
#[test]
fn typescript_install_failure_falls_back() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, options) in real_modes(&["node", "scip-typescript"], cache.path()) {
        if mode != "server" {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = copy("scip-ts", dir.path());
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"t","dependencies":{"missing":"file:./does-not-exist.tgz"}}"#,
        )
        .unwrap();
        let db = dir.path().join("index.sqlite");
        let report = build_structure_with(&root, &db, &options).unwrap();
        let warning = report
            .scip_warnings
            .iter()
            .find(|w| w.language == "typescript")
            .unwrap_or_else(|| panic!("{:?}", report.scip_warnings));
        assert!(
            warning.reason.contains("npm install failed"),
            "{}",
            warning.reason
        );
        assert!(edges_of(&db, "run").iter().all(|e| e.2 == "name"));
    }
}

// ---------------------------------------------------------------------------
// Python (KAIROS-T-0477)

fn check_python(db: &Path, report: &BuildReport) {
    assert!(
        report.scip_warnings.iter().all(|w| w.language != "python"),
        "{:?}",
        report.scip_warnings
    );
    // `store` is a Store: the call goes to Store.save, not to Cache.save.
    assert_eq!(
        edges_of(db, "run"),
        [(
            s("Store.save"),
            s("certain"),
            s("scip"),
            Some(s("models.py:Store.save"))
        )]
    );
    let (coverage, uses) = uses_of(db, "Store", "models.py");
    assert_eq!(coverage, kairos_index::Coverage::Full);
    assert_eq!(
        uses,
        [(s("import"), s("app.py"), 1), (s("type"), s("app.py"), 4)]
    );
}

#[test]
fn python_from_the_recorded_index_of_scip_python() {
    let (_dir, db, report) = build(
        "scip-python",
        &recorded("python", &["python"], "python.scip"),
    );
    check_python(&db, &report);
}

#[test]
fn python_with_no_index_has_name_edges() {
    let mut options = quiet();
    options
        .scip
        .runners
        .push(Arc::new(kairos_index::MissingRunner {
            name: "python",
            languages: &["python"],
            reason: "pip install failed".into(),
        }));
    let (_dir, db, report) = build("scip-python", &options);
    assert_eq!(report.scip_warnings[0].reason, "pip install failed");
    assert_eq!(
        edges_of(&db, "run"),
        [(s("store.save"), s("possible"), s("name"), None)]
    );
}

/// The fixture has no dependency file: scip-python reads the sources only.
#[test]
fn python_with_scip_python() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, options) in real_modes(&["node", "scip-python", "python3"], cache.path()) {
        eprintln!("Python, {mode}");
        let (_dir, db, report) = build("scip-python", &options);
        check_python(&db, &report);
    }
}

/// KAIROS-T-0477: a server build whose `pip install` fails gets the edges
/// by name and a warning. The requirement is a missing local wheel, so pip
/// fails with no network.
#[test]
fn python_install_failure_falls_back() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, options) in real_modes(&["node", "scip-python", "python3"], cache.path()) {
        if mode != "server" {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = copy("scip-python", dir.path());
        std::fs::write(
            root.join("requirements.txt"),
            "./does-not-exist-1.0-py3-none-any.whl\n",
        )
        .unwrap();
        let db = dir.path().join("index.sqlite");
        let report = build_structure_with(&root, &db, &options).unwrap();
        let warning = report
            .scip_warnings
            .iter()
            .find(|w| w.language == "python")
            .unwrap_or_else(|| panic!("{:?}", report.scip_warnings));
        assert!(
            warning.reason.contains("pip install failed"),
            "{}",
            warning.reason
        );
        assert_eq!(
            edges_of(&db, "run"),
            [(s("store.save"), s("possible"), s("name"), None)]
        );
    }
}

/// KAIROS-T-0668: a project that asks for another Python and another uv than
/// the pinned ones (`requires-python = "==3.12.*"` and a `required-version`
/// that excludes uv 0.13.1) falls back to edges by name, and says why. With
/// tool downloads, uv fetches CPython 3.12 and `pip` fetches a uv that fits,
/// and the project syncs. Needs the network, for GitHub releases and PyPI.
#[test]
fn python_tool_downloads_fetch_the_python_and_the_uv_the_project_asks_for() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, mut options) in real_modes(&["node", "scip-python", "python3"], cache.path()) {
        if mode != "server" {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = copy("scip-python-uv", dir.path());
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"shop\"\nversion = \"0.1.0\"\nrequires-python = \"==3.12.*\"\ndependencies = [\"helper\"]\n\n[tool.uv]\npackage = false\nrequired-version = \"<0.13\"\n\n[tool.uv.sources]\nhelper = { path = \"wheels/helper-0.1.0-py3-none-any.whl\" }\n",
        )
        .unwrap();

        // Without the setting: edges by name, and the reason names both asks.
        let db = dir.path().join("off.sqlite");
        let report = build_structure_with(&root, &db, &options).unwrap();
        let reason = report
            .scip_warnings
            .iter()
            .find(|w| w.language == "python")
            .map(|w| w.reason.clone())
            .expect("a python warning");
        assert!(
            reason.starts_with(
                "uv sync failed, the project asks for Python ==3.12.*, uv <0.13; the runner has "
            ),
            "{reason}"
        );

        // With the setting: the project syncs.
        options.scip.tools.tool_downloads = true;
        let db = dir.path().join("on.sqlite");
        let report = build_structure_with(&root, &db, &options).unwrap();
        assert!(
            report
                .scip_warnings
                .iter()
                .all(|w| !(w.language == "python" && w.reason.starts_with("uv sync"))),
            "{:?}",
            report.scip_warnings
        );
        let python = cache.path().join("test/repository/python");
        assert!(python.join("uv-required").is_dir(), "pip fetched no uv");
        assert!(python.join("uv-python").is_dir(), "uv fetched no Python");
        let venv = std::fs::read_dir(&python)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("venv-")
            })
            .expect("a venv");
        let config = std::fs::read_to_string(venv.join("pyvenv.cfg")).unwrap();
        assert!(config.contains("version_info = 3.12"), "{config}");
    }
}

/// KAIROS-T-0486: a server build of a project with `uv.lock` installs its
/// dependencies with `uv sync --frozen`: the fixture's dependency is a wheel
/// in the fixture, so the install needs no network. The edges are the
/// same as with no install.
#[test]
fn python_with_uv_lock_installs_with_uv() {
    let cache = tempfile::tempdir().unwrap();
    for (mode, options) in real_modes(&["node", "scip-python", "python3"], cache.path()) {
        if mode != "server" {
            continue;
        }
        let (_dir, db, report) = build("scip-python-uv", &options);
        check_python(&db, &report);
        let python = cache.path().join("test/repository/python");
        let venv = std::fs::read_dir(&python)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("venv-")
            })
            .unwrap_or_else(|| panic!("no venv in {}", python.display()));
        let config = std::fs::read_to_string(venv.join("pyvenv.cfg")).unwrap();
        assert!(
            config.contains("uv = "),
            "uv did not make the venv: {config}"
        );
        let site = std::fs::read_dir(venv.join("lib"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path()
            .join("site-packages");
        assert!(
            site.join("helper/__init__.py").is_file(),
            "helper is not installed"
        );
        assert!(python.join("uv").is_dir(), "uv kept no cache");
    }
}
