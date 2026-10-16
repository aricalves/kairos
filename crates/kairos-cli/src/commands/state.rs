//! `kairos state` — the live state of a task (KAIROS-T-0506,
//! `PUT /api/tasks/{short_code}/state`): branch, last commit, pushed, PR,
//! next step. The write replaces the whole state.

use clap::Args;

use kairos_client::types::{SetTaskStateRequest, Task, TaskState};

use crate::context::{Common, client, print_json};
use crate::error::CliError;

/// Write the live state of a task: branch, last commit, pushed, PR and
/// next step. The write replaces the whole state: send each field each
/// time. The state is not a note; `kairos tasks get` shows it.
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
    #[arg(long)]
    pub pushed: bool,
    /// The URL of the PR
    #[arg(long = "pr", value_name = "URL")]
    pub pr_url: Option<String>,
    /// What comes next
    #[arg(long = "next", value_name = "TEXT")]
    pub next_step: Option<String>,
    #[command(flatten)]
    pub common: Common,
}

impl StateArgs {
    pub async fn run(self) -> Result<(), CliError> {
        let client = client(&self.common)?;
        let body = SetTaskStateRequest {
            branch: self.branch,
            commit: self.commit,
            pushed: self.pushed,
            pr_url: self.pr_url,
            next_step: self.next_step,
        };
        let task = client.set_task_state(&self.short_code, &body).await?;
        emit_state(&self.common, &task)
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
