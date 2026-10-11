//! The agent dispatch section of the board settings (KAIROS-T-0384,
//! KAIROS-T-0385): the webhook targets (make one and see its secret once,
//! turn one on again, send a test, remove), the dispatch rules (make,
//! remove), and the delivery log with each try.
//!
//! The server decides the rights: a board manager writes, a person who
//! belongs to the board reads. A refusal shows in the notice of the page.

use aurora_dark::components::{
    Alert, Button, Code, CopyButton, Divider, ErrorState, Group, Loading, Panel, Pill, Select,
    Stack, Table, Text, TextInput,
};
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MutationOutcome, run_mutation};
use crate::api::{delete_json, get_json, post_empty, post_json};
use crate::auth::{Auth, use_auth};

/// mirror of: `kairos_client::types_dispatch::WebhookTarget` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub consecutive_failures: i32,
    pub disabled_reason: Option<String>,
}

/// mirror of: `kairos_client::types_dispatch::CreatedWebhookTarget`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct Created {
    target: Target,
    secret: String,
}

/// mirror of: `kairos_client::types_dispatch::DispatchRule` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Rule {
    pub id: String,
    pub column: String,
    pub target: String,
    pub task_type: Option<String>,
    pub metadata_slug: Option<String>,
    pub metadata_value: Option<String>,
    pub repository: Option<String>,
}

/// mirror of: `kairos_client::types_dispatch::WebhookDeliveryTry` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Try {
    pub try_number: i32,
    pub at: String,
    pub ok: bool,
    pub response_code: Option<i32>,
    pub error: Option<String>,
}

/// mirror of: `kairos_client::types_dispatch::WebhookDelivery` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Delivery {
    pub id: String,
    pub event: String,
    pub short_code: Option<String>,
    pub status: String,
    pub tries: i32,
    pub response_code: Option<i32>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub log: Vec<Try>,
}

/// The task types of a rule; the first is no filter.
const TASK_TYPES: [&str; 5] = ["any type", "task", "bug", "tech_debt", "support"];

