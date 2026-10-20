//! The BDD scenarios of the SCIP runners of Go, Python and TypeScript
//! (KAIROS-T-0667, KAIROS-T-0668, KAIROS-T-0669). The `.feature` file is in
//! `tests/scip_features/`.
//!
//! The tools are fake scripts in a tools folder, so the scenarios need no
//! network and no real Go, uv or scip-typescript. They check what the
//! runners do around the tools: the files they leave, the arguments and the
//! variables they give, and the warnings they record. The real indexers are
//! in `scip_languages.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use cucumber::{World, given, then, when};
use kairos_embed::DeterministicProvider;
use kairos_index::languages::{Component, DependencyCache, LanguageTools, Source};
use kairos_index::tools::Download;
use kairos_index::{
    BuildOptions, BuildReport, FakeSummarizer, UpdateOptions, build_structure_with,
    summarize_or_rebuild,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[derive(Debug, Default, World)]
struct ScipWorld {
    /// Holds the repository, the tools and the dependency cache.
    dir: Option<TempDir>,
    /// The tools of the scenario, by name: name and files of the archive.
    components: Vec<(String, Vec<(String, String)>)>,
    /// Whether the server may fetch the Python and the uv of a project.
    tool_downloads: bool,
    /// The report of the last build.
    report: Option<BuildReport>,
    /// The report of the first build of the structure, before a file changed.
    first: Option<BuildReport>,
    /// The file that made the last summary run build the structure again.
    rebuilt: Option<Option<String>>,
}

impl ScipWorld {
    fn dir(&mut self) -> &Path {
        self.dir
            .get_or_insert_with(|| tempfile::tempdir().unwrap())
            .path()
    }

    fn root(&mut self) -> PathBuf {
        let root = self.dir().join("repo");
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn tool(&mut self, name: &str, files: &[(&str, &str)]) {
        self.components.retain(|(n, _)| n != name);
        self.components.push((
            name.to_string(),
            files
                .iter()
                .map(|(p, t)| (p.to_string(), t.to_string()))
                .collect(),
        ));
    }

    /// The fake tools as a server build has them: in a tools folder, with a
    /// dependency cache.
    fn options(&mut self) -> BuildOptions {
        let dir = self.dir().to_path_buf();
        let tools = dir.join("tools");
        fs::create_dir_all(&tools).unwrap();
        let overrides = self
            .components
            .iter()
            .map(|(name, files)| fake_component(&tools, name, files))
            .collect();
        let mut options = BuildOptions {
            download: false,
            ..Default::default()
        };
        options.scip.tools = LanguageTools {
            dir: Some(tools),
            fetch: true,
            overrides,
            cache: Some(DependencyCache {
                root: dir.join("deps"),
                key: "t/r".into(),
                cap_bytes: u64::MAX,
                min_free_bytes: 0,
            }),
            tool_downloads: self.tool_downloads,
            ..Default::default()
        };
        options
    }

    fn build(&mut self) {
        let options = self.options();
        let root = self.root();
        let db = self.dir().join("index.sqlite");
        self.report = Some(build_structure_with(&root, &db, &options).unwrap());
    }

    fn warnings(&self, language: &str) -> Vec<String> {
        self.report
            .as_ref()
            .expect("a build ran")
            .scip_warnings
            .iter()
            .filter(|w| w.language == language)
            .map(|w| w.reason.clone())
            .collect()
    }

    /// The folder of the virtual environment that the Python runner made.
    fn venv(&mut self) -> PathBuf {
        let python = self.dir().join("deps/t/r/python");
        fs::read_dir(python)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("venv-"))
            })
            .expect("a virtual environment")
    }
}

