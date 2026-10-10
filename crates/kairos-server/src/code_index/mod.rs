//! The base code index of each repository (COLLIERY-T-1853, COLLIERY-I-0264
//! "The flow" and "Who builds the base index").
//!
//! # Storage
//!
//! Postgres, in the tenant schema ([`kairos_db::code_indexes`]):
//! - the summary pool of a repository, one row for each summary key, shared
//!   by its commits;
//! - the structure of each indexed commit: the gzip of the SQLite index file
//!   with no summaries ([`kairos_index::store`]).
//!
//! A download assembles an index file from the structure and the pool rows
//! that it uses. About 2 MB of structure for each commit of Kairos.
//!
//! # The clone
//!
//! The nearest indexed commit below a commit, and the tree of a new commit,
//! come from a bare clone of the repository in `KAIROS_CODE_INDEX_DIR`
//! ([`git`]). The clone fetches from the `repo_url` of the repository. When
//! the repository has a read token (COLLIERY-T-3105), the fetch gives it to
//! git through an askpass helper ([`git`]); [`crate::credentials`] reads and
//! decrypts it with `KAIROS_SECRETS_KEY`. With no token, the fetch has no
//! credential, and a remote that asks for one fails at once.
//!
//! # The builder
//!
//! [`sweep`] fetches each repository that has an index. When the head of its
//! default branch has no index, it updates the index of the nearest indexed
//! commit below it to that head ([`kairos_index::update`] with a SCIP run),
//! and stores the result.
//!
//! The order of a pass (KAIROS-T-0334): the repository with the oldest
//! newest index first, over all tenants. The time of the newest index of a
//! repository is the earliest time at which it became stale, so the
//! repository that has been stale longest goes first, whatever the order of
//! the list or of the pushes. A repository that a build just updated goes
//! last in the next pass. So a repository that gets a push during each
//! build of the others does not get 2 builds in a row while another
//! repository is stale. With `KAIROS_CODE_INDEX_MIN_REBUILD_SECS` (off by
//! default), a repository whose newest index is younger than that time gets
//! no update in the pass, and waits for a later pass.
//!
//! The order applies inside the update lane only. It does not change the
//! gate of KAIROS-T-0318: an update still goes ahead of the next step of a
//! first build, and the first-build lane serves the repositories that have
//! no index, in the order of their slugs. A skip by the minimum time does
//! not give the gate to a first build for longer than one pass: the next
//! pass tries the repository again.
//!
//! [`first_builds`] makes the first index of each repository that has none
//! (KAIROS-T-0318): a full build of the head of its default branch. A full
//! build of Kairos takes about 12 hours on a CPU, so it must not stop the
//! updates. [`run_builder`] has two lanes:
//!
//! - the update lane runs a [`sweep`] on an interval, as the embedding
//!   refresher does, and then wakes the first-build lane;
//! - the first-build lane runs [`first_builds`], one repository at a time.
//!   The update lane never waits for it.
//!
//! The two lanes share the CPU through [`WorkGate`]: one heavy step at a
//! time, and an update goes first. An update holds the gate for its whole
//! build; a first build takes it for its structure step and then for each
//! summary call and each embedding batch. So an update waits for one step
//! of a first build at most, and only one model run uses the
//! `KAIROS_CODE_INDEX_THREADS` threads at a time.
//!
//! Both lanes keep their summaries across a restart (KAIROS-T-0335). The
//! work database of a first build is in `first-builds/` of
//! `KAIROS_CODE_INDEX_DIR`, and the work database of an update is in
//! `updates/`. Each summary is written to it when it is made. After a
//! restart, the next build of the repository uses those summaries again:
//! a first build continues in its work database, and an update copies the
//! summaries of the old work database into the new one before it starts.
//! The keys of a summary name its input, so a summary of a build that did
//! not end is right for each later build that needs its key. An index
//! stores only the summaries that its structure uses, so a summary that no
//! later index needs never reaches the pool of the repository. The run
//! row counts the summaries that a run reused. The builder removes a work
//! database after it stores the index. A repository with
//! `code_index_build = off` gets no build.
//!
//! # The runs
//!
//! Each run has a row ([`kairos_db::code_index_builds`]) with the start of
//! its server: a random id that [`CodeIndexService`] makes (KAIROS-T-0354).
//! One run of a repository at a time: a lane does not start a run on a
//! repository that has a `running` run of this server (a requested build
//! that waits for its turn, or a first build), and says so in its note. A
//! `running` run of another id is of a server that stopped: the builder
//! ends it as failed when it starts ([`end_stale_runs`]), and before the
//! next run of the repository.
//!
//! # The tools
//!
//! The image does not hold the tools of the builder (COLLIERY-T-2525). At
//! each start, [`prepare_tools`] checks them in `tools/` of
//! `KAIROS_CODE_INDEX_DIR`, and downloads the ones that are not there
//! ([`kairos_index::tools`]). Without `KAIROS_CODE_INDEX_DIR`, nothing is
//! downloaded.

pub mod git;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use kairos_db::code_index_builds as runs;
use kairos_db::models::repositories::Repository;
use kairos_index::tools::{ToolSet, Tools};
use kairos_index::{
    IndexError, SummarizeOptions, Summarizer, SummaryRequest, UpdateOptions, UpdateReport,
};

use crate::blocking::BlockingTenantPool;
use crate::error::ApiError;
use crate::secrets::SecretsKey;

/// The most commits that a search for the nearest indexed commit reads
/// below the given commit.
pub const MAX_DISTANCE: usize = 1000;

/// The URL to fetch a repository from.
pub type RemoteOf = Arc<dyn Fn(&Repository) -> String + Send + Sync>;

/// The clones of the indexed repositories, and the work folders of the
/// builder.
pub struct CodeIndexService {
    dir: PathBuf,
    remote_of: RemoteOf,
    /// One lock for each clone: 2 fetches into one clone at the same time
    /// can fail.
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// The commits that a build could not index, so that each sweep does
    /// not try them again. Held here, not in the database: a restart tries
    /// them one more time.
    failed: Mutex<HashSet<(uuid::Uuid, String)>>,
    /// The SCIP options of a build: the tools of [`prepare_tools`], when
    /// they are ready.
    build: OnceLock<kairos_index::BuildOptions>,
    /// `KAIROS_SECRETS_KEY`, to decrypt the read token of a repository
    /// (COLLIERY-T-3105).
    secrets: Option<SecretsKey>,
    /// One heavy step at a time for the two lanes of the builder
    /// (KAIROS-T-0318).
    gate: WorkGate,
    /// The last reason why the first-build lane did not build each
    /// repository, so that the log gets a reason once, not at each pass.
    notes: Mutex<HashMap<uuid::Uuid, String>>,
    /// The builds that a person asked for (KAIROS-T-0332), in the sequence
    /// of the requests. The builder takes them when `wake` says so.
    requests: Mutex<std::collections::VecDeque<RebuildRequest>>,
    /// Woken for each request.
    wake: tokio::sync::Notify,
    /// The start of this server (KAIROS-T-0354): a random id, recorded on
    /// each run. A `running` run of another id is of a server that stopped.
    instance: uuid::Uuid,
    /// `KAIROS_CODE_INDEX_MIN_REBUILD_SECS` (KAIROS-T-0334): the least time
    /// between 2 updates of one repository. 0: off.
    min_rebuild_secs: std::sync::atomic::AtomicU64,
}

/// A build that a person asked for (KAIROS-T-0332). Its run row exists
/// already, as `running` with `trigger = request`.
#[derive(Debug, Clone)]
pub struct RebuildRequest {
    pub tenant: String,
    pub repository_id: uuid::Uuid,
    /// The id of the run row.
    pub run: uuid::Uuid,
}

impl std::fmt::Debug for CodeIndexService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeIndexService")
            .field("dir", &self.dir)
            .finish()
    }
}

