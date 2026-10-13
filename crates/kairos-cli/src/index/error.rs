//! The errors of `kairos index` and of the code tools (COLLIERY-T-1852).
//! The texts follow ASD-STE100: `scripts/ste-check.py --code` reads them.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::error::CliError;

/// An error of a `kairos index` command.
#[derive(Debug, thiserror::Error)]
pub enum IndexCommandError {
    #[error("No index is at {0}. Run `kairos index build` first.")]
    NoIndex(PathBuf),
    #[error(
        "No index that this CLI can update is at {path}, and Kairos gave no base index, because {why}. Run `kairos index --full`."
    )]
    NoStart { path: PathBuf, why: String },
    #[error(
        "{changed} files changed since {commit}, the nearest indexed commit in Kairos. The limit is {limit}, so the CLI built nothing. Rebase on {branch}, or run `kairos index --full`. A full build takes {estimate}."
    )]
    TooFar {
        changed: usize,
        commit: String,
        limit: usize,
        branch: String,
        estimate: String,
    },
    #[error("The folder {0} is not there.")]
    NoRoot(PathBuf),
    #[error("Git did not run in {path}: {message}.")]
    Git { path: PathBuf, message: String },
    #[error("The CLI did not write {path}: {source}.")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Index(#[from] kairos_index::IndexError),
    #[error("The MCP server stopped: {0}.")]
    Server(String),
}

impl From<IndexCommandError> for CliError {
    fn from(e: IndexCommandError) -> Self {
        CliError::Failure(e.to_string())
    }
}

/// An error of a code tool. The agent gets it as a tool error with the text
/// `CODE: message`, and a `details:` line when it has details, as from each
/// tool of the Kairos server.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error(
        "No index is at {0}. Call `fetch_index` with the checkout as `root`, or run `kairos index build` in the checkout."
    )]
    NoIndex(PathBuf),
    #[error(
        "The server has no open checkout. Call `fetch_index` with `root`, the path of a git checkout. Give `repository`, the slug in Kairos, when the `origin` remote of the checkout is not a repository of Kairos."
    )]
    NoCheckout,
    #[error("The folder {0} is not in a git checkout.")]
    NotACheckout(PathBuf),
    #[error(
        "Kairos has no index for the checkout {root}, because {why}. There is no local index either. Register the repository in Kairos, and its builder makes the first index. Or run `kairos index --full` in the checkout."
    )]
    NoBaseIndex { root: PathBuf, why: String },
    #[error("The download of the index of {commit} failed: {why}.")]
    Download { commit: String, why: String },
    #[error(transparent)]
    Command(#[from] IndexCommandError),
    #[error("No symbol has the name {name:?}{place}.")]
    NoSymbol { name: String, place: Place },
    #[error("{count} symbols have the name {name:?}. {choose}")]
    Ambiguous {
        name: String,
        count: usize,
        candidates: Vec<String>,
        choose: Choose,
    },
    #[error(
        "The call has the argument {argument:?}. This tool does not accept that argument. {known}"
    )]
    UnknownArgument {
        argument: String,
        allowed: Vec<String>,
        known: String,
    },
    #[error(
        "The call does not have the argument {argument:?}. This tool must have that argument. The arguments of this tool are: {allowed}."
    )]
    MissingArgument { argument: String, allowed: String },
    #[error("The arguments are not correct for this tool: {0}.")]
    BadArguments(String),
    #[error("The argument `limit` must be from 1 to {0}.")]
    Limit(usize),
    #[error("The argument `min_lines` must be 1 or more.")]
    MinLines,
    #[error(transparent)]
    Index(#[from] kairos_index::IndexError),
}

/// Where the agent looked for a symbol that is not there: the end of the
/// text of [`ToolError::NoSymbol`].
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    Anywhere,
    File(String),
    Line(u32),
    FileAndLine(String, u32),
}

impl Place {
    pub fn of(file: Option<&str>, line: Option<u32>) -> Place {
        match (file, line) {
            (None, None) => Place::Anywhere,
            (Some(file), None) => Place::File(file.to_string()),
            (None, Some(line)) => Place::Line(line),
            (Some(file), Some(line)) => Place::FileAndLine(file.to_string(), line),
        }
    }
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Place::Anywhere => Ok(()),
            Place::File(file) => write!(f, " in the file {file:?}"),
            Place::Line(line) => write!(f, " at line {line}"),
            Place::FileAndLine(file, line) => write!(f, " in the file {file:?} at line {line}"),
        }
    }
}

/// The advice of [`ToolError::Ambiguous`] (KAIROS-T-0417): `file` when the
/// candidates are in different files, `line` when they are in one file.
/// The fields are the names of the arguments of the tool (`path` has
/// `from_file` and `from_line`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Choose {
    #[error(
        "Give the argument `{file}` to choose one. Or give a candidate from the list as `{symbol}`."
    )]
    File {
        symbol: &'static str,
        file: &'static str,
    },
    #[error(
        "They are in one file. Give the argument `{line}` to choose one. Or give a candidate from the list as `{symbol}`."
    )]
    Line {
        symbol: &'static str,
        line: &'static str,
    },
    #[error(
        "Give the arguments `{file}` and `{line}` to choose one. Or give a candidate from the list as `{symbol}`."
    )]
    FileAndLine {
        symbol: &'static str,
        file: &'static str,
        line: &'static str,
    },
}

/// The names of the arguments that choose one symbol in a tool.
#[derive(Debug, Clone, Copy)]
pub struct SymbolArguments {
    pub symbol: &'static str,
    pub file: &'static str,
    pub line: &'static str,
}

impl Choose {
    /// The advice for candidates in `files` different files of `count`.
    pub fn of(files: usize, count: usize, args: SymbolArguments) -> Choose {
        let SymbolArguments { symbol, file, line } = args;
        if files <= 1 {
            Choose::Line { symbol, line }
        } else if files == count {
            Choose::File { symbol, file }
        } else {
            Choose::FileAndLine { symbol, file, line }
        }
    }
}

impl ToolError {
    /// The stable code of the error, as the Kairos server gives it.
    pub fn code(&self) -> &'static str {
        match self {
            ToolError::NoIndex(_) => "NO_INDEX",
            ToolError::NoCheckout => "NO_CHECKOUT",
            ToolError::NotACheckout(_) => "NOT_A_CHECKOUT",
            ToolError::NoBaseIndex { .. } => "NO_BASE_INDEX",
            ToolError::Download { .. } => "DOWNLOAD",
            ToolError::Command(_) => "INDEX",
            ToolError::NoSymbol { .. } => "NOT_FOUND",
            ToolError::Ambiguous { .. } => "AMBIGUOUS",
            ToolError::UnknownArgument { .. }
            | ToolError::MissingArgument { .. }
            | ToolError::BadArguments(_)
            | ToolError::Limit(_)
            | ToolError::MinLines => "VALIDATION",
            ToolError::Index(_) => "INDEX",
        }
    }

    fn details(&self) -> Option<Value> {
        match self {
            ToolError::Ambiguous { candidates, .. } => Some(json!({ "candidates": candidates })),
            ToolError::UnknownArgument {
                argument, allowed, ..
            } => Some(json!({ "argument": argument, "allowed": allowed })),
            ToolError::MissingArgument { argument, .. } => Some(json!({ "argument": argument })),
            _ => None,
        }
    }

    /// The text of the tool error: `CODE: message`, then `details: {...}`.
    pub fn text(&self) -> String {
        let mut text = format!("{}: {self}", self.code());
        if let Some(details) = self.details() {
            text.push_str("\ndetails: ");
            text.push_str(&details.to_string());
        }
        text
    }
}
