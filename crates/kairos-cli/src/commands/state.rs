//! `kairos state` — the live state of a task (KAIROS-T-0506,
//! `PUT /api/tasks/{short_code}/state`): branch, last commit, pushed, PR,
//! next step. The write is a patch (KAIROS-T-0533): an option that is not
//! given keeps its value, `--clear` empties a field.

use clap::Args;

use kairos_client::types::{SetTaskStateRequest, Task, TaskState};

use crate::context::{Common, client, print_json};
use crate::error::CliError;

/// Write the live state of a task: branch, last commit, pushed, PR and
/// next step. The write is a patch: a flag you give is set, a field you
/// name in --clear is cleared, a field you do not name is kept. The state
/// is not a note; `kairos tasks get` shows it.
#[derive(Args, Debug)]
pub struct StateArgs {
    /// The task's short code (e.g. ACME-T-0001)
    pub short_code: String,
    /// The branch of the work
    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,
    /// The last commit (a hash or a short hash)
    #[arg(long, value_name = "COMMIT")]
    pub commit: Option<String>,
    /// The last commit is pushed
    #[arg(long, conflicts_with = "not_pushed")]
    pub pushed: bool,
    /// The last commit is not pushed
    #[arg(long = "not-pushed")]
    pub not_pushed: bool,
    /// The URL of the PR
    #[arg(long = "pr", value_name = "URL")]
    pub pr_url: Option<String>,
    /// What comes next
    #[arg(long = "next", value_name = "TEXT")]
    pub next_step: Option<String>,
    /// Clear a field: branch, commit, pr or next (repeat the flag for more)
    #[arg(long = "clear", value_name = "FIELD", value_parser = ["branch", "commit", "pr", "next"])]
    pub clear: Vec<String>,
    /// The updated_at of the state you read: refuse the write when the
    /// state changed since
    #[arg(long = "expected-updated-at", value_name = "INSTANT")]
    pub expected_updated_at: Option<String>,
    #[command(flatten)]
    pub common: Common,
}

impl StateArgs {
    pub async fn run(self) -> Result<(), CliError> {
        let client = client(&self.common)?;
        let body = self.body();
        let task = client.set_task_state(&self.short_code, &body).await?;
        emit_state(&self.common, &task)
    }

    /// The body of the patch: an option that is not given stays out of the
    /// body, so the server keeps that field. A field in `--clear` is `null`.
    fn body(&self) -> SetTaskStateRequest {
        let field = |value: Option<String>, name: &str| -> Option<Option<String>> {
            if self.clear.iter().any(|c| c == name) {
                Some(None)
            } else {
                value.map(Some)
            }
        };
        SetTaskStateRequest {
            branch: field(self.branch.clone(), "branch"),
            commit: field(self.commit.clone(), "commit"),
            pushed: if self.pushed {
                Some(true)
            } else if self.not_pushed {
                Some(false)
            } else {
                None
            },
            pr_url: field(self.pr_url.clone(), "pr"),
            next_step: field(self.next_step.clone(), "next"),
            expected_updated_at: self.expected_updated_at.clone(),
        }
    }
}

/// `state` output: the state of the task.
pub fn emit_state(common: &Common, task: &Task) -> Result<(), CliError> {
    if common.json {
        return print_json(task);
    }
    println!("{}", state_line(task));
    Ok(())
}

/// The sentence about the state of a task.
pub fn state_line(task: &Task) -> String {
    match &task.state {
        Some(state) => format!("State of {}: {}", task.short_code, state_text(state)),
        None => format!("The task {} has no state.", task.short_code),
    }
}

/// The text of a state: the fields with a value, the next step, and who
/// wrote it when.
pub fn state_text(state: &TaskState) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (&state.branch, &state.commit) {
        (Some(branch), Some(commit)) => parts.push(format!("branch {branch} at {commit}")),
        (Some(branch), None) => parts.push(format!("branch {branch}")),
        (None, Some(commit)) => parts.push(format!("commit {commit}")),
        (None, None) => {}
    }
    if state.branch.is_some() || state.commit.is_some() || state.pushed {
        parts.push(if state.pushed { "pushed" } else { "not pushed" }.to_string());
    }
    if let Some(pr) = &state.pr_url {
        parts.push(format!("PR {pr}"));
    }
    let mut text = parts.join(", ");
    if let Some(next) = &state.next_step {
        if !text.is_empty() {
            text.push_str(". ");
        }
        text.push_str("Next: ");
        text.push_str(next);
    }
    if !text.is_empty() && !text.ends_with('.') {
        text.push('.');
    }
    let day = state.updated_at.get(..10).unwrap_or(&state.updated_at);
    format!("{text} (updated {day} by {})", state.updated_by_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        state: StateArgs,
    }

    fn body_of(args: &[&str]) -> serde_json::Value {
        let cli = Cli::try_parse_from(args).expect("the arguments parse");
        serde_json::to_value(cli.state.body()).expect("the body serializes")
    }

    // KAIROS-T-0663: the CLI sends a patch. A field that you do not name
    // stays out of the body, so the server keeps it.
    #[test]
    fn the_body_is_a_patch() {
        assert_eq!(
            body_of(&["state", "ACME-T-0001", "--next", "Open the PR"]),
            serde_json::json!({"next_step": "Open the PR"})
        );
        assert_eq!(
            body_of(&["state", "ACME-T-0001", "--clear", "pr", "--not-pushed"]),
            serde_json::json!({"pr_url": null, "pushed": false})
        );
    }

    #[test]
    fn the_text_names_each_field() {
        let state = TaskState {
            branch: Some("main".into()),
            commit: Some("abc1234".into()),
            pushed: false,
            pr_url: None,
            next_step: Some("Open the PR".into()),
            updated_at: "2026-10-15T10:00:00+00:00".into(),
            updated_by: "u1".into(),
            updated_by_name: "Robin".into(),
        };
        assert_eq!(
            state_text(&state),
            "branch main at abc1234, not pushed. Next: Open the PR. (updated 2026-10-15 by Robin)"
        );
    }
}