impl CodeIndexService {
    /// A service with its clones in `dir`, which fetches each repository
    /// from the URL that `remote_of` gives.
    pub fn new(dir: PathBuf, remote_of: RemoteOf) -> Self {
        CodeIndexService {
            dir,
            remote_of,
            locks: Mutex::default(),
            failed: Mutex::default(),
            build: OnceLock::new(),
            secrets: None,
            gate: WorkGate::default(),
            notes: Mutex::default(),
            requests: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            instance: uuid::Uuid::new_v4(),
            min_rebuild_secs: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The least time between 2 updates of one repository
    /// (`KAIROS_CODE_INDEX_MIN_REBUILD_SECS`, KAIROS-T-0334). 0: off.
    pub fn with_min_rebuild_secs(self, secs: u64) -> Self {
        self.set_min_rebuild_secs(secs);
        self
    }

    /// Change the time of [`Self::with_min_rebuild_secs`].
    pub fn set_min_rebuild_secs(&self, secs: u64) {
        self.min_rebuild_secs
            .store(secs, std::sync::atomic::Ordering::Relaxed);
    }

    fn min_rebuild(&self) -> Option<Duration> {
        match self
            .min_rebuild_secs
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => None,
            secs => Some(Duration::from_secs(secs)),
        }
    }

    /// The start of this server, as the runs record it (KAIROS-T-0354).
    pub fn instance(&self) -> uuid::Uuid {
        self.instance
    }

    /// Queue a build that a person asked for, and wake the builder.
    pub fn request_rebuild(&self, request: RebuildRequest) {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
        self.wake.notify_one();
    }

    /// Take each queued request, in the sequence of the requests.
    pub fn pending_rebuilds(&self) -> Vec<RebuildRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// Woken by [`Self::request_rebuild`].
    pub fn wake(&self) -> &tokio::sync::Notify {
        &self.wake
    }

    /// The gate of the heavy steps of the builder (KAIROS-T-0318).
    pub fn gate(&self) -> &WorkGate {
        &self.gate
    }

    /// The work database of the first build of `repo`. It stays after a
    /// failure or a restart, so that the next build reuses its summaries.
    fn first_build_db(&self, tenant: &str, repo: &Repository) -> PathBuf {
        self.dir
            .join("first-builds")
            .join(tenant)
            .join(format!("{}.db", repo.id))
    }

    /// The work database of the update of `repo` (KAIROS-T-0335). It stays
    /// after a failure or a restart, so that the next update reuses its
    /// summaries.
    fn update_db(&self, tenant: &str, repo: &Repository) -> PathBuf {
        self.dir
            .join("updates")
            .join(tenant)
            .join(format!("{}.db", repo.id))
    }

    /// Record `note` as the last reason for `repo`. True when it is not the
    /// reason that was recorded before.
    fn note_changed(&self, repo: uuid::Uuid, note: &str) -> bool {
        let mut notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        if notes.get(&repo).map(String::as_str) == Some(note) {
            return false;
        }
        notes.insert(repo, note.to_string());
        true
    }

    fn forget_note(&self, repo: uuid::Uuid) {
        self.notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&repo);
    }

    /// Decrypt the read token of each repository with `key`
    /// (`KAIROS_SECRETS_KEY`, COLLIERY-T-3105). With no key, a repository
    /// that has a token fails to fetch with a text that names the setting.
    pub fn with_secrets_key(mut self, key: Option<SecretsKey>) -> Self {
        self.secrets = key;
        self
    }

    /// The key of [`Self::with_secrets_key`].
    pub fn secrets_key(&self) -> Option<&SecretsKey> {
        self.secrets.as_ref()
    }

    /// The URL to fetch `repo` from.
    pub fn remote_of(&self, repo: &Repository) -> String {
        (self.remote_of)(repo)
    }

    /// The folder of the clones and of the tools.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Use `tools` for the SCIP run of each build. Only the first call
    /// counts.
    pub fn use_tools(&self, tools: &Tools) {
        let _ = self.build.set(tools.build_options());
    }

    /// The SCIP options of a build: the tools, else the pinned files at
    /// their default paths. Never a download.
    pub fn build_options(&self) -> kairos_index::BuildOptions {
        self.build
            .get()
            .cloned()
            .unwrap_or_else(|| kairos_index::BuildOptions {
                download: false,
                ..Default::default()
            })
    }

    /// A service that fetches each repository from its `repo_url`.
    pub fn from_repo_url(dir: PathBuf) -> Self {
        Self::new(dir, Arc::new(|repo: &Repository| repo.repo_url.clone()))
    }

    /// The folder of the bare clone of `repo`.
    fn clone_dir(&self, tenant: &str, repo: &Repository) -> PathBuf {
        self.dir
            .join("clones")
            .join(tenant)
            .join(format!("{}.git", repo.id))
    }

    fn lock(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(locks.entry(path.to_path_buf()).or_default())
    }

    /// Fetch `repo` into its clone, with its read token when it has one, and
    /// give the folder of the clone.
    pub fn fetch(
        &self,
        tenant: &str,
        repo: &Repository,
        token: Option<&git::GitToken>,
    ) -> Result<PathBuf, git::GitError> {
        let path = self.clone_dir(tenant, repo);
        let lock = self.lock(&path);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        git::fetch(&path, &(self.remote_of)(repo), token)?;
        Ok(path)
    }

    /// The commits at and below `commit`, nearest first, from the clone of
    /// `repo`. The clone fetches when it does not have the commit. `None`:
    /// the repository does not have the commit.
    pub(crate) fn ancestors(
        &self,
        tenant: &str,
        repo: &Repository,
        commit: &str,
        token: Option<&git::GitToken>,
    ) -> Result<Option<Vec<String>>, git::GitError> {
        let mut path = self.clone_dir(tenant, repo);
        if !git::has_commit(&path, commit) {
            path = self.fetch(tenant, repo, token)?;
            if !git::has_commit(&path, commit) {
                return Ok(None);
            }
        }
        git::ancestors(&path, commit, MAX_DISTANCE).map(Some)
    }
}

/// Gives the builder a summarizer.
pub trait SummarizerSource: Send + Sync {
    /// Run `job` with a summarizer.
    fn run(
        &self,
        job: &mut dyn FnMut(&mut dyn Summarizer) -> Result<UpdateReport, IndexError>,
    ) -> Result<UpdateReport, IndexError>;

    /// The model of the summaries, as `provider/model` (KAIROS-T-0341),
    /// for the record of a run.
    fn model_name(&self) -> String;
}

/// A hosted summarizer source (KAIROS-T-0341): an OpenAI-compatible chat
/// endpoint, from the settings of a tenant. A new client for each build.
pub struct HostedSummarizers {
    pub config: kairos_index::hosted::HostedConfig,
}

impl SummarizerSource for HostedSummarizers {
    fn run(
        &self,
        job: &mut dyn FnMut(&mut dyn Summarizer) -> Result<UpdateReport, IndexError>,
    ) -> Result<UpdateReport, IndexError> {
        let mut summarizer = kairos_index::hosted::HostedChat::new(self.config.clone())
            .map_err(IndexError::Model)?;
        job(&mut summarizer)
    }

    fn model_name(&self) -> String {
        format!("{}/{}", self.config.provider, self.config.model)
    }
}

/// The Bedrock summarizer source (KAIROS-T-0342), from the settings of a
/// tenant. A new client for each build.
pub struct BedrockSummarizers {
    pub config: kairos_index::bedrock::BedrockConfig,
}

impl SummarizerSource for BedrockSummarizers {
    fn run(
        &self,
        job: &mut dyn FnMut(&mut dyn Summarizer) -> Result<UpdateReport, IndexError>,
    ) -> Result<UpdateReport, IndexError> {
        let mut summarizer =
            kairos_index::bedrock::Bedrock::new(self.config.clone()).map_err(IndexError::Model)?;
        job(&mut summarizer)
    }

    fn model_name(&self) -> String {
        format!("bedrock/{}", self.config.model_id)
    }
}

/// The fake summarizer of `kairos-index`: a fixed text for each input, with
/// no model. For tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeSummarizers;

impl SummarizerSource for FakeSummarizers {
    fn run(
        &self,
        job: &mut dyn FnMut(&mut dyn Summarizer) -> Result<UpdateReport, IndexError>,
    ) -> Result<UpdateReport, IndexError> {
        job(&mut kairos_index::FakeSummarizer::default())
    }

    fn model_name(&self) -> String {
        kairos_index::FakeSummarizer::default().model()
    }
}

/// One heavy step of the builder at a time, and an update goes first
/// (KAIROS-T-0318).
///
/// The update lane and the first-build lane of [`run_builder`] run at the
/// same time. Without the gate, a first build and an update would each run
/// the model on `KAIROS_CODE_INDEX_THREADS` threads, or 2 SCIP runs, at the
/// same time. An update holds the gate for its whole build. A first build
/// takes it for each short step, and waits while an update holds it or
/// waits for it.
///
/// The gate blocks the thread: use it only off the async threads.
#[derive(Debug, Default)]
pub struct WorkGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct GateState {
    busy: bool,
    updates_waiting: usize,
}

/// The gate is held until this is dropped.
#[derive(Debug)]
pub struct GateHeld<'g>(&'g WorkGate);

impl Drop for GateHeld<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.busy = false;
        self.0.changed.notify_all();
    }
}

impl WorkGate {
    /// Hold the gate for an update. It waits only for the step that holds
    /// the gate now.
    pub fn update(&self) -> GateHeld<'_> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.updates_waiting += 1;
        while state.busy {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.updates_waiting -= 1;
        state.busy = true;
        GateHeld(self)
    }

    /// Hold the gate for one step of a first build. It waits while an
    /// update holds the gate or waits for it.
    pub fn first_build(&self) -> GateHeld<'_> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.busy || state.updates_waiting > 0 {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.busy = true;
        GateHeld(self)
    }
}

/// A summarizer that holds the gate for each call, for a first build.
struct GatedSummarizer<'a> {
    inner: &'a mut dyn Summarizer,
    /// None for a hosted provider (KAIROS-T-0341): the calls pass through.
    gate: Option<&'a WorkGate>,
}

impl Summarizer for GatedSummarizer<'_> {
    fn model(&self) -> String {
        self.inner.model()
    }

    fn summarize(&mut self, request: &SummaryRequest) -> Result<String, String> {
        let _held = self.gate.map(|gate| gate.first_build());
        self.inner.summarize(request)
    }

    fn concurrency(&self) -> usize {
        self.inner.concurrency()
    }

    fn summarize_many(&mut self, requests: &[SummaryRequest]) -> Vec<Result<String, String>> {
        let _held = self.gate.map(|gate| gate.first_build());
        self.inner.summarize_many(requests)
    }
}

