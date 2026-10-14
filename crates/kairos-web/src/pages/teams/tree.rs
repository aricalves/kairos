//! The team tree, grown in place (KAIROS-T-0438, decided in KAIROS-T-0435):
//! like Confluence, a person creates a folder or a page where they are, at
//! any level, and moves it by hand. Folders are the only containers.
//!
//! - Each folder and the root have "+ Page" and "+ Folder". The slug comes
//!   from the title; the person can change it, and does not have to type it.
//! - Drag a node onto a folder to move it into the folder (at the end).
//!   Drag it onto the gap before a node, or at the end of a list, to move it
//!   there; in the same list, that reorders it. The positions of the list
//!   are saved: [`plan_drop`] gives the writes, one PATCH each.
//! - The charter is not in the tree: it is the landing page, fixed at the
//!   top, and the server refuses a move of it.

use aurora_dark::components::{Alert, Anchor, Button, Group, Stack, Text, TextInput};
use aurora_dark::tokens::token;
use leptos::prelude::*;

use super::api;
use crate::auth::{Auth, use_auth};
use crate::pages::item::api::error_text;

/// A slug from a title: lower case, each run of other characters one
/// hyphen ("How-to Guides" gives "how-to-guides").
pub(crate) fn slug_from_title(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// Where a dragged node lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DropTarget {
    /// Into this folder, after its last child.
    Into(String),
    /// In the list of `parent` (None: the root), just before `before`.
    Before {
        parent: Option<String>,
        before: String,
    },
    /// At the end of the list of `parent` (None: the root).
    End { parent: Option<String> },
}

/// One write of a drop: the node, its new parent when it changes
/// (`Some(None)` is the root), and its new position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaceStep {
    pub id: String,
    pub parent: Option<Option<String>>,
    pub position: i32,
}

/// The children of `parent` in the order of the tree: position, then title.
/// The charter is not one of them.
pub(crate) fn siblings<'a>(
    pages: &'a [api::TeamPageNode],
    parent: Option<&str>,
) -> Vec<&'a api::TeamPageNode> {
    let mut list: Vec<_> = pages
        .iter()
        .filter(|p| p.parent_id.as_deref() == parent && !p.is_charter())
        .collect();
    list.sort_by(|a, b| a.position.cmp(&b.position).then(a.title.cmp(&b.title)));
    list
}

/// Whether `candidate` is `node` or a node inside it.
fn is_self_or_inside(pages: &[api::TeamPageNode], node: &str, candidate: &str) -> bool {
    let mut cursor = Some(candidate.to_string());
    for _ in 0..64 {
        let Some(id) = cursor else { return false };
        if id == node {
            return true;
        }
        cursor = pages
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.parent_id.clone());
    }
    false
}

/// The writes that put `dragged` at `target`, or `None` when the drop is not
/// allowed (the charter, a folder into itself or into a folder inside it, a
/// page as a container) or changes nothing. The new list of the target
/// parent is numbered from 1 at the root (the charter is 0) and from 0 in a
/// folder; only a node whose position or parent changes gets a write. The
/// dragged node comes first, so a refused move writes nothing else.
pub(crate) fn plan_drop(
    pages: &[api::TeamPageNode],
    dragged: &str,
    target: &DropTarget,
) -> Option<Vec<PlaceStep>> {
    let node = pages.iter().find(|p| p.id == dragged)?;
    if node.is_charter() {
        return None;
    }
    let (parent, before) = match target {
        DropTarget::Into(folder) => (Some(folder.clone()), None),
        DropTarget::Before { parent, before } => (parent.clone(), Some(before.clone())),
        DropTarget::End { parent } => (parent.clone(), None),
    };
    if let Some(parent) = &parent {
        let folder = pages.iter().find(|p| &p.id == parent)?;
        if folder.kind != "folder" || is_self_or_inside(pages, dragged, parent) {
            return None;
        }
    }
    if before.as_deref() == Some(dragged) {
        return None;
    }
    let mut order: Vec<&str> = siblings(pages, parent.as_deref())
        .into_iter()
        .map(|p| p.id.as_str())
        .filter(|id| *id != dragged)
        .collect();
    let index = match &before {
        Some(before) => order.iter().position(|id| id == before)?,
        None => order.len(),
    };
    order.insert(index, dragged);
    let base = if parent.is_none() { 1 } else { 0 };
    let parent_changes = node.parent_id != parent;
    let mut steps = Vec::new();
    for (offset, id) in order.iter().enumerate() {
        let position = base + offset as i32;
        let current = pages.iter().find(|p| p.id == *id)?;
        let is_dragged = *id == dragged;
        if is_dragged && (parent_changes || current.position != position) {
            steps.insert(
                0,
                PlaceStep {
                    id: dragged.to_string(),
                    parent: parent_changes.then(|| parent.clone()),
                    position,
                },
            );
        } else if !is_dragged && current.position != position {
            steps.push(PlaceStep {
                id: (*id).to_string(),
                parent: None,
                position,
            });
        }
    }
    (!steps.is_empty()).then_some(steps)
}