/// The filter of a rule in words. Pure, host-tested.
fn filter_words(rule: &Rule) -> String {
    let mut parts = Vec::new();
    if let Some(t) = &rule.task_type {
        parts.push(format!("type {t}"));
    }
    if let (Some(slug), Some(value)) = (&rule.metadata_slug, &rule.metadata_value) {
        parts.push(format!("{slug} = {value}"));
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

/// The body of a new rule. `metadata` is `slug=value` or empty. Pure,
/// host-tested.
fn rule_body(
    column: &str,
    target_id: &str,
    task_type: &str,
    metadata: &str,
    repository: &str,
) -> Result<Value, String> {
    let mut body = json!({ "column": column, "target_id": target_id });
    if task_type != TASK_TYPES[0] && !task_type.is_empty() {
        body["task_type"] = json!(task_type);
    }
    let metadata = metadata.trim();
    if !metadata.is_empty() {
        match metadata.split_once('=') {
            Some((slug, value)) if !slug.trim().is_empty() && !value.trim().is_empty() => {
                body["metadata_slug"] = json!(slug.trim());
                body["metadata_value"] = json!(value.trim());
            }
            _ => {
                return Err(format!(
                    "The metadata filter {metadata:?} is not slug=value. Write for example \
                     priority=high."
                ));
            }
        }
    }
    if !repository.trim().is_empty() {
        body["repository"] = json!(repository.trim());
    }
    Ok(body)
}

/// The color of a status. Pure.
fn status_color(status: &str) -> &'static str {
    match status {
        "delivered" => token::OK,
        "failed" => token::BAD,
        "pending" => token::GOLD,
        _ => token::MUTED,
    }
}

async fn targets(auth: Auth, board: &str) -> Result<Vec<Target>, ApiError> {
    get_json(auth, &format!("/api/boards/{board}/webhook-targets")).await
}

async fn rules(auth: Auth, board: &str) -> Result<Vec<Rule>, ApiError> {
    get_json(auth, &format!("/api/boards/{board}/dispatch-rules")).await
}

async fn deliveries(auth: Auth, board: &str) -> Result<Vec<Delivery>, ApiError> {
    get_json(
        auth,
        &format!("/api/boards/{board}/webhook-deliveries?limit=50"),
    )
    .await
}

/// The data of the section, in one load.
#[derive(Clone, Debug, PartialEq)]
struct Data {
    targets: Vec<Target>,
    rules: Vec<Rule>,
    log: Vec<Delivery>,
}

async fn load(auth: Auth, board: String) -> Result<Data, ApiError> {
    Ok(Data {
        targets: targets(auth, &board).await?,
        rules: rules(auth, &board).await?,
        log: deliveries(auth, &board).await?,
    })
}

/// The agent dispatch section of a board.
///
/// Its changes reload only this section, with a reload signal of its own
/// (KAIROS-T-0387). A reload of the page makes a new section, and the
/// secret of a new target, which shows one time, went with the old one.
#[component]
pub fn DispatchPanel(
    board_id: String,
    column_names: Vec<String>,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
) -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let board = StoredValue::new(board_id);
    let data = LocalResource::new(move || {
        let _ = auth.token();
        reload.get();
        load(auth, board.get_value())
    });
    // The secret of the target made last. It shows once.
    let made: RwSignal<Option<(String, String)>> = RwSignal::new(None);

    let target_name = RwSignal::new(String::new());
    let target_url = RwSignal::new(String::new());
    let on_add_target = move |_| {
        if busy.get_untracked() {
            return;
        }
        let board_id = board.get_value();
        let body = json!({
            "name": target_name.get_untracked().trim(),
            "url": target_url.get_untracked().trim(),
        });
        busy.set(true);
        made.set(None);
        leptos::task::spawn_local(async move {
            let result: Result<Created, ApiError> = post_json(
                auth,
                &format!("/api/boards/{board_id}/webhook-targets"),
                &body,
            )
            .await;
            busy.set(false);
            match result {
                Ok(created) => {
                    made.set(Some((created.target.name, created.secret)));
                    outcome.set(None);
                    target_name.set(String::new());
                    target_url.set(String::new());
                    reload.update(|n| *n += 1);
                }
                Err(error) => outcome.set(Some(Err(error))),
            }
        });
    };

    let rule_column = RwSignal::new(column_names.first().cloned().unwrap_or_default());
    let column_names = StoredValue::new(column_names);
    let rule_target = RwSignal::new(String::new());
    let rule_type = RwSignal::new(TASK_TYPES[0].to_string());
    let rule_metadata = RwSignal::new(String::new());
    let rule_repository = RwSignal::new(String::new());

    view! {
        <Panel title="Agent dispatch"
            caption="When a task enters the column of a rule and matches its filter, Kairos \
                     sends one signed delivery to the target of the rule. The runner of the \
                     target starts the agent.">
            <div data-testid="dispatch-panel">
            <Stack gap="sm">
                {move || made.get().map(|(name, secret)| {
                    let copied = secret.clone();
                    view! {
                        <Alert title=format!("The secret of {name}") color=token::OK>
                            <Stack gap="xs">
                                <Text size="sm">
                                    "Copy the secret now. Kairos does not show it again. The \
                                     runner uses it to check X-Kairos-Signature."
                                </Text>
                                <Group gap="sm" wrap=true>
                                    <Code>{secret}</Code>
                                    <CopyButton value=copied label="Copy secret"/>
                                </Group>
                                <Group justify="end">
                                    <Button variant="subtle" size="xs"
                                        on_click=Callback::new(move |_| made.set(None))>
                                        "Hide the secret"
                                    </Button>
                                </Group>
                            </Stack>
                        </Alert>
                    }
                })}
                {move || match data.get() {
                    None => view! { <Loading label="Loading the dispatch…"/> }.into_any(),
                    Some(Err(error)) => view! {
                        <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                    }.into_any(),
                    Some(Ok(data)) => {
                        let target_names: Vec<String> =
                            data.targets.iter().map(|t| t.name.clone()).collect();
                        if rule_target.get_untracked().is_empty()
                            || !target_names.contains(&rule_target.get_untracked())
                        {
                            rule_target.set(target_names.first().cloned().unwrap_or_default());
                        }
                        let ids = StoredValue::new(
                            data.targets
                                .iter()
                                .map(|t| (t.name.clone(), t.id.clone()))
                                .collect::<Vec<_>>(),
                        );
                        let on_add_rule = move |_| {
                            let board_id = board.get_value();
                            let target_id = ids
                                .get_value()
                                .iter()
                                .find(|(name, _)| *name == rule_target.get_untracked())
                                .map(|(_, id)| id.clone())
                                .unwrap_or_default();
                            let body = match rule_body(
                                &rule_column.get_untracked(),
                                &target_id,
                                &rule_type.get_untracked(),
                                &rule_metadata.get_untracked(),
                                &rule_repository.get_untracked(),
                            ) {
                                Ok(body) => body,
                                Err(message) => {
                                    outcome.set(Some(Err(ApiError::Unknown(message))));
                                    return;
                                }
                            };
                            run_mutation(
                                busy,
                                outcome,
                                reload,
                                "Kairos made the dispatch rule.".to_string(),
                                async move {
                                    post_json::<_, Value>(
                                        auth,
                                        &format!("/api/boards/{board_id}/dispatch-rules"),
                                        &body,
                                    )
                                    .await
                                    .map(|_| ())
                                },
                            );
                        };
                        view! {
                            <TargetRows targets=data.targets.clone() board busy outcome reload/>
                            <Group align="end" gap="sm" wrap=true>
                                <TextInput label="Target name" value=target_name
                                    placeholder="e.g. Agent runner"/>
                                <TextInput label="URL" value=target_url
                                    placeholder="https://runner.example.com/kairos"/>
                                <Button on_click=Callback::new(on_add_target)>"Add target"</Button>
                            </Group>
                            <Divider/>
                            <RuleRows rules=data.rules.clone() board busy outcome reload/>
                            <Group align="end" gap="sm" wrap=true>
                                <Select label="Column" options=column_names.get_value() value=rule_column/>
                                <Select label="Target" options=target_names value=rule_target/>
                                <Select label="Task type"
                                    options=TASK_TYPES.iter().map(|t| t.to_string()).collect()
                                    value=rule_type/>
                                <TextInput label="Metadata (slug=value)" value=rule_metadata
                                    placeholder="priority=high"/>
                                <TextInput label="Repository" value=rule_repository
                                    placeholder="slug"/>
                                <Button on_click=Callback::new(on_add_rule)>"Add rule"</Button>
                            </Group>
                            <Divider/>
                            <DeliveryLog log=data.log.clone() reload/>
                        }.into_any()
                    }
                }}
            </Stack>
            </div>
        </Panel>
    }
}