/// An embedder that holds the gate for each batch, for a first build.
struct GatedEmbedder<'a> {
    inner: &'a dyn kairos_embed::EmbeddingProvider,
    /// None for a remote endpoint: the calls pass through.
    gate: Option<&'a WorkGate>,
}

impl kairos_embed::EmbeddingProvider for GatedEmbedder<'_> {
    fn model_id(&self) -> &kairos_embed::ModelId {
        self.inner.model_id()
    }

    fn embed(
        &self,
        texts: &[String],
    ) -> Result<Vec<kairos_embed::Embedding>, kairos_embed::EmbedError> {
        let _held = self.gate.map(|gate| gate.first_build());
        self.inner.embed(texts)
    }
}

/// Qwen3-4B through llama.cpp, from the model file at `model`, on `threads`
/// CPU threads (`KAIROS_CODE_INDEX_THREADS`). The model is loaded for a
/// build and freed when no build uses it, so the server does not keep about
/// 3 GB of memory between pushes. When the 2 lanes of the builder run at
/// the same time, they share one loaded model, each with its own context
/// (KAIROS-T-0318).
#[cfg(feature = "llama")]
#[derive(Debug, Clone)]
pub struct LlamaSummarizers {
    pub model: PathBuf,
    pub threads: u32,
    loaded: Arc<Mutex<std::sync::Weak<kairos_index::LlamaModelFile>>>,
}

#[cfg(feature = "llama")]
impl LlamaSummarizers {
    /// The summarizers of the model file at `model`, on `threads` threads.
    pub fn new(model: PathBuf, threads: u32) -> Self {
        LlamaSummarizers {
            model,
            threads,
            loaded: Arc::default(),
        }
    }

    /// The loaded model: the one that a build uses now, else a new load.
    fn model(&self) -> Result<Arc<kairos_index::LlamaModelFile>, IndexError> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(model) = loaded.upgrade() {
            return Ok(model);
        }
        let model = Arc::new(kairos_index::LlamaModelFile::load(&self.model)?);
        *loaded = Arc::downgrade(&model);
        Ok(model)
    }
}

#[cfg(feature = "llama")]
impl SummarizerSource for LlamaSummarizers {
    fn run(
        &self,
        job: &mut dyn FnMut(&mut dyn Summarizer) -> Result<UpdateReport, IndexError>,
    ) -> Result<UpdateReport, IndexError> {
        let model = self.model()?;
        let mut summarizer = model.summarizer_with_threads(self.threads)?;
        job(&mut summarizer)
    }

    fn model_name(&self) -> String {
        format!(
            "embedded/{}",
            self.model
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "model".to_string())
        )
    }
}

/// Why a build of the server without the `llama` feature has no builder.
const NO_SUMMARIZER: &str = "this build of the server has no summarizer (the feature llama)";

/// Whether this build of the server has a summarizer: the `llama` feature.
/// The reason when it has none.
pub fn has_summarizer() -> Result<(), String> {
    if cfg!(feature = "llama") {
        Ok(())
    } else {
        Err(NO_SUMMARIZER.to_string())
    }
}

/// The summarizer of this build of the server, with the model file of
/// `tools`, on `threads` CPU threads. The reason when the build has none.
pub fn summarizers(tools: &Tools, threads: u32) -> Result<Arc<dyn SummarizerSource>, String> {
    #[cfg(feature = "llama")]
    {
        if tools.model.is_none() {
            return Err(NO_SUMMARIZER.to_string());
        }
        Ok(Arc::new(llama_summarizers(tools, threads)))
    }
    #[cfg(not(feature = "llama"))]
    {
        let _ = (tools, threads);
        Err(NO_SUMMARIZER.to_string())
    }
}

#[cfg(feature = "llama")]
fn llama_summarizers(tools: &Tools, threads: u32) -> LlamaSummarizers {
    LlamaSummarizers::new(
        tools
            .model
            .clone()
            .expect("the caller checked that the set has the model"),
        threads,
    )
}

/// The folder of the tools in `KAIROS_CODE_INDEX_DIR`.
pub const TOOLS_DIR: &str = "tools";

/// Check the tools of the builder in `tools/` of `code_index_dir`, and
/// download each one that is not there (COLLIERY-T-2525). `Ok(None)`: the
/// deployment has no `KAIROS_CODE_INDEX_DIR`, so nothing is checked or
/// downloaded. The error names the file or the URL that failed.
pub fn prepare_tools(
    code_index_dir: Option<&Path>,
    set: &ToolSet,
) -> Result<Option<Tools>, String> {
    let Some(dir) = code_index_dir else {
        return Ok(None);
    };
    let tools = kairos_index::tools::prepare(&dir.join(TOOLS_DIR), set, &mut |d| {
        tracing::info!(file = %d.file, url = %d.url, "the code index builder downloads a tool");
    })
    .map_err(|e| e.to_string())?;
    Ok(Some(tools))
}

/// What a sweep did for one repository.
#[derive(Debug)]
pub struct BuildOutcome {
    pub tenant: String,
    /// The slug of the repository.
    pub repository: String,
    /// The head of the default branch, if the clone has the branch.
    pub commit: String,
    /// The indexed commit that the build started from. `None` for a first
    /// build.
    pub base: Option<String>,
    /// The report of the build, when the sweep built an index.
    pub report: Option<UpdateReport>,
    /// Why the sweep built no index, when it did not.
    pub note: Option<String>,
    pub elapsed: Duration,
}

/// The note of a repository with `code_index_build = off`.
pub const OPTED_OUT: &str = "the repository has code_index_build off";

/// The note of an `embedded` repository on a server with no embedded model
/// (KAIROS-T-0341, KAIROS-T-0344).
pub const NO_EMBEDDED_MODEL: &str = "failed: This image has no embedded model. Set a hosted \
                                      provider for the organization and put the repository on \
                                      it (the default summarizer hosted, or the repository \
                                      set to hosted), or run the image with the model.";

/// What a build uses for its summaries and its vectors (KAIROS-T-0341),
/// chosen from the settings of the tenant and the opt-in of the repository.
pub(crate) struct Models {
    pub summarizers: Arc<dyn SummarizerSource>,
    pub embedder: Arc<dyn kairos_embed::EmbeddingProvider>,
    /// The summaries come from a hosted provider: the summarizer does not
    /// take the CPU gate of the builder.
    pub hosted_summaries: bool,
    /// The vectors come from a remote endpoint: the embedder does not take
    /// the CPU gate.
    pub hosted_vectors: bool,
}

/// Choose the models of a build (KAIROS-T-0341). `embedded` is the
/// summarizer of the server, when the image has one. The error is the note
/// of a failed run.
async fn models_for(
    blocking: &BlockingTenantPool,
    service: &Arc<CodeIndexService>,
    tenant: &str,
    repo: &Repository,
    embedded: &Option<Arc<dyn SummarizerSource>>,
    embedder: &Arc<dyn kairos_embed::EmbeddingProvider>,
) -> Result<Models, String> {
    let settings = blocking
        .run(tenant, |conn| {
            kairos_db::code_index_settings::load_or_default(conn).map_err(ApiError::internal)
        })
        .await
        .map_err(|e| format!("failed: the settings of the organization are not readable: {e:?}"))?;
    let open = |field: &str, sealed: Option<kairos_db::code_index_settings::SealedSecret>| {
        open_secret(service.secrets_key(), tenant, field, sealed)
            .map_err(|why| format!("failed: {why}"))
    };

    let hosted_summaries = hosted_summaries(repo, &settings);
    let summarizers: Arc<dyn SummarizerSource> = if hosted_summaries {
        let secret = open("summary.secret", settings.summary_secret())?;
        match settings.summary_provider.as_str() {
            "ollama-cloud" => {
                let (Some(base_url), Some(model)) = (
                    settings.summary_base_url.clone(),
                    settings.summary_model.clone(),
                ) else {
                    return Err(
                        "failed: the provider ollama-cloud of the organization has no \
                                base URL or no model"
                            .to_string(),
                    );
                };
                let mut config = kairos_index::hosted::HostedConfig::new(
                    "ollama-cloud",
                    &base_url,
                    &model,
                    settings.concurrency.max(1) as usize,
                );
                config.api_key = secret;
                Arc::new(HostedSummarizers { config })
            }
            "bedrock" => {
                let (Some(region), Some(model_id)) = (
                    settings.summary_region.clone(),
                    settings.summary_model.clone(),
                ) else {
                    return Err(
                        "failed: the provider bedrock of the organization has no region \
                                or no model id"
                            .to_string(),
                    );
                };
                let secret = secret.ok_or_else(|| {
                    "failed: the provider bedrock of the organization has no credentials"
                        .to_string()
                })?;
                let mut parts = secret.splitn(3, ':');
                let (Some(access_key_id), Some(secret_access_key)) = (parts.next(), parts.next())
                else {
                    return Err("failed: the credentials of bedrock are not <access key \
                                id>:<secret access key>"
                        .to_string());
                };
                let session_token = parts.next().filter(|t| !t.is_empty()).map(str::to_string);
                // A region that is a URL is an endpoint override, for a test
                // server; the signature then names us-east-1.
                let (region, endpoint) =
                    if region.starts_with("http://") || region.starts_with("https://") {
                        ("us-east-1".to_string(), Some(region))
                    } else {
                        (region, None)
                    };
                let mut config = kairos_index::bedrock::BedrockConfig::new(
                    &region,
                    &model_id,
                    access_key_id,
                    secret_access_key,
                    settings.concurrency.max(1) as usize,
                );
                config.session_token = session_token;
                config.endpoint = endpoint;
                Arc::new(BedrockSummarizers { config })
            }
            other => {
                return Err(format!(
                    "failed: the provider {other:?} of the organization is not available in \
                     this version of Kairos"
                ));
            }
        }
    } else {
        embedded
            .clone()
            .ok_or_else(|| NO_EMBEDDED_MODEL.to_string())?
    };

    let hosted_vectors = settings.vector_provider == "remote";
    let embedder: Arc<dyn kairos_embed::EmbeddingProvider> = if hosted_vectors {
        let (Some(base_url), Some(model)) = (
            settings.vector_base_url.clone(),
            settings.vector_model.clone(),
        ) else {
            return Err(
                "failed: the vector provider remote of the organization has no base URL \
                        or no model"
                    .to_string(),
            );
        };
        let api_key = open("vectors.secret", settings.vector_secret())?;
        let mut config = kairos_embed::remote::RemoteConfig::new(base_url, model);
        config.api_key = api_key;
        // The provider probes the endpoint when it is made: off the async
        // threads.
        let provider =
            tokio::task::spawn_blocking(move || kairos_embed::remote::RemoteProvider::new(config))
                .await
                .map_err(|e| format!("failed: {e}"))?
                .map_err(|e| format!("failed: the vector endpoint of the organization: {e}"))?;
        Arc::new(provider)
    } else {
        Arc::clone(embedder)
    };
    Ok(Models {
        summarizers,
        embedder,
        hosted_summaries,
        hosted_vectors,
    })
}

