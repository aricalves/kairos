//! The local MCP server of the code tools (COLLIERY-I-0264, "What an agent
//! gets"; COLLIERY-T-1852): `kairos index mcp` talks MCP over stdio and
//! reads the index of one checkout.
//!
//! Each tool call opens the index again. So a `kairos index update` that
//! runs while the server runs is seen at the next call. The results are
//! short texts (ASD-STE100), as the results of the Kairos server are.
//!
//! The checkout that the tools read is the one the server started in, when
//! that is a git checkout. The agent can open another one, or the first one
//! when the session started outside a checkout, with `fetch_index`: the
//! server takes the nearest index of the repository from Kairos and updates
//! it for the files that differ (KAIROS-T-0348).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kairos_embed::EmbeddingProvider;
use kairos_index::{
    CallEdge, DEFAULT_MIN_LINES, DuplicateKind, DuplicateOptions, Index, IndexError, Lookup,
    SearchMode, SymbolInfo, UpdateOptions,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, ServerCapabilities,
    ServerInfo,
};
use rmcp::schemars::JsonSchema;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;

use super::arguments::{Parameters, tool_error_of};
use super::error::{Choose, IndexCommandError, Place, SymbolArguments, ToolError};
use super::query::{QuerySource, ROUTE_TIMEOUT, RouteFailure};

/// The results of `code_search` when the call gives no `limit`.
const DEFAULT_LIMIT: usize = 10;
/// The most results of one `code_search`.
const MAX_LIMIT: usize = 50;
/// The groups of `duplicates` when the call gives no `limit`.
pub(super) const DEFAULT_GROUPS: usize = 20;
/// The most groups of one `duplicates`.
pub(super) const MAX_GROUPS: usize = 200;

/// Serve the code tools on stdin and stdout until the client closes stdin.
/// The tools read the checkout at `root` when it is one; otherwise the
/// agent opens a checkout with `fetch_index`.
pub async fn serve(root: PathBuf) -> Result<(), IndexCommandError> {
    let server = CodeTools::new(root)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| IndexCommandError::Server(e.to_string()))?;
    server
        .waiting()
        .await
        .map_err(|e| IndexCommandError::Server(e.to_string()))?;
    Ok(())
}

/// The source of the query vectors, chosen on the first search that needs
/// it: the checkout, the model of its index, and the source
/// (KAIROS-T-0360).
type QueryEmbedder = (Option<PathBuf>, String, QuerySource);

#[derive(Clone)]
pub struct CodeTools {
    /// The open checkout: the one the server started in, or the last one
    /// that `fetch_index` opened. `None` until a checkout is open.
    root: Arc<Mutex<Option<PathBuf>>>,
    embedder: Arc<Mutex<Option<QueryEmbedder>>>,
    tool_router: ToolRouter<Self>,
}