/// What every node of the tree shares: Copy handles only.
#[derive(Clone, Copy)]
struct TreeCtx {
    auth: Auth,
    team_id: StoredValue<String>,
    team_slug: StoredValue<String>,
    pages: StoredValue<Vec<api::TeamPageNode>>,
    manage: bool,
    drag: RwSignal<Option<String>>,
    error: RwSignal<Option<String>>,
    busy: RwSignal<bool>,
    on_changed: Callback<()>,
}

impl TreeCtx {
    fn legal(&self, target: &DropTarget) -> bool {
        self.drag.with_untracked(|d| {
            d.as_ref().is_some_and(|dragged| {
                self.pages
                    .with_value(|pages| plan_drop(pages, dragged, target).is_some())
            })
        })
    }

    fn drop_on(&self, target: DropTarget) {
        let Some(dragged) = self.drag.get_untracked() else {
            return;
        };
        self.drag.set(None);
        let Some(steps) = self
            .pages
            .with_value(|pages| plan_drop(pages, &dragged, &target))
        else {
            return;
        };
        if self.busy.get_untracked() {
            return;
        }
        let ctx = *self;
        ctx.busy.set(true);
        ctx.error.set(None);
        leptos::task::spawn_local(async move {
            let team_id = ctx.team_id.get_value();
            for step in steps {
                let parent = step.parent.as_ref().map(|p| p.as_deref());
                if let Err(e) =
                    api::place_page(ctx.auth, &team_id, &step.id, parent, step.position).await
                {
                    ctx.error.set(Some(error_text(&e)));
                    break;
                }
            }
            ctx.busy.set(false);
            ctx.on_changed.run(());
        });
    }
}

/// The team tree: folders and pages (not the charter), with in-place create
/// and drag-and-drop for a team member or an organization admin.
#[component]
pub(crate) fn TeamTree(
    team_id: String,
    team_slug: String,
    pages: Vec<api::TeamPageNode>,
    manage: bool,
    on_changed: Callback<()>,
) -> impl IntoView {
    let ctx = TreeCtx {
        auth: use_auth(),
        team_id: StoredValue::new(team_id.clone()),
        team_slug: StoredValue::new(team_slug),
        pages: StoredValue::new(pages),
        manage,
        drag: RwSignal::new(None),
        error: RwSignal::new(None),
        busy: RwSignal::new(false),
        on_changed,
    };
    let empty = ctx
        .pages
        .with_value(|pages| siblings(pages, None).is_empty());
    view! {
        <div class="kairos-doctree">
            <Stack gap="xs">
                {move || ctx.error.get().map(|message| view! {
                    <Alert title="Move failed" color=token::BAD>
                        <Text size="sm" dimmed=true>{message}</Text>
                    </Alert>
                })}
                {(empty && !manage).then(|| view! { <Text dimmed=true size="sm">"No pages yet."</Text> })}
                {render_level(ctx, None, String::new())}
                {manage.then(|| view! { <InlineCreate team_id parent_id=None on_changed/> })}
            </Stack>
        </div>
    }
}