/// Open a sealed secret of the code index settings of `tenant`
/// (`summary.secret` or `vectors.secret`). `None` when none is stored. The
/// error says why the secret does not open.
pub(crate) fn open_secret(
    key: Option<&SecretsKey>,
    tenant: &str,
    field: &str,
    sealed: Option<kairos_db::code_index_settings::SealedSecret>,
) -> Result<Option<String>, String> {
    let Some(sealed) = sealed else {
        return Ok(None);
    };
    let key = key.ok_or_else(|| {
        "the organization has a hosted provider with a secret, and this deployment has no \
         KAIROS_SECRETS_KEY to open it"
            .to_string()
    })?;
    let aad = crate::api::org::code_index_settings::settings_aad(tenant, field);
    let plain = key
        .open(&aad, &sealed.key_id, &sealed.nonce, &sealed.ciphertext)
        .map_err(|e| format!("the secret {field} of the organization does not open: {e:?}"))?;
    String::from_utf8(plain)
        .map(Some)
        .map_err(|_| format!("the secret {field} is not text"))
}

/// The input of a build: the base, read from the database.
struct Base {
    commit: String,
    structure: Vec<u8>,
    pool: Vec<kairos_db::code_indexes::PoolRow>,
    /// The time of the newest index of the repository (KAIROS-T-0334).
    newest: Option<chrono::DateTime<chrono::Utc>>,
}

/// Where an update of a repository starts (KAIROS-T-0347).
enum Start {
    /// The head has an index: nothing to do.
    HeadIndexed,
    /// The nearest indexed commit below the head.
    Base(Base),
    /// The nearest index (the head's or another) is of another schema
    /// version: an update cannot read it, so the repository gets a full
    /// build.
    OldSchema { commit: String, found: i64 },
}

/// Which repositories a pass of the builder reads.
#[derive(Debug, Clone, Copy)]
enum Pass {
    /// The repositories that have an index.
    Updates,
    /// The repositories that have no index.
    FirstBuilds,
}