/// A component whose archive holds `files`, in the tools folder.
fn fake_component(dir: &Path, name: &str, files: &[(String, String)]) -> Component {
    let file = format!("{name}-fake.tar.gz");
    let path = dir.join(&file);
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        fs::File::create(&path).unwrap(),
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
    let sha: String = Sha256::digest(fs::read(&path).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let name: &'static str = Box::leak(name.to_string().into_boxed_str());
    Component {
        name,
        version: "0.0.0",
        source: Some(Source::Archive {
            download: Download::new(format!("http://127.0.0.1:1/{file}"), sha),
            strip: 0,
        }),
    }
}

// ---------------------------------------------------------------------------
// Go (KAIROS-T-0667)

/// A `go` that adds a requirement to go.mod and writes go.sum, as
/// `GOFLAGS=-mod=mod` lets it.
const GO_THAT_WRITES: &str = "#!/bin/sh\n\
    if [ \"$1\" = \"mod\" ]; then\n\
      printf 'require example.com/dep v1.0.0\\n' >> go.mod\n\
      printf 'example.com/dep v1.0.0 h1:x\\n' > go.sum\n\
    fi\n";

/// A scip-go that writes an empty index where it is told.
const SCIP_GO: &str = "#!/bin/sh\n\
    while [ $# -gt 0 ]; do\n\
      if [ \"$1\" = \"--output\" ]; then : > \"$2\"; fi\n\
      shift\n\
    done\n";

#[given("a Go project whose go mod download rewrites go.mod and writes go.sum")]
fn go_project(world: &mut ScipWorld) {
    let root = world.root();
    fs::write(root.join("go.mod"), "module example.com/app\n").unwrap();
    fs::write(root.join("main.go"), "package main\n\nfunc main() {}\n").unwrap();
    world.tool("go", &[("bin/go", GO_THAT_WRITES)]);
    world.tool("scip-go", &[("scip-go", SCIP_GO)]);
}

#[when("I build the structure with the server tools")]
fn build_with_server_tools(world: &mut ScipWorld) {
    world.build();
}

#[then("go.mod is as it was and there is no go.sum")]
fn go_files_as_before(world: &mut ScipWorld) {
    let root = world.root();
    assert_eq!(
        fs::read_to_string(root.join("go.mod")).unwrap(),
        "module example.com/app\n"
    );
    assert!(!root.join("go.sum").exists());
}

#[then(expr = "the Go run has the warning {string}")]
fn go_warning(world: &mut ScipWorld, text: String) {
    let warnings = world.warnings("go");
    assert_eq!(warnings, [text], "{warnings:?}");
}

#[given("a Rust file that a step wrote after the build of the structure")]
fn changed_after_build(world: &mut ScipWorld) {
    let root = world.root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn one() -> u32 { 1 }\n").unwrap();
    let db = world.dir().join("index.sqlite");
    world.first = Some(kairos_index::build_structure(&root, &db).unwrap());
    fs::write(
        root.join("src/lib.rs"),
        "pub fn one() -> u32 { 2 }\n\npub fn two() -> u32 { 3 }\n",
    )
    .unwrap();
}

#[when("I run the summaries")]
fn run_summaries(world: &mut ScipWorld) {
    let root = world.root();
    let db = world.dir().join("index.sqlite");
    let first = world.first.clone().expect("a first build");
    let (report, changed) = summarize_or_rebuild(
        &root,
        &db,
        &mut FakeSummarizer::default(),
        &DeterministicProvider::default(),
        &UpdateOptions::default(),
        &first,
    )
    .unwrap();
    world.rebuilt = Some(changed);
    assert!(report.summary.failure.is_none());
    world.report = Some(report.build);
}

#[then(expr = "the structure was built again because of {string}")]
fn rebuilt_for(world: &mut ScipWorld, file: String) {
    assert_eq!(
        world.rebuilt.clone().flatten().as_deref(),
        Some(file.as_str())
    );
}

#[then(expr = "the index has {int} symbols")]
fn index_symbols(world: &mut ScipWorld, count: usize) {
    assert_eq!(world.report.as_ref().unwrap().symbols, count);
}

// ---------------------------------------------------------------------------
// Python (KAIROS-T-0668)

const FAKE_PYTHON: &str = "#!/bin/sh\n\
    if [ \"$1\" = \"-m\" ] && [ \"$2\" = \"venv\" ]; then\n\
      mkdir -p \"$3/bin\"\n\
    fi\n\
    if [ \"$1\" = \"-m\" ] && [ \"$2\" = \"pip\" ] && [ \"$3\" = \"install\" ]; then\n\
      mkdir -p \"$8/bin\"\n\
      echo \"$9\" > \"$8/spec\"\n\
      printf '#!/bin/sh\\nmkdir -p \"$UV_PROJECT_ENVIRONMENT/bin\"\\necho \"$@\" > \"$UV_PROJECT_ENVIRONMENT/uv.args\"\\n' > \"$8/bin/uv\"\n\
      chmod +x \"$8/bin/uv\"\n\
    fi\n";

/// A uv that makes the environment and records how it was called.
const UV: &str = "#!/bin/sh\n\
    mkdir -p \"$UV_PROJECT_ENVIRONMENT/bin\"\n\
    echo \"$@\" > \"$UV_PROJECT_ENVIRONMENT/uv.args\"\n\
    echo \"$UV_PYTHON_DOWNLOADS\" > \"$UV_PROJECT_ENVIRONMENT/uv.downloads\"\n";

/// A uv that the project's `required-version` excludes.
const OLD_UV: &str = "#!/bin/sh\n\
    echo 'error: Required uv version >=0.9.0,<0.13 does not match the running version 0.13.1' >&2\n\
    exit 2\n";

/// A uv whose sync fails.
const FAILING_UV: &str = "#!/bin/sh\necho 'the lock file needs to be updated' >&2\nexit 2\n";

fn python_tools(world: &mut ScipWorld, uv: &str) {
    world.tool("node", &[("bin/node", "#!/bin/sh\nexit 1\n")]);
    world.tool(
        "scip-python",
        &[("node_modules/@sourcegraph/scip-python/index.js", "")],
    );
    world.tool("python", &[("bin/python3", FAKE_PYTHON)]);
    world.tool("uv", &[("uv", uv)]);
}

fn python_project(world: &mut ScipWorld, pyproject: &str, python_version: Option<&str>) {
    let root = world.root();
    fs::write(root.join("app.py"), "def run():\n    return 1\n").unwrap();
    fs::write(root.join("uv.lock"), "").unwrap();
    fs::write(root.join("pyproject.toml"), pyproject).unwrap();
    if let Some(version) = python_version {
        fs::write(root.join(".python-version"), format!("{version}\n")).unwrap();
    }
}

#[given(
    expr = "a Python project with a uv.lock, requires-python {string} and .python-version {string}"
)]
fn python_pinned(world: &mut ScipWorld, requires: String, version: String) {
    python_project(
        world,
        &format!("[project]\nrequires-python = \"{requires}\"\n"),
        Some(&version),
    );
}