/// A drop zone: the thin gap before a node or at the end of a list.
fn gap(ctx: TreeCtx, target: DropTarget) -> AnyView {
    if !ctx.manage {
        return ().into_any();
    }
    let over = RwSignal::new(false);
    let target_for_check = target.clone();
    view! {
        <div
            class="kairos-doctree__gap"
            class:kairos-doctree__gap--over=move || over.get()
            on:dragover=move |ev: web_sys::DragEvent| {
                if ctx.legal(&target_for_check) {
                    ev.prevent_default();
                    over.set(true);
                }
            }
            on:dragleave=move |_| over.set(false)
            on:drop=move |ev: web_sys::DragEvent| {
                ev.prevent_default();
                over.set(false);
                ctx.drop_on(target.clone());
            }
        ></div>
    }
    .into_any()
}

/// Start a drag of `id` (a team member or an organization admin only).
fn drag_start(ctx: TreeCtx, id: String, ev: &web_sys::DragEvent) {
    if !ctx.manage {
        return;
    }
    // dataTransfer content is required for some engines to start a drag.
    if let Some(dt) = ev.data_transfer() {
        let _ = dt.set_data("text/plain", &id);
        dt.set_effect_allowed("move");
    }
    ctx.drag.set(Some(id));
}

/// One list: the children of `parent`, each with the gap before it, then
/// the gap at the end. Views are built eagerly (`view!` output is 'static).
fn render_level(ctx: TreeCtx, parent: Option<String>, path_prefix: String) -> AnyView {
    let nodes: Vec<api::TeamPageNode> = ctx.pages.with_value(|pages| {
        siblings(pages, parent.as_deref())
            .into_iter()
            .cloned()
            .collect()
    });
    let team_slug = ctx.team_slug.get_value();
    let mut rows = Vec::new();
    for node in nodes {
        rows.push(gap(
            ctx,
            DropTarget::Before {
                parent: parent.clone(),
                before: node.id.clone(),
            },
        ));
        let path = if path_prefix.is_empty() {
            node.slug.clone()
        } else {
            format!("{path_prefix}/{}", node.slug)
        };
        let href = format!("/teams/{team_slug}/pages/{path}");
        let id = node.id.clone();
        let title = node.title.clone();
        if node.kind == "folder" {
            let inner = render_level(ctx, Some(node.id.clone()), path.clone());
            let over = RwSignal::new(false);
            let into = DropTarget::Into(node.id.clone());
            let into_check = into.clone();
            let team_id = ctx.team_id.get_value();
            let folder_id = node.id.clone();
            rows.push(
                view! {
                    <details class="kairos-doctree__folder">
                        <summary
                            class="kairos-doctree__summary"
                            class:kairos-doctree__summary--over=move || over.get()
                            draggable=if ctx.manage { "true" } else { "false" }
                            on:dragstart=move |ev: web_sys::DragEvent| drag_start(ctx, id.clone(), &ev)
                            on:dragend=move |_| ctx.drag.set(None)
                            on:dragover=move |ev: web_sys::DragEvent| {
                                if ctx.legal(&into_check) {
                                    ev.prevent_default();
                                    over.set(true);
                                }
                            }
                            on:dragleave=move |_| over.set(false)
                            on:drop=move |ev: web_sys::DragEvent| {
                                ev.prevent_default();
                                over.set(false);
                                ctx.drop_on(into.clone());
                            }
                        >
                            <Group gap="xs">
                                <Text bright=true size="sm">{title}</Text>
                                <Anchor href=href>
                                    <Text dimmed=true size="xs">"open"</Text>
                                </Anchor>
                            </Group>
                        </summary>
                        <div class="kairos-doctree__children">
                            {inner}
                            {ctx.manage.then(|| view! {
                                <InlineCreate
                                    team_id
                                    parent_id=Some(folder_id)
                                    on_changed=ctx.on_changed
                                />
                            })}
                        </div>
                    </details>
                }
                .into_any(),
            );
        } else {
            rows.push(
                view! {
                    <div
                        class="kairos-doctree__page"
                        draggable=if ctx.manage { "true" } else { "false" }
                        on:dragstart=move |ev: web_sys::DragEvent| drag_start(ctx, id.clone(), &ev)
                        on:dragend=move |_| ctx.drag.set(None)
                    >
                        <Anchor href=href>{title}</Anchor>
                    </div>
                }
                .into_any(),
            );
        }
    }
    rows.push(gap(ctx, DropTarget::End { parent }));
    view! { <div class="kairos-doctree__list">{rows}</div> }.into_any()
}