/// The repositories of each tenant for `pass`, by tenant. An error is
/// logged, and the tenant is not in the result.
async fn repositories_of(
    blocking: &BlockingTenantPool,
    pass: Pass,
) -> Vec<(String, Vec<Repository>)> {
    let tenants = match blocking
        .run_public(|conn| kairos_db::list_tenants(conn).map_err(ApiError::internal))
        .await
    {
        Ok(tenants) => tenants,
        Err(e) => {
            tracing::warn!(error = ?e, "code index builder could not list tenants");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for tenant in tenants.into_iter().filter(|t| t.schema_exists) {
        let repos = blocking
            .run(&tenant.slug, move |conn| {
                let ids = match pass {
                    Pass::Updates => kairos_db::code_indexes::indexed_repositories(conn),
                    Pass::FirstBuilds => kairos_db::code_indexes::unindexed_repositories(conn),
                }
                .map_err(ApiError::internal)?;
                ids.into_iter()
                    .map(|id| kairos_db::repositories::load(conn, id).map_err(ApiError::internal))
                    .collect::<Result<Vec<_>, _>>()
            })
            .await;
        match repos {
            Ok(repos) => out.push((tenant.slug, repos)),
            Err(e) => {
                tracing::warn!(tenant = %tenant.slug, error = ?e, "code index builder could not list repositories");
            }
        }
    }
    out
}

/// The repositories of a pass of the update lane, over all tenants, the
/// repository with the oldest newest index first (KAIROS-T-0334). A tie
/// keeps the order of the tenants and of the slugs.
async fn update_order(blocking: &BlockingTenantPool) -> Vec<(String, Repository)> {
    let mut order = Vec::new();
    for (tenant, repos) in repositories_of(blocking, Pass::Updates).await {
        let newest: HashMap<uuid::Uuid, chrono::DateTime<chrono::Utc>> = blocking
            .run(&tenant, |conn| {
                kairos_db::code_indexes::newest_writes(conn).map_err(ApiError::internal)
            })
            .await
            .map(|rows| rows.into_iter().collect())
            .unwrap_or_else(|e| {
                tracing::warn!(tenant = %tenant, error = ?e, "code index builder could not read the times of the indexes");
                HashMap::new()
            });
        for repo in repos {
            let at = newest.get(&repo.id).copied();
            order.push((at, tenant.clone(), repo));
        }
    }
    // No time sorts first: such a repository is not known to be fresh.
    order.sort_by_key(|(at, _, _)| *at);
    order
        .into_iter()
        .map(|(_, tenant, repo)| (tenant, repo))
        .collect()
}

/// Fetch each repository that has an index, and update the index of the
/// head of its default branch when it has none. One repository at a time,
/// the repository with the oldest newest index first (KAIROS-T-0334). A
/// repository with `code_index_build = off` is not fetched.
pub async fn sweep(
    blocking: &BlockingTenantPool,
    service: &Arc<CodeIndexService>,
    summarizers: Option<Arc<dyn SummarizerSource>>,
    embedder: Arc<dyn kairos_embed::EmbeddingProvider>,
) -> Vec<BuildOutcome> {
    let mut outcomes = Vec::new();
    for (tenant, repo) in update_order(blocking).await {
        let outcome = build_repository(
            blocking,
            service,
            &tenant,
            repo,
            summarizers.clone(),
            Arc::clone(&embedder),
        )
        .await;
        match (&outcome.report, &outcome.note) {
            (Some(report), _) => tracing::info!(
                tenant = %outcome.tenant,
                repository = %outcome.repository,
                commit = %outcome.commit,
                base = ?outcome.base,
                symbols = report.summary.symbols.summarized,
                files = report.summary.files.summarized,
                modules = report.summary.modules.summarized,
                elapsed_secs = outcome.elapsed.as_secs_f64(),
                "code index built"
            ),
            (None, Some(note)) if note.starts_with("failed") => tracing::warn!(
                tenant = %outcome.tenant,
                repository = %outcome.repository,
                commit = %outcome.commit,
                note = %note,
                "code index not built"
            ),
            (None, Some(note)) if note.starts_with(ACTIVE_RUN) => tracing::info!(
                tenant = %outcome.tenant,
                repository = %outcome.repository,
                note = %note,
                "code index update not started"
            ),
            (None, Some(note)) if note.starts_with(MIN_REBUILD) => tracing::debug!(
                tenant = %outcome.tenant,
                repository = %outcome.repository,
                note = %note,
                "code index update waits"
            ),
            _ => {}
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// Make the first index of each repository that has no index
/// (KAIROS-T-0318): a full build of the head of its default branch, with
/// the SCIP edges and the summaries. One repository at a time. A
/// repository with `code_index_build = off` gets no build.
///
/// The log has the start, the end and the time of each build, and the
/// reason when a repository gets no build. A reason is logged once, until
/// it changes.
pub async fn first_builds(
    blocking: &BlockingTenantPool,
    service: &Arc<CodeIndexService>,
    summarizers: Option<Arc<dyn SummarizerSource>>,
    embedder: Arc<dyn kairos_embed::EmbeddingProvider>,
) -> Vec<BuildOutcome> {
    let mut outcomes = Vec::new();
    for (tenant, repos) in repositories_of(blocking, Pass::FirstBuilds).await {
        for repo in repos {
            let repo_id = repo.id;
            let outcome = full_build_repository(
                blocking,
                service,
                &tenant,
                repo,
                summarizers.clone(),
                Arc::clone(&embedder),
                "first",
                None,
            )
            .await;
            match (&outcome.report, &outcome.note) {
                (Some(report), _) => {
                    service.forget_note(repo_id);
                    tracing::info!(
                        tenant = %outcome.tenant,
                        repository = %outcome.repository,
                        commit = %outcome.commit,
                        files = report.build.files,
                        symbols = report.build.symbols,
                        summaries = report.summary.symbols.summarized
                            + report.summary.files.summarized
                            + report.summary.modules.summarized,
                        reused = report.summary.symbols.reused
                            + report.summary.files.reused
                            + report.summary.modules.reused,
                        elapsed_secs = outcome.elapsed.as_secs_f64(),
                        "code index first build ended"
                    );
                }
                (None, Some(note)) if note.starts_with("failed") => {
                    service.note_changed(repo_id, note);
                    tracing::warn!(
                        tenant = %outcome.tenant,
                        repository = %outcome.repository,
                        commit = %outcome.commit,
                        note = %note,
                        elapsed_secs = outcome.elapsed.as_secs_f64(),
                        "code index first build failed"
                    );
                }
                (None, Some(note)) => {
                    if service.note_changed(repo_id, note) {
                        tracing::info!(
                            tenant = %outcome.tenant,
                            repository = %outcome.repository,
                            note = %note,
                            "code index first build not started"
                        );
                    }
                }
                (None, None) => {}
            }
            outcomes.push(outcome);
        }
    }
    outcomes
}

/// Build the index of each repository that a person asked for
/// (KAIROS-T-0332): a full build of the head of its default branch, as a
/// first build, which replaces the index of that commit. One at a time, in
/// the sequence of the requests. The run row of each request is ended with
/// the result.
pub async fn requested_builds(
    blocking: &BlockingTenantPool,
    service: &Arc<CodeIndexService>,
    summarizers: Option<Arc<dyn SummarizerSource>>,
    embedder: Arc<dyn kairos_embed::EmbeddingProvider>,
) -> Vec<BuildOutcome> {
    let mut outcomes = Vec::new();
    for request in service.pending_rebuilds() {
        let repo_id = request.repository_id;
        let repo = blocking
            .run(&request.tenant, move |conn| {
                kairos_db::repositories::load(conn, repo_id).map_err(ApiError::internal)
            })
            .await;
        let repo = match repo {
            Ok(repo) => repo,
            Err(e) => {
                let note = format!("failed: the repository is not readable: {e:?}");
                record_end(blocking, &request.tenant, Some(request.run), Err(&note)).await;
                tracing::warn!(tenant = %request.tenant, repository = %repo_id, note = %note, "code index requested build failed");
                continue;
            }
        };
        let outcome = full_build_repository(
            blocking,
            service,
            &request.tenant,
            repo,
            summarizers.clone(),
            Arc::clone(&embedder),
            "request",
            Some(request.run),
        )
        .await;
        match (&outcome.report, &outcome.note) {
            (Some(report), _) => tracing::info!(
                tenant = %outcome.tenant,
                repository = %outcome.repository,
                commit = %outcome.commit,
                files = report.build.files,
                symbols = report.build.symbols,
                elapsed_secs = outcome.elapsed.as_secs_f64(),
                "code index requested build ended"
            ),
            (None, Some(note)) => tracing::warn!(
                tenant = %outcome.tenant,
                repository = %outcome.repository,
                note = %note,
                "code index requested build failed"
            ),
            (None, None) => {}
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// The clone of a repository, fetched, and the head of its default branch.
struct Fetched {
    clone: PathBuf,
    head: String,
    /// The commits at and below the head, nearest first, when asked for.
    ancestors: Vec<String>,
}

/// Read the token of `repo`, fetch its clone, and find the head of its
/// default branch. The error is the note of the outcome.
async fn fetch_head(
    blocking: &BlockingTenantPool,
    service: &Arc<CodeIndexService>,
    tenant: &str,
    repo: &Repository,
    with_ancestors: bool,
) -> Result<Fetched, String> {
    // The read token of the repository, when it has one (COLLIERY-T-3105).
    let token = {
        let key = service.secrets_key().cloned();
        let tenant_slug = tenant.to_string();
        let repo_id = repo.id;
        blocking
            .run(tenant, move |conn| {
                Ok(crate::credentials::read_token(
                    conn,
                    &tenant_slug,
                    repo_id,
                    key.as_ref(),
                ))
            })
            .await
    };
    let token = match token {
        Ok(Ok(token)) => token,
        Ok(Err(e)) => return Err(format!("failed: {e}")),
        Err(e) => return Err(format!("failed: {e:?}")),
    };

    // The clone and the head of the default branch.
    let fetched = {
        let service = Arc::clone(service);
        let tenant = tenant.to_string();
        let repo = repo.clone();
        tokio::task::spawn_blocking(move || {
            let path = service.fetch(&tenant, &repo, token.as_ref())?;
            let head = git::branch_head(&path, &repo.default_branch)?;
            let ancestors = match &head {
                Some(head) if with_ancestors => git::ancestors(&path, head, MAX_DISTANCE)?,
                _ => Vec::new(),
            };
            Ok::<_, git::GitError>((path, head, ancestors))
        })
        .await
    };
    let (clone, head, ancestors) = match fetched {
        Ok(Ok(fetched)) => fetched,
        Ok(Err(e)) => return Err(format!("failed: {e}")),
        Err(e) => return Err(format!("failed: {e}")),
    };
    let Some(head) = head else {
        return Err(format!(
            "the repository has no branch {}",
            repo.default_branch
        ));
    };
    Ok(Fetched {
        clone,
        head,
        ancestors,
    })
}

impl CodeIndexService {
    fn failed_before(&self, key: &(uuid::Uuid, String)) -> bool {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(key)
    }

    fn record_failure(&self, key: (uuid::Uuid, String)) {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
    }
}

/// Store the index of `head` of `repo`, built by the builder.
/// The counts of a run, from its report (KAIROS-T-0331).
fn counts_of(report: &UpdateReport) -> runs::Counts {
    let clamp = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
    runs::Counts {
        files: clamp(report.build.files),
        symbols: clamp(report.build.symbols),
        edges: clamp(report.build.edges.total()),
        summaries_made: clamp(
            report.summary.symbols.summarized
                + report.summary.files.summarized
                + report.summary.modules.summarized,
        ),
        summaries_reused: clamp(
            report.summary.symbols.reused
                + report.summary.files.reused
                + report.summary.modules.reused,
        ),
    }
}

/// Tell the readers of a repository that a run started or ended
/// (KAIROS-T-0333): the event `code_index_build_changed`, with the slug of
/// the repository as its short code. Inside the transaction of the write.
pub(crate) fn notify_run(
    conn: &mut diesel::pg::PgConnection,
    slug: &str,
    actor: Option<uuid::Uuid>,
) -> Result<(), ApiError> {
    kairos_db::events::emit_event(
        conn,
        &kairos_db::events::ThinEvent {
            event: kairos_db::events::EventKind::CodeIndexBuildChanged,
            entity_type: "repository".to_string(),
            short_code: slug.to_string(),
            board_id: None,
            column_id: None,
            actor: actor.unwrap_or_else(uuid::Uuid::nil),
        },
    )
    .map_err(ApiError::internal)
}

/// The slug of the repository of a run, for its event.
fn slug_of_run(conn: &mut diesel::pg::PgConnection, run: &runs::Build) -> Option<String> {
    kairos_db::repositories::load(conn, run.repository_id)
        .ok()
        .map(|repo| repo.slug)
}

/// The start of the note of a repository that waits for the least time
/// between 2 updates (KAIROS-T-0334).
pub const MIN_REBUILD: &str = "the repository was updated a short time ago";

/// The start of the note of a repository that has an active run
/// (KAIROS-T-0354).
pub const ACTIVE_RUN: &str = "a run of the repository is active";

/// The note of a repository whose run `run` is active (KAIROS-T-0354).
fn active_note(run: &runs::Build) -> String {
    format!(
        "{ACTIVE_RUN}: run {} (trigger {}), started {}",
        run.id,
        run.trigger,
        run.started_at.to_rfc3339()
    )
}

/// The active run of `repo_id`: a `running` run of this server
/// (KAIROS-T-0354). None when it has none, or when the read fails.
async fn active_run(
    blocking: &BlockingTenantPool,
    service: &CodeIndexService,
    tenant: &str,
    repo_id: uuid::Uuid,
) -> Option<runs::Build> {
    let instance = service.instance();
    blocking
        .run(tenant, move |conn| {
            runs::active(conn, repo_id, instance).map_err(ApiError::internal)
        })
        .await
        .ok()
        .flatten()
}

/// What [`record_start`] did.
enum Started {
    /// The row of the new run. None when the write failed: the failure is
    /// logged, and the build goes on with no record.
    Run(Option<uuid::Uuid>),
    /// A run of this server is active on the repository (KAIROS-T-0354):
    /// no run starts.
    Active(Box<runs::Build>),
}

/// Start the record of a run (KAIROS-T-0331): a `running` row of this
/// server, after the stale rows of the repository are ended. A stale row is
/// a `running` row of another start of the server (KAIROS-T-0354). When a
/// run of this server is active on the repository, no run starts.
async fn record_start(
    blocking: &BlockingTenantPool,
    service: &CodeIndexService,
    tenant: &str,
    repo: &Repository,
    trigger: &'static str,
) -> Started {
    let repo_id = repo.id;
    let branch = repo.default_branch.clone();
    let slug = repo.slug.clone();
    let instance = service.instance();
    let written = blocking
        .run(tenant, move |conn| {
            crate::api::org::run_in_transaction(conn, |conn| {
                runs::lock_repository(conn, repo_id).map_err(ApiError::internal)?;
                runs::end_stale(conn, repo_id, instance).map_err(ApiError::internal)?;
                if let Some(active) =
                    runs::active(conn, repo_id, instance).map_err(ApiError::internal)?
                {
                    return Ok(Err(active));
                }
                let run = runs::start(conn, repo_id, trigger, Some(&branch), None, Some(instance))
                    .map_err(ApiError::internal)?;
                notify_run(conn, &slug, None)?;
                Ok(Ok(run))
            })
        })
        .await;
    match written {
        Ok(Ok(run)) => Started::Run(Some(run.id)),
        Ok(Err(active)) => Started::Active(Box::new(active)),
        Err(e) => {
            tracing::warn!(tenant, repository = %repo.slug, error = ?e, "code index run not recorded");
            Started::Run(None)
        }
    }
}

/// Give the record of a run its commit.
async fn record_commit(
    blocking: &BlockingTenantPool,
    tenant: &str,
    run: Option<uuid::Uuid>,
    commit: &str,
) {
    let Some(id) = run else { return };
    let commit = commit.to_string();
    if let Err(e) = blocking
        .run(tenant, move |conn| {
            runs::set_commit(conn, id, &commit).map_err(ApiError::internal)
        })
        .await
    {
        tracing::warn!(tenant, error = ?e, "code index run commit not recorded");
    }
}

/// End the record of a run: `ok` with the counts, or `failed` with the
/// text.
async fn record_end(
    blocking: &BlockingTenantPool,
    tenant: &str,
    run: Option<uuid::Uuid>,
    result: Result<(runs::Counts, String), &str>,
) {
    let Some(id) = run else { return };
    let result = result.map_err(str::to_owned);
    if let Err(e) = blocking
        .run(tenant, move |conn| {
            crate::api::org::run_in_transaction(conn, |conn| {
                let run = match &result {
                    Ok((counts, model)) => runs::end_ok(conn, id, Some(*counts), Some(model)),
                    Err(error) => runs::end_failed(conn, id, error),
                }
                .map_err(ApiError::internal)?;
                if let Some(slug) = slug_of_run(conn, &run) {
                    notify_run(conn, &slug, run.requested_by)?;
                }
                Ok(())
            })
        })
        .await
    {
        tracing::warn!(tenant, error = ?e, "code index run end not recorded");
    }
}

/// Record a failure that came before the run had a row: the fetch, or the
/// choice of the base. A repeat of the newest failure adds no row
/// ([`runs::record_failure_once`]).
async fn record_failure_once(
    blocking: &BlockingTenantPool,
    tenant: &str,
    repo: &Repository,
    trigger: &'static str,
    commit: Option<&str>,
    error: &str,
) {
    let repo_id = repo.id;
    let branch = repo.default_branch.clone();
    let slug = repo.slug.clone();
    let commit = commit.map(str::to_owned);
    let error = error.to_string();
    if let Err(e) = blocking
        .run(tenant, move |conn| {
            crate::api::org::run_in_transaction(conn, |conn| {
                let written = runs::record_failure_once(
                    conn,
                    repo_id,
                    trigger,
                    commit.as_deref(),
                    Some(&branch),
                    &error,
                )
                .map_err(ApiError::internal)?;
                if written.is_some() {
                    notify_run(conn, &slug, None)?;
                }
                Ok(())
            })
        })
        .await
    {
        tracing::warn!(tenant, repository = %repo.slug, error = ?e, "code index failure not recorded");
    }
}

/// End the `running` rows of each tenant that another start of the server
/// began, when the builder starts: that server stopped during those runs
/// (KAIROS-T-0331, KAIROS-T-0354). A run of `service` (this server) stays.
pub async fn end_stale_runs(blocking: &BlockingTenantPool, service: &CodeIndexService) {
    let instance = service.instance();
    let tenants = match blocking
        .run_public(|conn| kairos_db::list_tenants(conn).map_err(ApiError::internal))
        .await
    {
        Ok(tenants) => tenants,
        Err(e) => {
            tracing::warn!(error = ?e, "code index builder could not list tenants");
            return;
        }
    };
    for tenant in tenants.into_iter().filter(|t| t.schema_exists) {
        match blocking
            .run(&tenant.slug, move |conn| {
                runs::end_all_stale(conn, instance).map_err(ApiError::internal)
            })
            .await
        {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(tenant = %tenant.slug, runs = n, "code index runs ended: the server stopped during them")
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant.slug, error = ?e, "code index stale runs not ended")
            }
        }
    }
}

async fn store_built(
    blocking: &BlockingTenantPool,
    tenant: &str,
    repo: &Repository,
    head: String,
    split: kairos_index::store::Split,
) -> Result<(), String> {
    let index = kairos_db::code_indexes::NewCodeIndex {
        repository_id: repo.id,
        commit_sha: head,
        ref_name: Some(repo.default_branch.clone()),
        source: "build",
        structure: split.structure_gz,
        structure_bytes: split.structure_bytes as i64,
        summary_keys: split.keys.len() as i32,
        vector_model: split.vector_model,
        created_by: None,
    };
    let pool = used_rows(split.pool, &split.keys);
    blocking
        .run(tenant, move |conn| {
            kairos_db::code_indexes::put(conn, &index, &pool).map_err(ApiError::internal)
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("failed: {e:?}"))
}

async fn build_repository(
    blocking: &BlockingTenantPool,
    service: &Arc<CodeIndexService>,
    tenant: &str,
    repo: Repository,
    summarizers: Option<Arc<dyn SummarizerSource>>,
    embedder: Arc<dyn kairos_embed::EmbeddingProvider>,
) -> BuildOutcome {
    let started = Instant::now();
    let mut outcome = BuildOutcome {
        tenant: tenant.to_string(),
        repository: repo.slug.clone(),
        commit: String::new(),
        base: None,
        report: None,
        note: None,
        elapsed: Duration::ZERO,
    };
    let done = |mut outcome: BuildOutcome, note: String| {
        outcome.note = Some(note);
        outcome.elapsed = started.elapsed();
        outcome
    };
    if !repo.code_index_build_on() {
        return done(outcome, OPTED_OUT.into());
    }
    // A run of this server is active on the repository: a requested build
    // that waits for its turn, or a first build (KAIROS-T-0354). The update
    // waits for the next pass, and the active run keeps its row.
    if let Some(active) = active_run(blocking, service, tenant, repo.id).await {
        return done(outcome, active_note(&active));
    }

    let Fetched {
        clone,
        head,
        ancestors,
    } = match fetch_head(blocking, service, tenant, &repo, true).await {
        Ok(fetched) => fetched,
        Err(note) => {
            record_failure_once(blocking, tenant, &repo, "push", None, &note).await;
            return done(outcome, note);
        }
    };
    outcome.commit = head.clone();
    let key = (repo.id, head.clone());
    if service.failed_before(&key) {
        return done(outcome, "an earlier build of this commit failed".into());
    }

    // The nearest indexed commit, and its structure and the pool. An
    // index of another schema version (a server upgrade bumped it) cannot
    // start an update: the repository gets a full build, in place of a
    // failed update on each tick (KAIROS-T-0347).
    let repo_id = repo.id;
    let start = blocking
        .run(tenant, move |conn| {
            let listed =
                kairos_db::code_indexes::list(conn, repo_id).map_err(ApiError::internal)?;
            let newest = listed.iter().map(|i| i.updated_at).max();
            let indexed: HashSet<String> = listed.into_iter().map(|i| i.commit_sha).collect();
            let Some(nearest) = ancestors.iter().find(|c| indexed.contains(*c)) else {
                return Ok(None);
            };
            let prefix = kairos_db::code_indexes::structure_prefix(
                conn,
                repo_id,
                nearest,
                kairos_index::store::VERSION_PREFIX_BYTES as i32,
            )
            .map_err(ApiError::internal)?
            .unwrap_or_default();
            if let Some(found) = kairos_index::store::structure_version(&prefix)
                .filter(|found| *found != kairos_index::SCHEMA_VERSION)
            {
                return Ok(Some(Start::OldSchema {
                    commit: nearest.clone(),
                    found,
                }));
            }
            if *nearest == ancestors[0] {
                return Ok(Some(Start::HeadIndexed));
            }
            let structure = kairos_db::code_indexes::structure(conn, repo_id, nearest)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::internal("the index went away"))?;
            let pool =
                kairos_db::code_indexes::whole_pool(conn, repo_id).map_err(ApiError::internal)?;
            Ok(Some(Start::Base(Base {
                commit: nearest.clone(),
                structure,
                pool,
                newest,
            })))
        })
        .await;
    let base = match start {
        Ok(Some(Start::Base(base))) => base,
        Ok(Some(Start::HeadIndexed)) => return done(outcome, "the head has an index".into()),
        Ok(Some(Start::OldSchema { commit, found })) => {
            tracing::warn!(
                tenant = %tenant,
                repository = %repo.slug,
                commit = %commit,
                found,
                reads = kairos_index::SCHEMA_VERSION,
                "code index of another schema version: a full build"
            );
            return full_build_repository(
                blocking,
                service,
                tenant,
                repo,
                summarizers,
                embedder,
                "push",
                None,
            )
            .await;
        }
        // No indexed commit is an ancestor of the head within the limit: the
        // history was rewritten, or the index is of a commit that is gone.
        // An update cannot start, so the head gets a full build, in place of
        // a failed update that stays (KAIROS-T-0356).
        Ok(None) => {
            tracing::warn!(
                tenant = %tenant,
                repository = %repo.slug,
                commit = %head,
                limit = MAX_DISTANCE,
                "no indexed commit is below the head: a full build"
            );
            return full_build_repository(
                blocking,
                service,
                tenant,
                repo,
                summarizers,
                embedder,
                "push",
                None,
            )
            .await;
        }
        Err(e) => {
            let note = format!("failed: {e:?}");
            record_failure_once(blocking, tenant, &repo, "push", Some(&head), &note).await;
            return done(outcome, note);
        }
    };
    outcome.base = Some(base.commit.clone());
    // The least time between 2 updates of one repository (KAIROS-T-0334):
    // a repository updated a short time ago waits for a later pass.
    if let (Some(min), Some(newest)) = (service.min_rebuild(), base.newest) {
        let age = (chrono::Utc::now() - newest).to_std().unwrap_or_default();
        if age < min {
            return done(
                outcome,
                format!(
                    "{MIN_REBUILD}: the newest index is {} s old, and \
                     KAIROS_CODE_INDEX_MIN_REBUILD_SECS is {}",
                    age.as_secs(),
                    min.as_secs()
                ),
            );
        }
    }
    let run = match record_start(blocking, service, tenant, &repo, "push").await {
        Started::Run(run) => run,
        Started::Active(active) => return done(outcome, active_note(&active)),
    };
    record_commit(blocking, tenant, run, &head).await;
    let models = match models_for(blocking, service, tenant, &repo, &summarizers, &embedder).await {
        Ok(models) => models,
        Err(note) => {
            record_end(blocking, tenant, run, Err(&note)).await;
            return done(outcome, note);
        }
    };
    let model_name = models.summarizers.model_name();

    // The update, off the async threads: it runs rust-analyzer and the
    // model for minutes. It holds the gate for the whole build, so a first
    // build does not run a step at the same time (KAIROS-T-0318).
    let db = service.update_db(tenant, &repo);
    let built = {
        let head = head.clone();
        let service = Arc::clone(service);
        let db = db.clone();
        let tenant = tenant.to_string();
        let slug = repo.slug.clone();
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            // The gate: the whole build for the embedded model; the
            // structure only for a hosted provider (KAIROS-T-0341), whose
            // summaries take no CPU of the host.
            let mut held = Some(service.gate().update());
            let work = tempfile::Builder::new()
                .prefix("kairos-code-index-")
                .tempdir()
                .map_err(|e| format!("no work folder: {e}"))?;
            let tree = work.path().join("tree");
            std::fs::create_dir(&tree).map_err(|e| format!("no tree folder: {e}"))?;
            prepare_update_db(&db, &base.structure, base.pool, &tenant, &slug)?;
            git::checkout_tree(&clone, &head, &tree).map_err(|e| e.to_string())?;
            let options = UpdateOptions {
                // The server runs SCIP for each push (COLLIERY-I-0264,
                // decision 5 of 2026-10-01).
                rust_edges: true,
                build: service.build_options(),
                ..Default::default()
            };
            let build =
                kairos_index::update_structure(&tree, &db, &options).map_err(|e| e.to_string())?;
            if models.hosted_summaries && models.hosted_vectors {
                held.take();
            }
            let Models {
                summarizers,
                embedder,
                ..
            } = models;
            let report = summarizers
                .run(&mut |summarizer| {
                    let summary = kairos_index::summarize(
                        &tree,
                        &db,
                        summarizer,
                        embedder.as_ref(),
                        &SummarizeOptions::default(),
                    )?;
                    Ok(UpdateReport {
                        build: build.clone(),
                        summary,
                    })
                })
                .map_err(|e| e.to_string())?;
            drop(held);
            let split = kairos_index::store::split(&db).map_err(|e| e.to_string())?;
            Ok((report, split))
        })
        .await
    };
    let (report, split) = match built {
        Ok(Ok(built)) => built,
        Ok(Err(e)) => {
            service.record_failure(key);
            let note = format!("failed: {e}");
            record_end(blocking, tenant, run, Err(&note)).await;
            return done(outcome, note);
        }
        Err(e) => {
            let note = format!("failed: {e}");
            record_end(blocking, tenant, run, Err(&note)).await;
            return done(outcome, note);
        }
    };
    if let Err(note) = store_built(blocking, tenant, &repo, head, split).await {
        record_end(blocking, tenant, run, Err(&note)).await;
        return done(outcome, note);
    }
    record_end(blocking, tenant, run, Ok((counts_of(&report), model_name))).await;
    remove_database(&db);
    outcome.report = Some(report);
    outcome.elapsed = started.elapsed();
    outcome
}

/// Write the work database `db` of an update (KAIROS-T-0335): the
/// structure of the base and the pool of the repository, and the summaries
/// of the work database of an update that did not end, when `db` has one.
/// Those summaries are of keys that no index stores yet: the model made
/// them before a restart or a failure. The error is the text of a failed
/// run.
fn prepare_update_db(
    db: &Path,
    structure: &[u8],
    pool: Vec<kairos_db::code_indexes::PoolRow>,
    tenant: &str,
    slug: &str,
) -> Result<(), String> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("no folder {}: {e}", parent.display()))?;
    }
    let mut next = db.as_os_str().to_owned();
    next.push(".next");
    let next = PathBuf::from(next);
    remove_database(&next);
    kairos_index::store::assemble(structure, pool.into_iter().map(pool_row_of), &next)
        .map_err(|e| e.to_string())?;
    if db.is_file() {
        // A work database that cannot give its summaries (another schema
        // version, the vectors of another model) is not used.
        match kairos_index::store::copy_pool(db, &next) {
            Ok(()) => tracing::info!(
                tenant,
                repository = slug,
                "code index update reuses the summaries of an update that did not end"
            ),
            Err(e) => tracing::warn!(
                tenant,
                repository = slug,
                error = %e,
                "code index update cannot reuse the summaries of an update that did not end"
            ),
        }
    }
    remove_database(db);
    std::fs::rename(&next, db).map_err(|e| format!("no work database {}: {e}", db.display()))
}

/// A full build of one repository: the first build of a repository that
/// has no index (KAIROS-T-0318, `trigger = first`), or a build that a
/// person asked for (KAIROS-T-0332, `trigger = request`, with the run row
/// `run` that the request made). A requested build skips the memory of
/// the commits that failed before: the person asks for one more try.
#[allow(clippy::too_many_arguments)]
async fn full_build_repository(
    blocking: &BlockingTenantPool,
    service: &Arc<CodeIndexService>,
    tenant: &str,
    repo: Repository,
    summarizers: Option<Arc<dyn SummarizerSource>>,
    embedder: Arc<dyn kairos_embed::EmbeddingProvider>,
    trigger: &'static str,
    run: Option<uuid::Uuid>,
) -> BuildOutcome {
    let started = Instant::now();
    let mut outcome = BuildOutcome {
        tenant: tenant.to_string(),
        repository: repo.slug.clone(),
        commit: String::new(),
        base: None,
        report: None,
        note: None,
        elapsed: Duration::ZERO,
    };
    let done = |mut outcome: BuildOutcome, note: String| {
        outcome.note = Some(note);
        outcome.elapsed = started.elapsed();
        outcome
    };
    if !repo.code_index_build_on() {
        record_end(blocking, tenant, run, Err(OPTED_OUT)).await;
        return done(outcome, OPTED_OUT.into());
    }
    // A build with no run row of its own does not start while a run of
    // this server is active on the repository (KAIROS-T-0354).
    if run.is_none()
        && let Some(active) = active_run(blocking, service, tenant, repo.id).await
    {
        return done(outcome, active_note(&active));
    }

    let Fetched { clone, head, .. } = match fetch_head(blocking, service, tenant, &repo, false)
        .await
    {
        Ok(fetched) => fetched,
        Err(note) => {
            match run {
                Some(_) => record_end(blocking, tenant, run, Err(&note)).await,
                None => record_failure_once(blocking, tenant, &repo, trigger, None, &note).await,
            }
            return done(outcome, note);
        }
    };
    outcome.commit = head.clone();
    let key = (repo.id, head.clone());
    if run.is_none() && service.failed_before(&key) {
        return done(outcome, "an earlier build of this commit failed".into());
    }
    tracing::info!(
        tenant = %tenant,
        repository = %repo.slug,
        commit = %head,
        trigger,
        "code index full build started"
    );
    let run = match run {
        Some(id) => Some(id),
        None => match record_start(blocking, service, tenant, &repo, trigger).await {
            Started::Run(run) => run,
            Started::Active(active) => return done(outcome, active_note(&active)),
        },
    };
    record_commit(blocking, tenant, run, &head).await;
    let models = match models_for(blocking, service, tenant, &repo, &summarizers, &embedder).await {
        Ok(models) => models,
        Err(note) => {
            record_end(blocking, tenant, run, Err(&note)).await;
            return done(outcome, note);
        }
    };
    let model_name = models.summarizers.model_name();

    // The build, off the async threads: it runs for hours on a large
    // repository. It takes the gate for each step, so the updates of the
    // other repositories go between the steps.
    let db = service.first_build_db(tenant, &repo);
    let built = {
        let head = head.clone();
        let service = Arc::clone(service);
        let db = db.clone();
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            if let Some(parent) = db.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("no folder {}: {e}", parent.display()))?;
            }
            let work = tempfile::Builder::new()
                .prefix("kairos-code-index-first-")
                .tempdir()
                .map_err(|e| format!("no work folder: {e}"))?;
            let tree = work.path().join("tree");
            std::fs::create_dir(&tree).map_err(|e| format!("no tree folder: {e}"))?;
            git::checkout_tree(&clone, &head, &tree).map_err(|e| e.to_string())?;
            let options = UpdateOptions {
                rust_edges: true,
                build: service.build_options(),
                ..Default::default()
            };
            let build = {
                let _held = service.gate().first_build();
                kairos_index::update_structure(&tree, &db, &options)
            };
            let build = match build {
                Ok(build) => build,
                Err(e) => {
                    // A work database that the build cannot read (for
                    // example of another schema version) is removed, so
                    // that the next build starts again from nothing.
                    remove_database(&db);
                    return Err(e.to_string());
                }
            };
            let gate = service.gate();
            let Models {
                summarizers,
                embedder,
                hosted_summaries,
                hosted_vectors,
            } = models;
            let report = summarizers
                .run(&mut |summarizer| {
                    // The gate for each call of a model on the CPU; none for
                    // a hosted provider (KAIROS-T-0341).
                    let mut gated = GatedSummarizer {
                        inner: summarizer,
                        gate: (!hosted_summaries).then_some(gate),
                    };
                    let embedder = GatedEmbedder {
                        inner: embedder.as_ref(),
                        gate: (!hosted_vectors).then_some(gate),
                    };
                    let summary = kairos_index::summarize(
                        &tree,
                        &db,
                        &mut gated,
                        &embedder,
                        &SummarizeOptions::default(),
                    )?;
                    Ok(UpdateReport {
                        build: build.clone(),
                        summary,
                    })
                })
                .map_err(|e| e.to_string())?;
            let split = kairos_index::store::split(&db).map_err(|e| e.to_string())?;
            Ok((report, split))
        })
        .await
    };
    let (report, split) = match built {
        Ok(Ok(built)) => built,
        Ok(Err(e)) => {
            service.record_failure(key);
            let note = format!("failed: {e}");
            record_end(blocking, tenant, run, Err(&note)).await;
            return done(outcome, note);
        }
        Err(e) => {
            let note = format!("failed: {e}");
            record_end(blocking, tenant, run, Err(&note)).await;
            return done(outcome, note);
        }
    };
    if let Err(note) = store_built(blocking, tenant, &repo, head, split).await {
        record_end(blocking, tenant, run, Err(&note)).await;
        return done(outcome, note);
    }
    record_end(blocking, tenant, run, Ok((counts_of(&report), model_name))).await;
    remove_database(&db);
    outcome.report = Some(report);
    outcome.elapsed = started.elapsed();
    outcome
}