// The arguments of each tool. `deny_unknown_fields` is the rule of the
// Kairos MCP tools: an argument that does nothing tells the agent that it
// did something.

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct FetchIndexParams {
    /// The path of the checkout: any folder inside a git checkout.
    pub root: String,
    /// The repository in Kairos, by its slug. Default: the repository whose URL is the `origin` remote of the checkout.
    pub repository: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    /// What the code does, in plain words.
    pub query: String,
    /// The most results to give, from 1 to 50. Default 10.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct SymbolParams {
    /// A name, `Type::name` (Rust) or `Type.name`. A candidate from the list
    /// of an AMBIGUOUS error, `name (file:line)`, is also correct.
    pub symbol: String,
    /// The file of the symbol, from the root of the checkout, or the end of
    /// its path. Give it when more than one symbol has the name.
    pub file: Option<String>,
    /// A line of the symbol: the symbol that starts on it, or else the
    /// smallest symbol that holds it. Give it when more than one symbol of
    /// one file has the name.
    pub line: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct EdgeParams {
    /// A name, `Type::name` (Rust) or `Type.name`. A candidate from the list
    /// of an AMBIGUOUS error, `name (file:line)`, is also correct.
    pub symbol: String,
    /// The file of the symbol, when more than one symbol has the name.
    pub file: Option<String>,
    /// A line of the symbol, when more than one symbol of one file has the
    /// name: the symbol that starts on it, or else the smallest symbol that
    /// holds it.
    pub line: Option<u32>,
    /// Also give the possible calls: calls by a name that more than one
    /// function has. Default false.
    pub possible: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct PathParams {
    /// The symbol where the chain starts, as `symbol` of the tool `symbol`.
    pub from: String,
    /// The symbol where the chain stops, as `symbol` of the tool `symbol`.
    pub to: String,
    /// The file of `from`, when more than one symbol has the name.
    pub from_file: Option<String>,
    /// The file of `to`, when more than one symbol has the name.
    pub to_file: Option<String>,
    /// A line of `from`, when more than one symbol of one file has the name.
    pub from_line: Option<u32>,
    /// A line of `to`, when more than one symbol of one file has the name.
    pub to_line: Option<u32>,
}

/// A kind of repeated code, as an argument.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "kebab-case")]
pub enum KindParam {
    Exact,
    Near,
    SameIdea,
}

impl From<KindParam> for DuplicateKind {
    fn from(k: KindParam) -> Self {
        match k {
            KindParam::Exact => DuplicateKind::Exact,
            KindParam::Near => DuplicateKind::Near,
            KindParam::SameIdea => DuplicateKind::SameIdea,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct DuplicatesParams {
    /// Only this kind: `exact` (the same code with other whitespace or
    /// comments), `near` (a copy with renamed names or a few changed lines) or
    /// `same-idea` (summaries with the same meaning). Default: all 3.
    pub kind: Option<KindParam>,
    /// The smallest function to compare, in lines. Default 5.
    pub min_lines: Option<u32>,
    /// Also compare test code. Default false.
    pub tests: Option<bool>,
    /// Only the functions in files under this path, from the root of the
    /// checkout.
    pub under: Option<String>,
    /// The most groups to give, from 1 to 200. Default 20.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct ModuleMapParams {
    /// Only the folders under this path, from the root of the checkout.
    pub under: Option<String>,
}

impl CodeTools {
    /// The tools of `root`. A folder that is not a git checkout and has no
    /// index opens nothing: the agent opens a checkout with `fetch_index`.
    pub fn new(root: PathBuf) -> Self {
        let open = root.join(kairos_index::INDEX_FILE).is_file() || root.join(".git").exists();
        CodeTools {
            root: Arc::new(Mutex::new(open.then_some(root))),
            embedder: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// The open checkout, or `None`.
    fn root(&self) -> Option<PathBuf> {
        self.root.lock().expect("the lock of the root").clone()
    }

    fn open(&self) -> Result<Index, ToolError> {
        let root = self.root().ok_or(ToolError::NoCheckout)?;
        let db = root.join(kairos_index::INDEX_FILE);
        if !db.is_file() {
            return Err(ToolError::NoIndex(db));
        }
        Ok(Index::open(&db)?)
    }

    /// `fetch_index`: open the checkout at `params.root`. Kairos gives the
    /// nearest index of the repository; the server puts it at
    /// `.kairos/index.db` of the checkout, updates the structure for the
    /// files that differ, and links the summaries of the pool. It runs no
    /// model. With no base from Kairos, a local index is used when there is
    /// one.
    async fn fetch(&self, params: FetchIndexParams) -> Result<String, ToolError> {
        let given = PathBuf::from(&params.root);
        let folder = given
            .canonicalize()
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| ToolError::NotACheckout(given.clone()))?;
        let root = super::git_output(&folder, &["rev-parse", "--show-toplevel"])
            .map(PathBuf::from)
            .ok_or_else(|| ToolError::NotACheckout(given.clone()))?;
        let root = root.canonicalize().unwrap_or(root);

        let remote = super::base::RemoteArgs {
            repository: params.repository,
            ..Default::default()
        };
        let found = super::base::find(&root, &remote).await;
        let (db, _exclude) = super::prepare_quiet(&root)?;
        // One process at a time (KAIROS-T-0357): another agent can fetch the
        // index of this checkout now. The wait is off the async threads.
        let _lock = {
            let db = db.clone();
            tokio::task::spawn_blocking(move || super::lock_index(&db))
                .await
                .map_err(|e| IndexCommandError::Server(e.to_string()))??
        };
        let had_index = db.is_file();
        let mut lines = Vec::new();
        let mut hosted = false;
        // The count of the files of the tree that differ from the local
        // index, when the CLI can read it.
        let local_changed = if had_index {
            let (root, db) = (root.clone(), db.clone());
            tokio::task::spawn_blocking(move || {
                kairos_index::changed_files(&root, &db)
                    .ok()
                    .map(|c| c.len())
            })
            .await
            .ok()
            .flatten()
        } else {
            None
        };
        match found {
            // The local index is nearer to the tree than the base: it keeps
            // its structure, and takes the summaries of the base when the
            // base has more (KAIROS-T-0355), as `kairos index update` does.
            super::base::Found::Base(b) if local_changed.is_some_and(|n| n <= b.changed) => {
                let n = local_changed.unwrap_or_default();
                if n > super::DEFAULT_MAX_CHANGED {
                    return Err(super::too_far(&root, &b, super::DEFAULT_MAX_CHANGED).into());
                }
                lines.push(format!(
                    "Repository {}: the local index is nearer to the tree than the index of \
                     commit {} from Kairos ({} changed, against {}). The local index is used.",
                    b.repository,
                    b.short(),
                    plural(n, "file", "files"),
                    b.changed
                ));
                if let Some(line) = super::take_base_summaries(&db, &b).await {
                    lines.push(line);
                }
                hosted = b.hosted;
            }
            super::base::Found::Base(b) => {
                if b.changed > super::DEFAULT_MAX_CHANGED {
                    return Err(super::too_far(&root, &b, super::DEFAULT_MAX_CHANGED).into());
                }
                let bytes = b.download().await.map_err(|why| ToolError::Download {
                    commit: b.short().to_string(),
                    why,
                })?;
                if let Some(warning) = super::install_quiet(&db, &bytes, had_index)? {
                    lines.push(warning);
                }
                lines.push(format!(
                    "Repository {}: the index of commit {} from Kairos, {} changed since it.",
                    b.repository,
                    b.short(),
                    plural(b.changed, "file", "files")
                ));
                hosted = b.hosted;
            }
            found => {
                let why = super::reason(&found);
                if !had_index {
                    return Err(ToolError::NoBaseIndex { root, why });
                }
                lines.push(format!(
                    "Kairos gave no base index, because {why}. The local index is used."
                ));
            }
        }

        // The update, off the async threads: it parses the changed files.
        let (report, outcome) = {
            let root = root.clone();
            let db = db.clone();
            tokio::task::spawn_blocking(move || -> Result<_, IndexCommandError> {
                let options = UpdateOptions {
                    rust_edges: false,
                    ..UpdateOptions::default()
                };
                let report = kairos_index::update_structure(&root, &db, &options)?;
                let no_model = if hosted {
                    super::summaries::HOSTED
                } else {
                    super::summaries::LINK_ONLY
                };
                let outcome = super::summaries::run(&root, &db, Some(no_model))?;
                Ok((report, outcome))
            })
            .await
            .map_err(|e| IndexCommandError::Server(e.to_string()))??
        };
        let summary = match &outcome {
            super::summaries::Outcome::Linked(r) | super::summaries::Outcome::NotMade(r, _) => r,
            #[cfg(feature = "llama")]
            super::summaries::Outcome::Made(r, _) => r,
        };
        let linked = summary.symbols.reused + summary.files.reused + summary.modules.reused;
        let left = summary.symbols.left + summary.files.left + summary.modules.left;

        *self.root.lock().expect("the lock of the root") = Some(root.clone());
        lines.insert(
            0,
            format!("The server opened the index of {}.", display(&root)),
        );
        lines.push(format!(
            "{}, {}, {} linked from the pool, {} with none. The tools answer for this checkout now.",
            plural(report.files, "file", "files"),
            plural(report.symbols, "symbol", "symbols"),
            plural(linked, "summary", "summaries"),
            left
        ));
        Ok(lines.join(" "))
    }

    /// The model of the vectors of the index of the open checkout.
    fn vector_model(&self) -> Result<Option<String>, ToolError> {
        Ok(self.open()?.vector_model()?)
    }

    /// The source of the query vectors for the model of the pool, kept for
    /// the checkout and the model.
    fn query_source(&self, model: &str) -> QuerySource {
        let root = self.root();
        let mut cached = self.embedder.lock().expect("the lock of the embedder");
        if let Some((cached_root, cached_model, source)) = cached.as_ref()
            && cached_model == model
            && *cached_root == root
        {
            return source.clone();
        }
        let source = super::query::query_source(model, root.as_deref());
        *cached = Some((root, model.to_string(), source.clone()));
        source
    }

    /// The provider of the vector of `query`, and whether Kairos made the
    /// vector; or why there is none. A lasting refusal of Kairos is kept as
    /// the reason, so the next search does not ask again.
    async fn query_provider(
        &self,
        model: &str,
        query: &str,
    ) -> Result<(Arc<dyn EmbeddingProvider>, bool), String> {
        let source = self.query_source(model);
        let from_kairos = matches!(source, QuerySource::Kairos(_));
        match super::query::query_provider(&source, model, query, ROUTE_TIMEOUT).await {
            Ok(provider) => Ok((provider, from_kairos)),
            Err(RouteFailure::Once(why)) => Err(why),
            Err(RouteFailure::Lasting(why)) => {
                if from_kairos {
                    let mut cached = self.embedder.lock().expect("the lock of the embedder");
                    if let Some((_, cached_model, source)) = cached.as_mut()
                        && cached_model == model
                    {
                        *source = QuerySource::Text(why.clone());
                    }
                }
                Err(why)
            }
        }
    }
}

/// The arguments that choose a symbol in `symbol`, `callers` and `callees`.
const SYMBOL: SymbolArguments = SymbolArguments {
    symbol: "symbol",
    file: "file",
    line: "line",
};
/// The arguments of `path` for `from`.
const FROM: SymbolArguments = SymbolArguments {
    symbol: "from",
    file: "from_file",
    line: "from_line",
};
/// The arguments of `path` for `to`.
const TO: SymbolArguments = SymbolArguments {
    symbol: "to",
    file: "to_file",
    line: "to_line",
};

/// Find the symbol, or the error that says why not. Each tool that takes a
/// symbol finds it here (KAIROS-T-0417).
fn find(
    index: &Index,
    symbol: &str,
    file: Option<&str>,
    line: Option<u32>,
    args: SymbolArguments,
) -> Result<SymbolInfo, ToolError> {
    match index.lookup(symbol, file, line)? {
        Lookup::Found(found) => Ok(found),
        Lookup::Missing => Err(ToolError::NoSymbol {
            name: symbol.trim().to_string(),
            place: Place::of(file, line),
        }),
        Lookup::Ambiguous(all) => Err(ambiguous(symbol, &all, args)),
    }
}

/// The error for more than one symbol: the candidates, and the argument
/// that chooses one of them.
fn ambiguous(symbol: &str, all: &[SymbolInfo], args: SymbolArguments) -> ToolError {
    let files: std::collections::HashSet<&str> = all.iter().map(|s| s.file.as_str()).collect();
    ToolError::Ambiguous {
        name: symbol.trim().to_string(),
        count: all.len(),
        candidates: all.iter().take(20).map(described).collect(),
        choose: Choose::of(files.len(), all.len(), args),
    }
}

/// `Stack::pop (src/stack.rs:20)`.
fn described(s: &SymbolInfo) -> String {
    format!("{} ({})", s.qualified(), s.place())
}

fn text(t: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(t)])
}

fn error(e: ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.text())])
}

fn result(r: Result<String, ToolError>) -> Result<CallToolResult, ErrorData> {
    Ok(match r {
        Ok(t) => text(t),
        Err(e) => error(e),
    })
}

/// The marks of an edge after its class: where it comes from when that is
/// not plain, and whether it waits for SCIP.
fn marks(edge: &CallEdge) -> String {
    let mut out = edge.class.clone();
    if edge.class == "possible" {
        out.push_str(&format!(
            ": 1 of {} functions with this name",
            edge.candidates
        ));
    }
    if edge.origin == "macro-text" {
        out.push_str(", in the text of a macro");
    }
    if edge.scip_pending {
        out.push_str(", by name until the next SCIP run");
    }
    out
}

/// The text of `duplicates`, for the MCP tool and for `kairos index
/// duplicates` (COLLIERY-T-1857): the count, what was left out, then each
/// group with its kind, its score and its symbols.
pub(super) fn duplicates_text(
    index: &Index,
    options: &DuplicateOptions,
    limit: usize,
) -> Result<String, IndexError> {
    let found = index.duplicates(options)?;
    let compared = plural(found.compared, "function", "functions");
    let mut out = if found.groups.is_empty() {
        format!("No repeated code is in the {compared} that were compared.")
    } else {
        let mut head = format!(
            "{} of repeated code in {compared}.",
            plural(found.groups.len(), "group", "groups")
        );
        if found.groups.len() > limit {
            head.push_str(&format!(
                " Only {} shown.",
                plural(limit, "group is", "groups are")
            ));
        }
        head
    };
    if !options.include_tests {
        out.push_str(" Test code is left out.");
    }
    if options.min_lines > 1 {
        out.push_str(&format!(
            " Functions under {} lines are left out.",
            options.min_lines
        ));
    }
    if options.kinds.contains(&DuplicateKind::SameIdea) && !found.summary_vectors {
        out.push_str(" The functions have no summary vectors, so no search for the same idea ran.");
    }
    for (i, g) in found.groups.iter().take(limit).enumerate() {
        out.push_str(&format!(
            "\n{}. {}, score {:.2}, {}:",
            i + 1,
            g.kind.as_str(),
            g.score,
            plural(g.symbols.len(), "symbol", "symbols")
        ));
        for s in &g.symbols {
            out.push_str(&format!(
                "\n- {} ({}:{}-{})",
                s.qualified(),
                s.file,
                s.start_line,
                s.end_line
            ));
        }
    }
    Ok(out)
}

pub(super) fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

#[tool_router]
impl CodeTools {
    #[tool(
        description = "Open the index of a checkout. Give `root`, the path of the checkout. The server takes the nearest index of its repository from Kairos, updates it for the files that differ, and the other tools answer for this checkout. Give `repository`, the slug in Kairos, when the origin remote of the checkout is not a repository of Kairos. Call it first when the session did not start inside a checkout."
    )]
    async fn fetch_index(
        &self,
        Parameters(params): Parameters<FetchIndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.fetch(params).await)
    }

    #[tool(
        description = "Search the code of this checkout by what it does. Give a description in plain words. The result is the best functions and types, with their files, lines and summaries. If the index has no summaries, the search reads the names, the signatures and the paths, and the result says so."
    )]
    async fn code_search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return result(Err(ToolError::Limit(MAX_LIMIT)));
        }
        // The vector of the query first: Kairos can make it (KAIROS-T-0360),
        // and the index is not held over the request.
        let mut why_no_vectors = None;
        let mut from_kairos = false;
        let embedder = match self.vector_model() {
            Err(e) => return result(Err(e)),
            Ok(None) => None,
            Ok(Some(model)) => match self.query_provider(&model, &params.query).await {
                Ok((provider, kairos)) => {
                    from_kairos = kairos;
                    Some(provider)
                }
                Err(why) => {
                    why_no_vectors = Some(why);
                    None
                }
            },
        };
        result((|| {
            let index = self.open()?;
            let found = index.search(&params.query, embedder.as_deref(), limit)?;
            let mut out = match found.mode {
                SearchMode::Vectors if from_kairos => "The search used the summaries and their \
                     vectors. Kairos made the vector of the query."
                    .to_string(),
                SearchMode::Vectors => {
                    "The search used the summaries and their vectors.".to_string()
                }
                SearchMode::SummaryText => format!(
                    "The search used the text of the summaries only. {}",
                    why_no_vectors.unwrap_or_else(|| "The summaries have no vectors.".into())
                ),
                SearchMode::Names => "The index has no summaries. So the search used the names, \
                     the signatures and the paths only. A build with the summarizer adds them."
                    .to_string(),
            };
            if found.hits.is_empty() {
                out.push_str(&format!("\nNo result for {:?}.", params.query));
                return Ok(out);
            }
            out.push_str(&format!(
                "\n{} for {:?}:",
                plural(found.hits.len(), "result", "results"),
                params.query
            ));
            for (i, hit) in found.hits.iter().enumerate() {
                let s = &hit.symbol;
                out.push_str(&format!("\n{}. {}, {}", i + 1, described(s), s.kind));
                if s.is_test {
                    out.push_str(", test code");
                }
                match (&s.summary, &s.signature) {
                    (Some(summary), _) => out.push_str(&format!(": {summary}")),
                    (None, Some(signature)) => out.push_str(&format!(": `{signature}`")),
                    (None, None) => {}
                }
                if s.summary.is_none() && found.mode != SearchMode::Names {
                    out.push_str(" (no summary yet: found by its name and path)");
                }
            }
            Ok(out)
        })())
    }

    #[tool(
        description = "One symbol of this checkout: its kind, file, lines, signature and summary. `symbol` is a name, `Type::name` or `Type.name`. When a type and its impl blocks have the name, the result is the type. If more than one symbol has the name, the error lists them: give `file`, `line`, or a candidate from the list as `symbol`, to choose one."
    )]
    async fn symbol(
        &self,
        Parameters(params): Parameters<SymbolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        result((|| {
            let index = self.open()?;
            let s = find(
                &index,
                &params.symbol,
                params.file.as_deref(),
                params.line,
                SYMBOL,
            )?;
            let mut out = format!(
                "{} ({}, {})\nFile: {}:{}-{}",
                s.qualified(),
                s.kind,
                s.language,
                s.file,
                s.start_line,
                s.end_line
            );
            if let Some(signature) = &s.signature {
                out.push_str(&format!("\nSignature: `{signature}`"));
            }
            match &s.summary {
                Some(summary) => out.push_str(&format!("\nSummary: {summary}")),
                None => out.push_str("\nSummary: none."),
            }
            if s.is_test {
                out.push_str("\nThis is test code.");
            }
            Ok(out)
        })())
    }

    #[tool(
        description = "The functions that call a symbol of this checkout. Give the symbol as for the tool `symbol`, with `file` and `line` when more than one symbol has the name. The result has the certain calls only, unless `possible` is true. A possible call is a call by a name that more than one function has: the result marks it. A call marked \"by name until the next SCIP run\" is in changed Rust code."
    )]
    async fn callers(
        &self,
        Parameters(params): Parameters<EdgeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        result((|| {
            let index = self.open()?;
            let s = find(
                &index,
                &params.symbol,
                params.file.as_deref(),
                params.line,
                SYMBOL,
            )?;
            let all = index.callers(s.id, true)?;
            let possible = params.possible.unwrap_or(false);
            let (shown, hidden): (Vec<&CallEdge>, Vec<&CallEdge>) =
                all.iter().partition(|e| possible || e.class == "certain");
            let mut out = format!(
                "{} of {}:",
                plural(shown.len(), "caller", "callers"),
                described(&s)
            );
            for e in &shown {
                // A call in the body of a macro_rules! definition: each
                // function that the macro makes does the call (KAIROS-T-0353).
                let made = if e.other.kind == "macro" {
                    ": each function that the macro makes does this call"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "\n- {}, call at line {}, {}{made}",
                    described(&e.other),
                    e.line,
                    marks(e)
                ));
            }
            if !hidden.is_empty() {
                out.push_str(&format!(
                    "\n{} not shown. Set `possible` to true to see them.",
                    plural(hidden.len(), "possible caller is", "possible callers are")
                ));
            }
            Ok(out)
        })())
    }

    #[tool(
        description = "The functions of this checkout that a symbol calls, and the names outside the checkout that it calls. Give the symbol as for the tool `symbol`, with `file` and `line` when more than one symbol has the name. The result has the certain calls only, unless `possible` is true. A possible call has one line for each function with that name, and the result marks it."
    )]
    async fn callees(
        &self,
        Parameters(params): Parameters<EdgeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        result((|| {
            let index = self.open()?;
            let s = find(
                &index,
                &params.symbol,
                params.file.as_deref(),
                params.line,
                SYMBOL,
            )?;
            let possible = params.possible.unwrap_or(false);
            let kairos_index::Callees {
                edges,
                no_symbol,
                externals,
            } = index.callees(s.id, possible)?;
            let mut out = format!(
                "{} calls {} in the checkout:",
                described(&s),
                plural(edges.len() + no_symbol.len(), "function", "functions")
            );
            // One line for each callee, in the order of the calls. A callee
            // with no symbol (a function that a macro makes) shows the place
            // of its definition (COLLIERY-T-2531).
            let mut lines: Vec<(u32, String)> = edges
                .iter()
                .map(|e| {
                    (
                        e.line,
                        format!(
                            "\n- {}, call at line {}, {}",
                            described(&e.other),
                            e.line,
                            marks(e)
                        ),
                    )
                })
                .collect();
            lines.extend(no_symbol.iter().map(|c| {
                let made = if c.macro_made {
                    "made by a macro there, with no symbol"
                } else {
                    "defined there, with no symbol"
                };
                (
                    c.call_line,
                    format!(
                        "\n- {} ({}:{}, {made}), call at line {}, {}",
                        c.name, c.file, c.line, c.call_line, c.class
                    ),
                )
            }));
            lines.sort_by_key(|(line, _)| *line);
            for (_, line) in lines {
                out.push_str(&line);
            }
            if !externals.is_empty() {
                let mut seen = std::collections::HashSet::new();
                let names: Vec<&str> = externals
                    .iter()
                    .map(|e| e.name.as_str())
                    .filter(|name| seen.insert(*name))
                    .collect();
                out.push_str(&format!("\nOutside the checkout: {}", names.join(", ")));
            }
            if !possible {
                let all = index.callees(s.id, true)?.edges.len();
                if all > edges.len() {
                    out.push_str(&format!(
                        "\n{} not shown. Set `possible` to true to see them.",
                        plural(
                            all - edges.len(),
                            "possible callee is",
                            "possible callees are"
                        )
                    ));
                }
            }
            Ok(out)
        })())
    }

    #[tool(
        description = "How one symbol of this checkout reaches another: the 3 shortest chains of certain calls from `from` to `to`, shortest first, each with other steps. Give `from` and `to` as `symbol` of the tool `symbol`, with `from_file`, `from_line`, `to_file` and `to_line` when more than one symbol has a name. Each chain has one call on each line, in order. Possible calls are not followed."
    )]
    async fn path(
        &self,
        Parameters(params): Parameters<PathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        result((|| {
            let index = self.open()?;
            let from = find(
                &index,
                &params.from,
                params.from_file.as_deref(),
                params.from_line,
                FROM,
            )?;
            let to = find(
                &index,
                &params.to,
                params.to_file.as_deref(),
                params.to_line,
                TO,
            )?;
            let chains = index.paths(from.id, to.id, MAX_CHAINS)?;
            if chains.is_empty() {
                return Ok(format!(
                    "No chain of certain calls goes from {} to {}.",
                    described(&from),
                    described(&to)
                ));
            }
            let mut out = format!(
                "{} of certain calls from {} to {}, shortest first:",
                plural(chains.len(), "chain", "chains"),
                described(&from),
                described(&to)
            );
            for (n, steps) in chains.iter().enumerate() {
                out.push_str(&format!(
                    "\n\nChain {}, {}:",
                    n + 1,
                    plural(steps.len(), "call", "calls")
                ));
                for (i, step) in steps.iter().enumerate() {
                    out.push_str(&format!(
                        "\n{}. {} calls {} ({}), at {}:{}",
                        i + 1,
                        step.caller.qualified(),
                        step.callee.qualified(),
                        step.callee.place(),
                        step.caller.file,
                        step.line
                    ));
                    if step.scip_pending {
                        out.push_str(", by name until the next SCIP run");
                    }
                }
            }
            Ok(out)
        })())
    }

    #[tool(
        description = "Find repeated code in this checkout. The kinds: `exact`, the same code with other whitespace or comments; `near`, a copy with renamed names or a few changed lines; `same-idea`, 2 functions whose summaries have the same meaning. Each group has its kind, a score from 0 to 1, and its functions with their files and lines. Test code and functions under 5 lines are left out, unless you set `tests` or `min_lines`."
    )]
    async fn duplicates(
        &self,
        Parameters(params): Parameters<DuplicatesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        result((|| {
            let limit = params.limit.unwrap_or(DEFAULT_GROUPS);
            if !(1..=MAX_GROUPS).contains(&limit) {
                return Err(ToolError::Limit(MAX_GROUPS));
            }
            let min_lines = params.min_lines.unwrap_or(DEFAULT_MIN_LINES);
            if min_lines == 0 {
                return Err(ToolError::MinLines);
            }
            let options = DuplicateOptions {
                kinds: match params.kind {
                    Some(kind) => vec![kind.into()],
                    None => DuplicateKind::ALL.to_vec(),
                },
                min_lines,
                include_tests: params.tests.unwrap_or(false),
                under: params.under,
                ..DuplicateOptions::default()
            };
            let index = self.open()?;
            Ok(duplicates_text(&index, &options, limit)?)
        })())
    }

    #[tool(
        description = "The map of this checkout: each folder of source and test files, with its summary and its files, each file with its summary. Use it in place of .metis/code-index.md. `under` keeps the folders under one path."
    )]
    async fn module_map(
        &self,
        Parameters(params): Parameters<ModuleMapParams>,
    ) -> Result<CallToolResult, ErrorData> {
        result((|| {
            let index = self.open()?;
            let modules = index.module_map(params.under.as_deref())?;
            let files: usize = modules.iter().map(|m| m.files.len()).sum();
            let mut out = format!(
                "{}, {}.",
                plural(modules.len(), "module", "modules"),
                plural(files, "file", "files")
            );
            if modules
                .iter()
                .all(|m| m.summary.is_none() && m.files.iter().all(|f| f.summary.is_none()))
            {
                out.push_str(" The index has no summaries.");
            }
            for m in &modules {
                out.push_str(&format!("\n\n## {}\n", m.path));
                out.push_str(m.summary.as_deref().unwrap_or("No summary."));
                for f in &m.files {
                    match &f.summary {
                        Some(summary) => out.push_str(&format!(
                            "\n- {}{}: {summary}",
                            f.path,
                            if f.is_test { " (test code)" } else { "" }
                        )),
                        None => out.push_str(&format!(
                            "\n- {} ({}{})",
                            f.path,
                            if f.is_test { "test code, " } else { "" },
                            plural(f.symbols, "symbol", "symbols")
                        )),
                    }
                }
            }
            Ok(out)
        })())
    }
}

