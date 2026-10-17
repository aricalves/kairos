//! The notifications of the signed-in person in the GUI (KAIROS-T-0383):
//!
//! - [`NotificationBell`]: a bell in the top bar of the shell with the
//!   unread count. The count changes live: the bell listens on
//!   `/ws/events` for the events of the person (`notification_created`,
//!   `notifications_read`), which the server sends to the sockets of that
//!   person only. A click opens a drawer with the list.
//! - `/notifications` ([`NotificationsPage`]): the list, and the settings
//!   that turn a kind off.
//!
//! Each row links to its item and has "Mark read". "Mark all read" marks
//! each unread one. A notification marked read here is read for the agent
//! of the person too: it is one record on the server.

use aurora_dark::components::{
    Anchor, Button, Divider, Empty, ErrorState, Group, Loading, PageHeader, Panel, Pill, Stack,
    Switch, Text,
};
use aurora_dark::frame::Drawer;
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::Deserialize;
use serde_json::json;

use super::admin::{MutationNotice, MutationOutcome, run_mutation};
use crate::auth::{Auth, use_auth};

/// mirror of: `kairos_client::types_notifications::Notification`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Notification {
    pub id: String,
    pub kind: String,
    pub short_code: Option<String>,
    pub text: String,
    pub created_at: String,
    pub read_at: Option<String>,
}

/// mirror of: `kairos_client::types_notifications::NotificationList`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct NotificationList {
    pub unread: i64,
    pub items: Vec<Notification>,
}

/// mirror of: `kairos_client::types_notifications::NotificationSettings`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct NotificationSettings {
    hand_off: bool,
    blocker_cleared: bool,
    blocked_again: bool,
    cancelled: bool,
    blocker_cancelled: bool,
    mention: bool,
    p0_bug: bool,
    delivery_failing: bool,
    agent_key_revoked: bool,
}

/// mirror of: `kairos_client::types_notifications::MarkNotificationsReadResponse`
/// (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct Marked {
    unread: i64,
}

const LIST: &str = "/api/me/notifications";
const READ: &str = "/api/me/notifications/read";
const SETTINGS: &str = "/api/me/notification-settings";

async fn list(auth: Auth) -> Result<NotificationList, ApiError> {
    crate::api::get_json(auth, LIST).await
}

/// The body that marks one notification, or all. Pure, host-tested.
fn mark_body(id: Option<&str>) -> serde_json::Value {
    match id {
        Some(id) => json!({ "ids": [id] }),
        None => json!({ "all": true }),
    }
}

async fn mark(auth: Auth, id: Option<&str>) -> Result<Marked, ApiError> {
    crate::api::post_json(auth, READ, &mark_body(id)).await
}

/// The words of a kind. Pure, host-tested.
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "hand_off" => "Hand-off",
        "blocker_cleared" => "Blocker cleared",
        "blocked_again" => "Blocked again",
        "cancelled" => "Cancelled",
        "blocker_cancelled" => "Blocker cancelled",
        "mention" => "Mention",
        "p0_bug" => "P0 bug",
        "delivery_failing" => "Delivery failing",
        "agent_key_revoked" => "Agent key revoked",
        _ => "Notification",
    }
}

/// `2026-10-12T09:15:00Z` → `2026-10-12 09:15 UTC`. Pure, host-tested.
fn when(value: &str) -> String {
    match value.split_once('T') {
        Some((date, time)) => format!("{date} {} UTC", time.get(..5).unwrap_or(time)),
        None => value.to_string(),
    }
}

/// The text of the count of the bell: empty for 0, `99+` above 99. Pure,
/// host-tested.
fn count_text(unread: i64) -> String {
    match unread {
        n if n <= 0 => String::new(),
        n if n > 99 => "99+".to_string(),
        n => n.to_string(),
    }
}