/// Remove the SQLite database at `path` and its journal files. A file that
/// is not there is not an error.
fn remove_database(path: &Path) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(file));
    }
}

/// Run the builder for ever, in 2 lanes (KAIROS-T-0318):
///
/// - the update lane runs a [`sweep`] on `interval`, then wakes the
///   first-build lane;
/// - the first-build lane runs [`first_builds`] each time it is woken. A
///   wake that comes during a pass starts one more pass after it.
///
/// The update lane does not wait for the first-build lane, so a first build
/// of hours does not stop the updates. An error is logged and the next pass
/// tries again; it never stops the server.
pub async fn run_builder(
    blocking: BlockingTenantPool,
    service: Arc<CodeIndexService>,
    summarizers: Option<Arc<dyn SummarizerSource>>,
    embedder: Arc<dyn kairos_embed::EmbeddingProvider>,
    interval: Duration,
) {
    tracing::info!(
        interval_secs = interval.as_secs(),
        "code index builder started"
    );
    end_stale_runs(&blocking, &service).await;
    let wake = tokio::sync::Notify::new();
    let updates = async {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // A request (KAIROS-T-0332) goes before the next sweep.
            tokio::select! {
                _ = ticker.tick() => {
                    sweep(
                        &blocking,
                        &service,
                        summarizers.clone(),
                        Arc::clone(&embedder),
                    )
                    .await;
                    wake.notify_one();
                }
                _ = service.wake().notified() => {
                    requested_builds(
                        &blocking,
                        &service,
                        summarizers.clone(),
                        Arc::clone(&embedder),
                    )
                    .await;
                }
            }
        }
    };
    let firsts = async {
        loop {
            wake.notified().await;
            first_builds(
                &blocking,
                &service,
                summarizers.clone(),
                Arc::clone(&embedder),
            )
            .await;
        }
    };
    tokio::join!(updates, firsts);
}