// `call_tool` is by hand: the refusal of the arguments comes from the
// extractor as an error, and the agent gets it as a tool error.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodeTools {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let call = ToolCallContext::new(self, request, context);
        match self.tool_router.call(call).await {
            Ok(result) => Ok(result),
            Err(e) => tool_error_of(e),
        }
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("kairos-code", env!("CARGO_PKG_VERSION"))
                    .with_title("Kairos code index"),
            )
            .with_instructions(format!(
                "{} Use these tools before you read files: \
                 `module_map` for the map, `code_search` to find code by what it does, \
                 `symbol` for one symbol, `callers` and `callees` for the call graph, \
                 `path` for how one symbol reaches another, and `duplicates` for repeated \
                 code. `fetch_index` opens a checkout: it takes the nearest index of the \
                 repository from Kairos. Run `kairos index update` after you change code.",
                match self.root() {
                    Some(root) => format!("The code index of the checkout {}.", display(&root)),
                    None => "The server has no open checkout: call `fetch_index` with the \
                             path of the checkout you work on."
                        .to_string(),
                }
            ))
    }
}

/// The most chains that `path` gives (COLLIERY-T-2531).
const MAX_CHAINS: usize = 3;

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(file: &str, line: u32) -> SymbolInfo {
        SymbolInfo {
            id: i64::from(line),
            file: file.into(),
            name: "ContentHash".into(),
            container: None,
            kind: "implementation".into(),
            language: "rust".into(),
            start_line: line,
            end_line: line + 5,
            signature: None,
            is_test: false,
            summary: None,
        }
    }

    #[test]
    fn candidates_in_one_file_ask_for_the_line() {
        let all = [sym("src/document.rs", 126), sym("src/document.rs", 168)];
        assert_eq!(
            ambiguous("ContentHash", &all, SYMBOL).text(),
            "AMBIGUOUS: 2 symbols have the name \"ContentHash\". They are in one file. \
             Give the argument `line` to choose one. Or give a candidate from the list as \
             `symbol`.\ndetails: {\"candidates\":[\"ContentHash (src/document.rs:126)\",\
             \"ContentHash (src/document.rs:168)\"]}"
        );
    }

    #[test]
    fn candidates_in_different_files_ask_for_the_file() {
        let all = [sym("src/a.rs", 1), sym("src/b.rs", 1)];
        assert_eq!(
            ambiguous("ContentHash", &all, FROM).to_string(),
            "2 symbols have the name \"ContentHash\". Give the argument `from_file` to choose \
             one. Or give a candidate from the list as `from`."
        );
        let all = [sym("src/a.rs", 1), sym("src/a.rs", 9), sym("src/b.rs", 1)];
        assert_eq!(
            ambiguous("ContentHash", &all, TO).to_string(),
            "3 symbols have the name \"ContentHash\". Give the arguments `to_file` and \
             `to_line` to choose one. Or give a candidate from the list as `to`."
        );
    }

    #[test]
    fn the_not_found_text_quotes_the_name_and_the_file() {
        let text = |file, line| {
            ToolError::NoSymbol {
                name: "ContentHash".into(),
                place: Place::of(file, line),
            }
            .text()
        };
        assert_eq!(
            text(None, None),
            "NOT_FOUND: No symbol has the name \"ContentHash\"."
        );
        assert_eq!(
            text(Some("src/document.rs"), None),
            "NOT_FOUND: No symbol has the name \"ContentHash\" in the file \"src/document.rs\"."
        );
        assert_eq!(
            text(Some("src/document.rs"), Some(12)),
            "NOT_FOUND: No symbol has the name \"ContentHash\" in the file \"src/document.rs\" \
             at line 12."
        );
        assert_eq!(
            text(None, Some(12)),
            "NOT_FOUND: No symbol has the name \"ContentHash\" at line 12."
        );
    }
}