/// "+ Page" and "+ Folder" in place: a title, and a slug that comes from
/// the title until the person changes it.
#[component]
pub(crate) fn InlineCreate(
    team_id: String,
    parent_id: Option<String>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let auth = use_auth();
    let team_id = StoredValue::new(team_id);
    let parent_id = StoredValue::new(parent_id);
    let kind = RwSignal::new(None::<&'static str>);
    let title = RwSignal::new(String::new());
    let slug = RwSignal::new(String::new());
    let slug_touched = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let reset = move || {
        kind.set(None);
        title.set(String::new());
        slug.set(String::new());
        slug_touched.set(false);
        error.set(None);
    };
    let create = move |_: ()| {
        let Some(kind_value) = kind.get_untracked() else {
            return;
        };
        let title_value = title.get_untracked().trim().to_string();
        let slug_value = slug.get_untracked().trim().to_string();
        if title_value.is_empty() || slug_value.is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let result = api::create_page(
                auth,
                &team_id.get_value(),
                parent_id.get_value().as_deref(),
                kind_value,
                &slug_value,
                &title_value,
            )
            .await;
            busy.set(false);
            match result {
                Ok(_) => {
                    reset();
                    on_changed.run(());
                }
                Err(e) => error.set(Some(error_text(&e))),
            }
        });
    };
    view! {
        {move || match kind.get() {
            None => view! {
                <Group gap="xs">
                    <Button variant="subtle" size="xs" on_click=Callback::new(move |_| kind.set(Some("page")))>
                        "+ Page"
                    </Button>
                    <Button variant="subtle" size="xs" on_click=Callback::new(move |_| kind.set(Some("folder")))>
                        "+ Folder"
                    </Button>
                </Group>
            }.into_any(),
            Some(kind_value) => view! {
                <Stack gap="xs">
                    {move || error.get().map(|message| view! {
                        <Alert title="Create failed" color=token::BAD>
                            <Text size="sm" dimmed=true>{message}</Text>
                        </Alert>
                    })}
                    <Group align="end" gap="sm">
                        <TextInput
                            label=if kind_value == "folder" { "New folder" } else { "New page" }
                            placeholder="Title"
                            value=title
                            on_input=Callback::new(move |text: String| {
                                if !slug_touched.get_untracked() {
                                    slug.set(slug_from_title(&text));
                                }
                            })
                        />
                        <TextInput
                            label="Slug"
                            value=slug
                            mono=true
                            on_input=Callback::new(move |_| slug_touched.set(true))
                        />
                        <Button
                            disabled=move || title.get().trim().is_empty() || slug.get().trim().is_empty()
                            loading=busy
                            loading_label="Creating…"
                            on_click=Callback::new(create)
                        >
                            "Create"
                        </Button>
                        <Button variant="subtle" on_click=Callback::new(move |_| reset())>"Cancel"</Button>
                    </Group>
                </Stack>
            }.into_any(),
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: Option<&str>, kind: &str, position: i32) -> api::TeamPageNode {
        api::TeamPageNode {
            id: id.into(),
            parent_id: parent.map(Into::into),
            kind: kind.into(),
            slug: id.into(),
            title: id.into(),
            content: String::new(),
            position,
            is_protected: false,
            version: 1,
        }
    }

    /// The charter (0), root folders a (1) and b (2), pages p (0) and q (1)
    /// in a, and folder c (2) in a.
    fn tree() -> Vec<api::TeamPageNode> {
        let mut charter = node("charter", None, "page", 0);
        charter.is_protected = true;
        vec![
            charter,
            node("a", None, "folder", 1),
            node("b", None, "folder", 2),
            node("p", Some("a"), "page", 0),
            node("q", Some("a"), "page", 1),
            node("c", Some("a"), "folder", 2),
        ]
    }

    fn step(id: &str, parent: Option<Option<&str>>, position: i32) -> PlaceStep {
        PlaceStep {
            id: id.into(),
            parent: parent.map(|p| p.map(Into::into)),
            position,
        }
    }

    #[test]
    fn the_slug_comes_from_the_title() {
        assert_eq!(slug_from_title("How-to Guides"), "how-to-guides");
        assert_eq!(slug_from_title("  Ways of working!"), "ways-of-working");
        assert_eq!(slug_from_title("Q3 plan"), "q3-plan");
    }

    #[test]
    fn a_drop_on_a_folder_moves_the_node_to_its_end() {
        let pages = tree();
        assert_eq!(
            plan_drop(&pages, "p", &DropTarget::Into("b".into())),
            Some(vec![step("p", Some(Some("b")), 0)])
        );
        // Out of a folder to the end of the root: after a (1) and b (2).
        assert_eq!(
            plan_drop(&pages, "q", &DropTarget::End { parent: None }),
            Some(vec![step("q", Some(None), 3)])
        );
    }

    #[test]
    fn a_drop_before_a_sibling_reorders_and_renumbers_the_list() {
        let pages = tree();
        // q before p in a: q 0, p 1; c keeps 2.
        assert_eq!(
            plan_drop(
                &pages,
                "q",
                &DropTarget::Before {
                    parent: Some("a".into()),
                    before: "p".into()
                }
            ),
            Some(vec![step("q", None, 0), step("p", None, 1)])
        );
        // b before a at the root: b 1, a 2; the charter keeps 0.
        assert_eq!(
            plan_drop(
                &pages,
                "b",
                &DropTarget::Before {
                    parent: None,
                    before: "a".into()
                }
            ),
            Some(vec![step("b", None, 1), step("a", None, 2)])
        );
        // c into the root before b: moves, and b goes from 2 to 3.
        assert_eq!(
            plan_drop(
                &pages,
                "c",
                &DropTarget::Before {
                    parent: None,
                    before: "b".into()
                }
            ),
            Some(vec![step("c", Some(None), 2), step("b", None, 3)])
        );
    }

    #[test]
    fn a_drop_that_changes_nothing_or_is_not_allowed_writes_nothing() {
        let pages = tree();
        // Already last in a.
        assert_eq!(
            plan_drop(
                &pages,
                "c",
                &DropTarget::End {
                    parent: Some("a".into())
                }
            ),
            None
        );
        // Before itself.
        assert_eq!(
            plan_drop(
                &pages,
                "p",
                &DropTarget::Before {
                    parent: Some("a".into()),
                    before: "p".into()
                }
            ),
            None
        );
        // A folder into itself, or into a folder inside it.
        assert_eq!(plan_drop(&pages, "a", &DropTarget::Into("a".into())), None);
        assert_eq!(plan_drop(&pages, "a", &DropTarget::Into("c".into())), None);
        // A page is not a container.
        assert_eq!(plan_drop(&pages, "q", &DropTarget::Into("p".into())), None);
        // The charter does not move.
        assert_eq!(
            plan_drop(&pages, "charter", &DropTarget::Into("a".into())),
            None
        );
    }
}