/// Whether the summaries of `repo` come from the hosted provider of the
/// tenant: its own value, or the default of the organization when it
/// follows the organization (KAIROS-T-0358), and the tenant has a hosted
/// provider. The ONE rule, for the builder and for the API.
pub(crate) fn hosted_summaries(
    repo: &Repository,
    settings: &kairos_db::code_index_settings::Settings,
) -> bool {
    repo.summaries_with(&settings.default_summaries) == "hosted"
        && settings.summary_provider != "embedded"
}

/// A pool row of the database as `kairos-index` takes it.
pub(crate) fn pool_row_of(row: kairos_db::code_indexes::PoolRow) -> kairos_index::store::PoolRow {
    kairos_index::store::PoolRow {
        key: row.key,
        level: row.level,
        summary: row.summary,
        vector: row.vector,
        model: row.model,
    }
}

/// The rows of `pool` whose keys the structure uses, as the database takes
/// them. A row that no structure uses is not stored.
pub(crate) fn used_rows(
    pool: Vec<kairos_index::store::PoolRow>,
    keys: &[String],
) -> Vec<kairos_db::code_indexes::PoolRow> {
    let keys: HashSet<&str> = keys.iter().map(String::as_str).collect();
    pool.into_iter()
        .filter(|row| keys.contains(row.key.as_str()))
        .map(|row| kairos_db::code_indexes::PoolRow {
            key: row.key,
            level: row.level,
            summary: row.summary,
            vector: row.vector,
            model: row.model,
        })
        .collect()
}