#[given(expr = "a Python project with a uv.lock and the uv required-version {string}")]
fn python_required_uv(world: &mut ScipWorld, required: String) {
    python_project(
        world,
        &format!("[tool.uv]\nrequired-version = \"{required}\"\n"),
        None,
    );
}

#[given("a Python project with a uv.lock")]
fn python_plain(world: &mut ScipWorld) {
    python_project(world, "[project]\nname = \"app\"\n", None);
}

#[given("a uv whose sync fails")]
fn failing_uv(world: &mut ScipWorld) {
    python_tools(world, FAILING_UV);
}

#[given("a uv that the required-version excludes")]
fn old_uv(world: &mut ScipWorld) {
    python_tools(world, OLD_UV);
}

#[given("a uv that syncs")]
fn working_uv(world: &mut ScipWorld) {
    python_tools(world, UV);
}

#[given("the server may fetch the tools of a project")]
fn tool_downloads_on(world: &mut ScipWorld) {
    world.tool_downloads = true;
}

#[given("the server fetches no tool of a project")]
fn tool_downloads_off(world: &mut ScipWorld) {
    world.tool_downloads = false;
}

#[then(expr = "the Python warning starts with {string}")]
fn python_warning_starts(world: &mut ScipWorld, start: String) {
    let warnings = world.warnings("python");
    assert!(
        warnings.iter().any(|w| w.starts_with(&start)),
        "{warnings:?}"
    );
}

#[then(expr = "the Python warning ends with the output of uv {string}")]
fn python_warning_has_output(world: &mut ScipWorld, output: String) {
    let warnings = world.warnings("python");
    assert!(warnings.iter().any(|w| w.contains(&output)), "{warnings:?}");
}

#[then(expr = "uv got no {string} argument")]
fn uv_no_argument(world: &mut ScipWorld, argument: String) {
    let args = fs::read_to_string(world.venv().join("uv.args")).unwrap();
    assert!(!args.contains(&argument), "{args}");
}

#[then(expr = "uv got the {string} argument")]
fn uv_argument(world: &mut ScipWorld, argument: String) {
    let args = fs::read_to_string(world.venv().join("uv.args")).unwrap();
    assert!(args.contains(&argument), "{args}");
}

#[then(expr = "uv ran with UV_PYTHON_DOWNLOADS {string}")]
fn uv_downloads(world: &mut ScipWorld, value: String) {
    let text = fs::read_to_string(world.venv().join("uv.downloads")).unwrap();
    assert_eq!(text.trim(), value);
}

#[then(expr = "pip fetched {string} into the dependency cache")]
fn pip_fetched(world: &mut ScipWorld, spec: String) {
    let folder = world.dir().join("deps/t/r/python/uv-required");
    let found: Vec<String> = fs::read_dir(folder)
        .unwrap()
        .flatten()
        .map(|e| {
            fs::read_to_string(e.path().join("spec"))
                .unwrap()
                .trim()
                .to_string()
        })
        .collect();
    assert_eq!(found, [spec]);
}

#[then("the sync ran again with the fetched uv")]
fn synced_with_fetched_uv(world: &mut ScipWorld) {
    // Only the fetched uv writes `uv.args` here; the excluded one fails.
    assert!(world.venv().join("uv.args").is_file());
}

#[then("nothing was fetched")]
fn nothing_fetched(world: &mut ScipWorld) {
    assert!(!world.dir().join("deps/t/r/python/uv-required").exists());
    assert!(!world.dir().join("deps/t/r/python/uv-python").exists());
}