#[component]
fn TargetRows(
    targets: Vec<Target>,
    board: StoredValue<String>,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    if targets.is_empty() {
        return view! {
            <Text size="sm" dimmed=true>"The board has no webhook target."</Text>
        }
        .into_any();
    }
    targets
        .into_iter()
        .map(|target| {
            let id = StoredValue::new(target.id.clone());
            let name = target.name.clone();
            let action = move |path: &'static str, done: String| {
                let board_id = board.get_value();
                let target_id = id.get_value();
                run_mutation(busy, outcome, reload, done, async move {
                    let url = format!("/api/boards/{board_id}/webhook-targets/{target_id}{path}");
                    if path.is_empty() {
                        delete_json::<Value>(auth, &url).await.map(|_| ())
                    } else {
                        post_empty::<Value>(auth, &url).await.map(|_| ())
                    }
                });
            };
            let (on_test, on_enable, on_remove) = {
                let (a, b, c) = (name.clone(), name.clone(), name.clone());
                (
                    move |_| action("/test", format!("Kairos queued a test delivery to {a}.")),
                    move |_| action("/enable", format!("Kairos turned on {b}.")),
                    move |_| action("", format!("Kairos removed {c}, with its rules.")),
                )
            };
            let state = if target.enabled {
                view! { <Pill color=token::OK>"on"</Pill> }.into_any()
            } else {
                view! { <Pill color=token::BAD>"off"</Pill> }.into_any()
            };
            view! {
                <Group justify="between" wrap=true>
                    <Group gap="sm" wrap=true>
                        <Text bright=true size="sm">{target.name.clone()}</Text>
                        {state}
                        <Text mono=true size="xs" dimmed=true>{target.url.clone()}</Text>
                        <Text size="xs" dimmed=true>
                            {format!("{} failed in a row", target.consecutive_failures)}
                        </Text>
                        {target.disabled_reason.clone().map(|reason| view! {
                            <Text size="xs">{reason}</Text>
                        })}
                    </Group>
                    <Group gap="xs">
                        <Button variant="default" size="xs" on_click=Callback::new(on_test)>
                            "Send test"
                        </Button>
                        {(!target.enabled).then(|| view! {
                            <Button variant="default" size="xs" on_click=Callback::new(on_enable)>
                                "Turn on"
                            </Button>
                        })}
                        <Button variant="default" size="xs" bad=true
                            on_click=Callback::new(on_remove)>
                            "Remove"
                        </Button>
                    </Group>
                </Group>
            }
            .into_any()
        })
        .collect_view()
        .into_any()
}