#[cfg(all(test, feature = "llama"))]
mod tests {
    use super::*;

    #[test]
    fn the_builder_summarizes_on_the_threads_of_the_setting() {
        let tools = Tools {
            model: Some(PathBuf::from("/tools/model.gguf")),
            rust_analyzer: PathBuf::from("/tools/rust-analyzer"),
            rust_src: PathBuf::from("/tools/rust-src.tar.gz"),
            sysroot: PathBuf::from("/tools/rust"),
            downloaded: Vec::new(),
            hashed: Vec::new(),
        };
        let summarizers = llama_summarizers(&tools, 6);
        assert_eq!(summarizers.model, PathBuf::from("/tools/model.gguf"));
        assert_eq!(summarizers.threads, 6);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn an_update_goes_before_the_next_step_of_a_first_build() {
        let gate = Arc::new(WorkGate::default());
        // A first build holds the gate for one step.
        let step = gate.first_build();
        // An update comes, and waits for that step.
        let updated = Arc::new(AtomicBool::new(false));
        let update = {
            let (gate, updated) = (Arc::clone(&gate), Arc::clone(&updated));
            std::thread::spawn(move || {
                let _held = gate.update();
                std::thread::sleep(Duration::from_millis(100));
                updated.store(true, Ordering::SeqCst);
            })
        };
        while gate.state.lock().unwrap().updates_waiting == 0 {
            std::thread::yield_now();
        }
        assert!(!updated.load(Ordering::SeqCst), "the step holds the gate");
        drop(step);
        // The next step of the first build waits until the update ends.
        let next = gate.first_build();
        assert!(updated.load(Ordering::SeqCst), "the update went first");
        drop(next);
        update.join().unwrap();
    }

    #[test]
    fn a_first_build_takes_the_gate_when_no_update_wants_it() {
        let gate = WorkGate::default();
        drop(gate.first_build());
        drop(gate.first_build());
        drop(gate.update());
        drop(gate.first_build());
    }
}