// ---------------------------------------------------------------------------
// TypeScript (KAIROS-T-0669)

/// A node that stands for scip-typescript. It logs each call. It stops with
/// "no files got indexed" unless it gets the tsconfig of Kairos, as the real
/// one does for JavaScript that the tsconfig of the project does not take.
fn typescript_node(log: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"$@\" >> '{log}'\n\
         out=''\n\
         config=''\n\
         while [ $# -gt 0 ]; do\n\
           if [ \"$1\" = \"--output\" ]; then out=\"$2\"; fi\n\
           if [ \"$1\" = \"tsconfig.kairos.json\" ]; then config=\"$1\"; fi\n\
           shift\n\
         done\n\
         if [ -n \"$config\" ] && [ -f \"$config\" ]; then : > \"$out\"; exit 0; fi\n\
         echo 'error: no files got indexed' >&2\n\
         exit 1\n",
        log = log.display()
    )
}

/// A node that finds no file, with any config.
fn empty_typescript_node(log: &Path) -> String {
    format!(
        "#!/bin/sh\necho \"$@\" >> '{log}'\necho 'error: no files got indexed' >&2\nexit 1\n",
        log = log.display()
    )
}

fn typescript_tools(world: &mut ScipWorld, node: String) {
    world.tool("node", &[("bin/node", &node)]);
    world.tool(
        "scip-typescript",
        &[(
            "node_modules/@sourcegraph/scip-typescript/dist/src/main.js",
            "",
        )],
    );
}

fn javascript_files(world: &mut ScipWorld) {
    let root = world.root();
    fs::write(root.join("app.mjs"), "export function run() {}\n").unwrap();
}

#[given("a JavaScript project with .mjs files and no tsconfig.json")]
fn mjs_project(world: &mut ScipWorld) {
    javascript_files(world);
    let log = world.dir().join("node.log");
    typescript_tools(world, typescript_node(&log));
}

#[given("a JavaScript project with a tsconfig.json that has no allowJs")]
fn tsconfig_without_allowjs(world: &mut ScipWorld) {
    javascript_files(world);
    let root = world.root();
    fs::write(root.join("tsconfig.json"), "{\"compilerOptions\":{}}\n").unwrap();
    let log = world.dir().join("node.log");
    typescript_tools(world, typescript_node(&log));
}

#[given("a project with no file that scip-typescript takes")]
fn no_files_project(world: &mut ScipWorld) {
    javascript_files(world);
    let log = world.dir().join("node.log");
    typescript_tools(world, empty_typescript_node(&log));
}

fn node_calls(world: &mut ScipWorld) -> Vec<String> {
    fs::read_to_string(world.dir().join("node.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[then(expr = "scip-typescript ran {int} time(s)")]
fn typescript_runs(world: &mut ScipWorld, count: usize) {
    assert_eq!(node_calls(world).len(), count);
}

#[then("each run used tsconfig.kairos.json and none used --infer-tsconfig")]
fn runs_used_own_config(world: &mut ScipWorld) {
    let calls = node_calls(world);
    assert!(!calls.is_empty());
    for call in calls {
        assert!(call.ends_with("tsconfig.kairos.json"), "{call}");
        assert!(!call.contains("--infer-tsconfig"), "{call}");
    }
}

#[then("the first run used no config and the second used tsconfig.kairos.json")]
fn first_plain_then_own(world: &mut ScipWorld) {
    let calls = node_calls(world);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(!calls[0].contains("tsconfig"), "{calls:?}");
    assert!(calls[1].ends_with("tsconfig.kairos.json"), "{calls:?}");
}

#[then("tsconfig.json is as it was")]
fn tsconfig_as_before(world: &mut ScipWorld) {
    let root = world.root();
    assert_eq!(
        fs::read_to_string(root.join("tsconfig.json")).unwrap(),
        "{\"compilerOptions\":{}}\n"
    );
}

#[then("the project has no tsconfig.kairos.json")]
fn no_own_config_left(world: &mut ScipWorld) {
    let root = world.root();
    assert!(!root.join("tsconfig.kairos.json").exists());
}

#[then("the TypeScript run has no warning")]
fn typescript_no_warning(world: &mut ScipWorld) {
    let warnings = world.warnings("typescript");
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[then(expr = "the TypeScript warning ends with {string}")]
fn typescript_warning_ends(world: &mut ScipWorld, end: String) {
    let warnings = world.warnings("typescript");
    assert!(warnings.iter().any(|w| w.ends_with(&end)), "{warnings:?}");
}

#[tokio::main]
async fn main() {
    ScipWorld::cucumber()
        .fail_on_skipped()
        .run_and_exit(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scip_features"))
        .await;
}