/// A bell, drawn with the token colors.
#[component]
fn BellIcon() -> impl IntoView {
    view! {
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path
                d="M6 16 V11 a6 6 0 0 1 12 0 V16 l1.5 2 H4.5 Z"
                style="stroke:var(--ice);"
                stroke-width="1.8"
                stroke-linejoin="round"
            />
            <path d="M10 20.5 a2 2 0 0 0 4 0" style="stroke:var(--ice);" stroke-width="1.8"/>
        </svg>
    }
}

/// The rows of a list of notifications, with "Mark read" on each unread
/// row. `on_marked` runs after a mark, with the error when it failed.
#[component]
fn NotificationRows(
    items: Vec<Notification>,
    busy: RwSignal<bool>,
    on_marked: Callback<Result<(), ApiError>>,
) -> impl IntoView {
    let auth = use_auth();
    if items.is_empty() {
        return view! { <Empty message="You have no notifications."/> }.into_any();
    }
    items
        .into_iter()
        .map(|n| {
            let id = StoredValue::new(n.id.clone());
            let unread = n.read_at.is_none();
            let on_mark = move |_| {
                if busy.get_untracked() {
                    return;
                }
                busy.set(true);
                let id = id.get_value();
                leptos::task::spawn_local(async move {
                    let result = mark(auth, Some(&id)).await.map(|_| ());
                    busy.set(false);
                    on_marked.run(result);
                });
            };
            view! {
                <div data-testid="notification" data-unread=unread.to_string()>
                    <Stack gap="xs">
                        <Group justify="between" wrap=true>
                            <Group gap="sm" wrap=true>
                                <Pill color=if unread { token::GOLD } else { token::MUTED }>
                                    {kind_label(&n.kind)}
                                </Pill>
                                {n.short_code.clone().map(|code| view! {
                                    <Anchor href=format!("/items/{code}")>{code.clone()}</Anchor>
                                })}
                                <Text dimmed=true size="sm">{when(&n.created_at)}</Text>
                            </Group>
                            {unread.then(|| view! {
                                <Button variant="default" size="xs"
                                    on_click=Callback::new(on_mark)>
                                    "Mark read"
                                </Button>
                            })}
                        </Group>
                        <Text size="sm" bright=unread>{n.text.clone()}</Text>
                        <Divider/>
                    </Stack>
                </div>
            }
        })
        .collect_view()
        .into_any()
}

/// The bell of the shell: the unread count, live, and a drawer with the
/// list.
#[component]
pub fn NotificationBell() -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let open = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error: RwSignal<Option<ApiError>> = RwSignal::new(None);
    let notifications = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        list(auth)
    });

    // The count changes live: each event of the person reads the list
    // again, and so does each reconnect.
    let guard = super::boards::live::subscribe_person_events(auth, move || {
        reload.try_update(|n| *n += 1);
    });
    let live_guard: StoredValue<Option<super::boards::live::LiveBoardGuard>, LocalStorage> =
        StoredValue::new_local(Some(guard));
    on_cleanup(move || live_guard.set_value(None));

    let unread = Signal::derive(move || match notifications.get() {
        Some(Ok(list)) => list.unread,
        _ => 0,
    });
    let on_marked = Callback::new(move |result: Result<(), ApiError>| {
        error.set(result.err());
        reload.update(|n| *n += 1);
    });
    let on_mark_all = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        leptos::task::spawn_local(async move {
            let result = mark(auth, None).await.map(|_| ());
            busy.set(false);
            on_marked.run(result);
        });
    };

    view! {
        <span data-testid="notification-bell">
            <Button
                variant="subtle"
                size="xs"
                aria_label="Notifications"
                title="Notifications"
                on_click=Callback::new(move |_| open.set(true))
            >
                <Group gap="xs">
                    <BellIcon/>
                    {move || {
                        let text = count_text(unread.get());
                        (!text.is_empty()).then(|| view! {
                            <span data-testid="notification-count">
                                <Pill color=token::GOLD>{text}</Pill>
                            </span>
                        })
                    }}
                </Group>
            </Button>
        </span>
        <Drawer open=open title="Notifications">
            <div data-testid="notification-drawer">
                <Stack gap="sm">
                    <Group justify="between" wrap=true>
                        <Text size="sm" dimmed=true>
                            {move || format!("{} unread", unread.get())}
                        </Text>
                        <Group gap="sm">
                            <Button variant="default" size="xs"
                                disabled=Signal::derive(move || unread.get() == 0)
                                on_click=Callback::new(on_mark_all)>
                                "Mark all read"
                            </Button>
                            <Button variant="subtle" size="xs" href="/notifications"
                                on_click=Callback::new(move |_| open.set(false))>
                                "Settings"
                            </Button>
                        </Group>
                    </Group>
                    {move || error.get().map(|error| view! { <ErrorState error/> })}
                    {move || match notifications.get() {
                        None => view! { <Loading/> }.into_any(),
                        Some(Err(error)) => view! {
                            <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                        }.into_any(),
                        Some(Ok(list)) => view! {
                            <NotificationRows items=list.items busy on_marked/>
                        }.into_any(),
                    }}
                </Stack>
            </div>
        </Drawer>
    }
}

