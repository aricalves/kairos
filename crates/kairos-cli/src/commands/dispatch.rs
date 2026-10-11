//! `kairos boards dispatch` — the webhook targets, the agent dispatch rules
//! and the delivery log of a board (KAIROS-T-0384, KAIROS-T-0385).

use crate::context::{Common, client, print_json};
use crate::error::CliError;
use crate::table::Table;

use kairos_client::types_dispatch::{
    CreateDispatchRuleRequest, CreateWebhookTargetRequest, DispatchRule, WebhookDeliveriesQuery,
    WebhookDelivery, WebhookTarget,
};

/// The dispatch of a board to agent runners.
#[derive(clap::Subcommand, Debug)]
pub enum DispatchCommand {
    /// Show the dispatch rules and the webhook targets of a board
    List {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        #[command(flatten)]
        common: Common,
    },
    /// Make a webhook target (needs configure_boards). The command shows the
    /// secret this one time
    #[command(name = "add-target")]
    AddTarget {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// A name for people, e.g. "Agent runner"
        #[arg(long)]
        name: String,
        /// The http:// or https:// URL of the receiver
        #[arg(long = "hook-url", value_name = "URL")]
        hook_url: String,
        #[command(flatten)]
        common: Common,
    },
    /// Remove a webhook target, with its rules and its log (needs
    /// configure_boards)
    #[command(name = "remove-target")]
    RemoveTarget {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// The id of the target
        #[arg(value_name = "TARGET")]
        target: String,
        #[command(flatten)]
        common: Common,
    },
    /// Turn a webhook target on again after Kairos turned it off (needs
    /// configure_boards)
    #[command(name = "enable-target")]
    EnableTarget {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// The id of the target
        #[arg(value_name = "TARGET")]
        target: String,
        #[command(flatten)]
        common: Common,
    },
    /// Send a test delivery (the event ping) to a webhook target (needs
    /// configure_boards)
    #[command(name = "test-target")]
    TestTarget {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// The id of the target
        #[arg(value_name = "TARGET")]
        target: String,
        #[command(flatten)]
        common: Common,
    },
    /// Make a dispatch rule: a task that enters COLUMN and matches the
    /// filter sends one signed delivery to the target (needs
    /// configure_boards)
    #[command(name = "add-rule")]
    AddRule {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// Column name or id (UUID)
        #[arg(long)]
        column: String,
        /// The id of a webhook target of the board
        #[arg(long)]
        target: String,
        /// Only tasks of this type: task, bug, tech_debt or support
        #[arg(long = "task-type")]
        task_type: Option<String>,
        /// Only tasks with this metadata value, as SLUG=VALUE (e.g.
        /// priority=high)
        #[arg(long, value_name = "SLUG=VALUE")]
        metadata: Option<String>,
        /// Only tasks of this repository (slug or UUID)
        #[arg(long)]
        repository: Option<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Remove a dispatch rule (needs configure_boards). Its deliveries stay
    /// in the log
    #[command(name = "remove-rule")]
    RemoveRule {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// The id of the rule
        #[arg(value_name = "RULE")]
        rule: String,
        #[command(flatten)]
        common: Common,
    },
    /// Show the delivery log of a board, newest first, with each try
    Log {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// Only the deliveries of this target (id)
        #[arg(long)]
        target: Option<String>,
        /// Only the deliveries of this rule (id)
        #[arg(long)]
        rule: Option<String>,
        /// The newest deliveries to show, 1 to 200 (50)
        #[arg(long)]
        limit: Option<i64>,
        #[command(flatten)]
        common: Common,
    },
}

/// `SLUG=VALUE` of `--metadata`. Pure.
pub fn split_metadata(raw: &str) -> Result<(String, String), CliError> {
    match raw.split_once('=') {
        Some((slug, value)) if !slug.trim().is_empty() && !value.trim().is_empty() => {
            Ok((slug.trim().to_string(), value.trim().to_string()))
        }
        _ => Err(CliError::Failure(format!(
            "--metadata {raw:?} is not SLUG=VALUE. Send for example --metadata priority=high."
        ))),
    }
}

/// The filter of a rule in words. Pure.
fn filter_text(rule: &DispatchRule) -> String {
    let mut parts = Vec::new();
    if let Some(t) = &rule.task_type {
        parts.push(format!("type {t}"));
    }
    if let (Some(slug), Some(value)) = (&rule.metadata_slug, &rule.metadata_value) {
        parts.push(format!("{slug}={value}"));
    }
    if let Some(repo) = &rule.repository {
        parts.push(format!("repository {repo}"));
    }
    if parts.is_empty() {
        "each task".to_string()
    } else {
        parts.join(", ")
    }
}

fn target_state(target: &WebhookTarget) -> String {
    if target.enabled {
        format!("on ({} failed in a row)", target.consecutive_failures)
    } else {
        "off".to_string()
    }
}

fn delivery_rows(deliveries: &[WebhookDelivery]) -> Table {
    let mut table = Table::new(&[
        "CREATED", "DELIVERY", "EVENT", "TASK", "STATUS", "TRIES", "CODE", "ERROR",
    ]);
    for d in deliveries {
        table.row(vec![
            d.created_at.clone(),
            d.id.clone(),
            d.event.clone(),
            d.short_code.clone().unwrap_or_else(|| "-".into()),
            d.status.clone(),
            d.tries.to_string(),
            d.response_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            d.last_error.clone().unwrap_or_default(),
        ]);
    }
    table
}

impl DispatchCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { board, common } => {
                let client = client(&common)?;
                let rules = client.list_dispatch_rules(&board).await?;
                let targets = client.list_webhook_targets(&board).await?;
                if common.json {
                    return print_json(&serde_json::json!({
                        "rules": rules,
                        "targets": targets,
                    }));
                }
                let mut table = Table::new(&["RULE", "COLUMN", "FILTER", "TARGET"]);
                for rule in &rules {
                    table.row(vec![
                        rule.id.clone(),
                        rule.column.clone(),
                        filter_text(rule),
                        rule.target.clone(),
                    ]);
                }
                print!("{}", table.render());
                println!();
                let mut table = Table::new(&["TARGET", "NAME", "URL", "STATE"]);
                for target in &targets {
                    table.row(vec![
                        target.id.clone(),
                        target.name.clone(),
                        target.url.clone(),
                        target_state(target),
                    ]);
                }
                print!("{}", table.render());
                Ok(())
            }
            Self::AddTarget {
                board,
                name,
                hook_url,
                common,
            } => {
                let client = client(&common)?;
                let created = client
                    .create_webhook_target(
                        &board,
                        &CreateWebhookTargetRequest {
                            name,
                            url: hook_url,
                        },
                    )
                    .await?;
                if common.json {
                    return print_json(&created);
                }
                println!(
                    "Made the webhook target {} ({}) for {}.",
                    created.target.name, created.target.id, created.target.url
                );
                println!("secret: {}", created.secret);
                println!(
                    "Kairos shows the secret this one time. Keep it in the receiver: \
                     X-Kairos-Signature is sha256=<hex HMAC-SHA256 of the body with this secret>."
                );
                Ok(())
            }
            Self::RemoveTarget {
                board,
                target,
                common,
            } => {
                let client = client(&common)?;
                let removed = client.delete_webhook_target(&board, &target).await?;
                if common.json {
                    return print_json(&removed);
                }
                println!(
                    "Removed the webhook target {} ({}), with its rules and its log.",
                    removed.name, removed.id
                );
                Ok(())
            }
            Self::EnableTarget {
                board,
                target,
                common,
            } => {
                let client = client(&common)?;
                let target = client.enable_webhook_target(&board, &target).await?;
                if common.json {
                    return print_json(&target);
                }
                println!("The webhook target {} ({}) is on.", target.name, target.id);
                Ok(())
            }
            Self::TestTarget {
                board,
                target,
                common,
            } => {
                let client = client(&common)?;
                let delivery = client.test_webhook_target(&board, &target).await?;
                if common.json {
                    return print_json(&delivery);
                }
                println!(
                    "Queued the test delivery {} ({}). `kairos boards dispatch log {board}` \
                     shows its result.",
                    delivery.id, delivery.status
                );
                Ok(())
            }
            Self::AddRule {
                board,
                column,
                target,
                task_type,
                metadata,
                repository,
                common,
            } => {
                let (metadata_slug, metadata_value) = match metadata.as_deref() {
                    Some(raw) => {
                        let (slug, value) = split_metadata(raw)?;
                        (Some(slug), Some(value))
                    }
                    None => (None, None),
                };
                let client = client(&common)?;
                let rule = client
                    .create_dispatch_rule(
                        &board,
                        &CreateDispatchRuleRequest {
                            column,
                            target_id: target,
                            task_type,
                            metadata_slug,
                            metadata_value,
                            repository,
                        },
                    )
                    .await?;
                if common.json {
                    return print_json(&rule);
                }
                println!(
                    "Made the dispatch rule {}: {} that enters {} goes to {}.",
                    rule.id,
                    filter_text(&rule),
                    rule.column,
                    rule.target
                );
                Ok(())
            }
            Self::RemoveRule {
                board,
                rule,
                common,
            } => {
                let client = client(&common)?;
                let removed = client.delete_dispatch_rule(&board, &rule).await?;
                if common.json {
                    return print_json(&removed);
                }
                println!(
                    "Removed the dispatch rule {} (column {}).",
                    removed.id, removed.column
                );
                Ok(())
            }
            Self::Log {
                board,
                target,
                rule,
                limit,
                common,
            } => {
                let client = client(&common)?;
                let deliveries = client
                    .list_webhook_deliveries(
                        &board,
                        &WebhookDeliveriesQuery {
                            target_id: target,
                            rule_id: rule,
                            limit,
                        },
                    )
                    .await?;
                if common.json {
                    return print_json(&deliveries);
                }
                print!("{}", delivery_rows(&deliveries).render());
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_is_slug_equals_value() {
        assert_eq!(
            split_metadata("priority=high").unwrap(),
            ("priority".to_string(), "high".to_string())
        );
        assert_eq!(
            split_metadata(" area = web ").unwrap(),
            ("area".to_string(), "web".to_string())
        );
        for bad in ["priority", "=high", "priority="] {
            assert!(split_metadata(bad).is_err(), "{bad}");
        }
    }
}