#[component]
fn RuleRows(
    rules: Vec<Rule>,
    board: StoredValue<String>,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    if rules.is_empty() {
        return view! {
            <Text size="sm" dimmed=true>"The board has no dispatch rule."</Text>
        }
        .into_any();
    }
    rules
        .into_iter()
        .map(|rule| {
            let id = StoredValue::new(rule.id.clone());
            let on_remove = move |_| {
                let board_id = board.get_value();
                let rule_id = id.get_value();
                run_mutation(
                    busy,
                    outcome,
                    reload,
                    "Kairos removed the dispatch rule.".to_string(),
                    async move {
                        delete_json::<Value>(
                            auth,
                            &format!("/api/boards/{board_id}/dispatch-rules/{rule_id}"),
                        )
                        .await
                        .map(|_| ())
                    },
                );
            };
            view! {
                <Group justify="between" wrap=true>
                    <Text size="sm" attr:data-testid="dispatch-rule">
                        {format!(
                            "{} that enters {} goes to {}",
                            filter_words(&rule),
                            rule.column,
                            rule.target
                        )}
                    </Text>
                    <Button variant="default" size="xs" bad=true
                        on_click=Callback::new(on_remove)>
                        "Remove"
                    </Button>
                </Group>
            }
        })
        .collect_view()
        .into_any()
}

#[component]
fn DeliveryLog(log: Vec<Delivery>, reload: RwSignal<u32>) -> impl IntoView {
    let empty = log.is_empty();
    let rows = log
        .into_iter()
        .map(|d| {
            let tries = d
                .log
                .iter()
                .map(|t| {
                    format!(
                        "#{} {} {}{}",
                        t.try_number,
                        t.at,
                        if t.ok { "ok" } else { "failed" },
                        t.response_code
                            .map(|c| format!(" ({c})"))
                            .or_else(|| t.error.clone().map(|e| format!(" ({e})")))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            view! {
                <tr>
                    <td>{d.created_at.clone()}</td>
                    <td>{d.event.clone()}</td>
                    <td>{d.short_code.clone().unwrap_or_else(|| "-".into())}</td>
                    <td><Pill color=status_color(&d.status)>{d.status.clone()}</Pill></td>
                    <td class="cl-num">{d.tries}</td>
                    <td class="cl-num">{d.response_code.map(|c| c.to_string()).unwrap_or_default()}</td>
                    <td>{d.last_error.clone().unwrap_or_default()}</td>
                    <td title=tries.clone()>{tries.clone()}</td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <div data-testid="delivery-log">
            <Group justify="between">
                <Text bright=true bold=true size="sm">"Delivery log, newest first"</Text>
                <Button variant="subtle" size="xs"
                    on_click=Callback::new(move |_| reload.update(|n| *n += 1))>
                    "Refresh"
                </Button>
            </Group>
            {empty.then(|| view! { <Text size="sm" dimmed=true>"No delivery yet."</Text> })}
            <Table label="Delivery log">
                <thead><tr><th>"Time"</th><th>"Event"</th><th>"Task"</th><th>"Status"</th>
                    <th class="cl-num">"Tries"</th><th class="cl-num">"Code"</th>
                    <th>"Error"</th><th>"Each try"</th></tr></thead>
                <tbody>{rows}</tbody>
            </Table>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Rule {
        Rule {
            id: "r".into(),
            column: "Todo".into(),
            target: "Runner".into(),
            task_type: None,
            metadata_slug: None,
            metadata_value: None,
            repository: None,
        }
    }

    #[test]
    fn the_filter_reads_in_words() {
        assert_eq!(filter_words(&rule()), "each task");
        let r = Rule {
            task_type: Some("bug".into()),
            metadata_slug: Some("priority".into()),
            metadata_value: Some("high".into()),
            repository: Some("site".into()),
            ..rule()
        };
        assert_eq!(
            filter_words(&r),
            "type bug, priority = high, repository site"
        );
    }

    #[test]
    fn the_body_of_a_rule_has_only_the_parts_that_are_set() {
        assert_eq!(
            rule_body("Todo", "t1", "any type", "", "").unwrap(),
            json!({ "column": "Todo", "target_id": "t1" })
        );
        assert_eq!(
            rule_body("Todo", "t1", "bug", " priority = high ", "site").unwrap(),
            json!({
                "column": "Todo", "target_id": "t1", "task_type": "bug",
                "metadata_slug": "priority", "metadata_value": "high", "repository": "site",
            })
        );
        assert!(rule_body("Todo", "t1", "bug", "priority", "").is_err());
    }

    #[test]
    fn each_status_has_a_color() {
        assert_eq!(status_color("delivered"), token::OK);
        assert_eq!(status_color("failed"), token::BAD);
        assert_eq!(status_color("skipped"), token::MUTED);
    }
}