/// `/notifications`: the list, and the settings of the kinds.
#[component]
pub fn NotificationsPage() -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let busy = RwSignal::new(false);
    let outcome: RwSignal<MutationOutcome> = RwSignal::new(None);
    let notifications = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        list(auth)
    });
    let guard = super::boards::live::subscribe_person_events(auth, move || {
        reload.try_update(|n| *n += 1);
    });
    let live_guard: StoredValue<Option<super::boards::live::LiveBoardGuard>, LocalStorage> =
        StoredValue::new_local(Some(guard));
    on_cleanup(move || live_guard.set_value(None));

    // The switches of the settings, filled when the settings arrive.
    let hand_off = RwSignal::new(true);
    let blocker_cleared = RwSignal::new(true);
    let blocked_again = RwSignal::new(true);
    let cancelled = RwSignal::new(true);
    let blocker_cancelled = RwSignal::new(true);
    let mention = RwSignal::new(true);
    let p0_bug = RwSignal::new(true);
    let delivery_failing = RwSignal::new(true);
    let agent_key_revoked = RwSignal::new(true);
    let settings_reload = RwSignal::new(0u32);
    let settings = LocalResource::new(move || {
        auth.track_session();
        settings_reload.get();
        async move { crate::api::get_json::<NotificationSettings>(auth, SETTINGS).await }
    });
    Effect::new(move |_| {
        if let Some(Ok(s)) = settings.get() {
            hand_off.set(s.hand_off);
            blocker_cleared.set(s.blocker_cleared);
            blocked_again.set(s.blocked_again);
            cancelled.set(s.cancelled);
            blocker_cancelled.set(s.blocker_cancelled);
            mention.set(s.mention);
            p0_bug.set(s.p0_bug);
            delivery_failing.set(s.delivery_failing);
            agent_key_revoked.set(s.agent_key_revoked);
        }
    });
    let on_save = move |_| {
        let body = json!({
            "hand_off": hand_off.get_untracked(),
            "blocker_cleared": blocker_cleared.get_untracked(),
            "blocked_again": blocked_again.get_untracked(),
            "cancelled": cancelled.get_untracked(),
            "blocker_cancelled": blocker_cancelled.get_untracked(),
            "mention": mention.get_untracked(),
            "p0_bug": p0_bug.get_untracked(),
            "delivery_failing": delivery_failing.get_untracked(),
            "agent_key_revoked": agent_key_revoked.get_untracked(),
        });
        run_mutation(
            busy,
            outcome,
            settings_reload,
            "Kairos saved your notification settings.".to_string(),
            async move {
                crate::api::put_json::<_, NotificationSettings>(auth, SETTINGS, &body)
                    .await
                    .map(|_| ())
            },
        );
    };
    let on_marked = Callback::new(move |result: Result<(), ApiError>| {
        if let Err(error) = result {
            outcome.set(Some(Err(error)));
        }
        reload.update(|n| *n += 1);
    });
    let on_mark_all = move |_| {
        run_mutation(
            busy,
            outcome,
            reload,
            "Kairos marked each notification read.".to_string(),
            async move { mark(auth, None).await.map(|_| ()) },
        );
    };

    view! {
        <PageHeader title="Notifications" sub="Kairos tells you when a person hands off a task \
                                               to you, when a blocker of your task clears, when \
                                               a person cancels a task that you created, when a \
                                               person writes your user name, when a P0 bug \
                                               lands on the board of your team, and when Kairos \
                                               turns off a webhook target that you own."/>
        <Stack gap="md">
            <MutationNotice outcome/>
            <Panel title="Your notifications"
                caption="A notification that you mark read here is read for your agent too.">
                <Stack gap="sm">
                    <Group justify="end">
                        <Button variant="default" size="xs" on_click=Callback::new(on_mark_all)>
                            "Mark all read"
                        </Button>
                    </Group>
                    {move || match notifications.get() {
                        None => view! { <Loading/> }.into_any(),
                        Some(Err(error)) => view! {
                            <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                        }.into_any(),
                        Some(Ok(list)) => view! {
                            <NotificationRows items=list.items busy on_marked/>
                        }.into_any(),
                    }}
                </Stack>
            </Panel>
            <Panel title="Settings"
                caption="Turn a kind off, and Kairos makes no notification of that kind for you.">
                {move || match settings.get() {
                    None => view! { <Loading/> }.into_any(),
                    Some(Err(error)) => view! { <ErrorState error/> }.into_any(),
                    Some(Ok(_)) => view! {
                        <div data-testid="notification-settings">
                            <Stack gap="sm">
                                <Switch checked=hand_off label="A person hands off a task to you"/>
                                <Switch checked=blocker_cleared
                                    label="A blocker of your claimed task is done, archived or unlinked"/>
                                <Switch checked=blocked_again
                                    label="A done blocker of your task is open again"/>
                                <Switch checked=cancelled
                                    label="A person cancels a task that you created"/>
                                <Switch checked=blocker_cancelled
                                    label="A person cancels a blocker of your task"/>
                                <Switch checked=mention label="A person writes your user name"/>
                                <Switch checked=p0_bug label="A P0 bug lands on the board of your team"/>
                                <Switch checked=delivery_failing
                                    label="Kairos turns off a webhook target that you own after failed deliveries"/>
                                <Switch checked=agent_key_revoked
                                    label="An organization admin revokes an agent key of yours"/>
                                <Group justify="end">
                                    <Button on_click=Callback::new(on_save)>"Save"</Button>
                                </Group>
                            </Stack>
                        </div>
                    }.into_any(),
                }}
            </Panel>
        </Stack>
    }
}

#[cfg(test)]
mod tests {
    use super::{count_text, kind_label, mark_body, when};

    #[test]
    fn the_body_marks_one_or_all() {
        assert_eq!(mark_body(Some("x")), serde_json::json!({"ids": ["x"]}));
        assert_eq!(mark_body(None), serde_json::json!({"all": true}));
    }

    #[test]
    fn each_kind_has_words() {
        for kind in [
            "hand_off",
            "blocker_cleared",
            "blocked_again",
            "cancelled",
            "blocker_cancelled",
            "mention",
            "p0_bug",
            "delivery_failing",
            "agent_key_revoked",
        ] {
            assert_ne!(kind_label(kind), "Notification", "{kind}");
        }
        assert_eq!(kind_label("other"), "Notification");
    }

    #[test]
    fn the_count_and_the_time_read_short() {
        assert_eq!(count_text(0), "");
        assert_eq!(count_text(7), "7");
        assert_eq!(count_text(150), "99+");
        assert_eq!(when("2026-10-12T09:15:00Z"), "2026-10-12 09:15 UTC");
    }
}
