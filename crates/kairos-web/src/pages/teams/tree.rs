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

thread_local! {
    /// The folders a person opened in this browser session. The tree renders
    /// again after each change (a refetch), so the open state lives here, not
    /// in the DOM; a drop into a folder opens it.
    static OPEN_FOLDERS: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

fn folder_is_open(id: &str) -> bool {
    OPEN_FOLDERS.with(|open| open.borrow().contains(id))
}

fn set_folder_open(id: &str, open: bool) {
    OPEN_FOLDERS.with(|folders| {
        let mut folders = folders.borrow_mut();
        if open {
            folders.insert(id.to_string());
        } else {
            folders.remove(id);
        }
    });
}

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

/// The position of a new node in `parent`: after its last sibling (the
/// root starts at 1, after the charter).
pub(crate) fn next_position(pages: &[api::TeamPageNode], parent: Option<&str>) -> i32 {
    let base = if parent.is_none() { 1 } else { 0 };
    siblings(pages, parent)
        .iter()
        .filter(|p| p.position < api::UNPLACED)
        .map(|p| p.position + 1)
        .max()
        .unwrap_or(base)
        .max(base)
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
    /// The rules line shows while a drag is on. Separate from `drag`
    /// (KAIROS-T-0512): Chromium cancels a native drag when the DOM changes
    /// inside the `dragstart` handler, so this is set after the handler
    /// returns, while `drag` is set at once (the first `dragover` reads it).
    rules: RwSignal<bool>,
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
        drag_end(*self);
        let Some(steps) = self
            .pages
            .with_value(|pages| plan_drop(pages, &dragged, &target))
        else {
            return;
        };
        // The folder that takes the node opens, so the person sees it land.
        let landing = match &target {
            DropTarget::Into(folder) => Some(folder.clone()),
            DropTarget::Before { parent, .. } | DropTarget::End { parent } => parent.clone(),
        };
        if let Some(folder) = landing {
            set_folder_open(&folder, true);
        }
        if self.busy.get_untracked() {
            return;
        }
        let ctx = *self;
        ctx.busy.set(true);
        ctx.error.set(None);
        // A step on a document or an ADR places the item (KAIROS-T-0437);
        // an item always sends its parent, so a reorder keeps its folder.
        let items: Vec<(String, String, Option<String>)> = ctx.pages.with_value(|pages| {
            pages
                .iter()
                .filter_map(|p| {
                    p.item
                        .as_ref()
                        .map(|i| (p.id.clone(), i.short_code.clone(), p.parent_id.clone()))
                })
                .collect()
        });
        leptos::task::spawn_local(async move {
            let team_id = ctx.team_id.get_value();
            for step in steps {
                let result = match items.iter().find(|(id, _, _)| *id == step.id) {
                    Some((_, code, current_parent)) => {
                        let parent = match &step.parent {
                            Some(parent) => parent.clone(),
                            None => current_parent.clone(),
                        };
                        api::place_item(ctx.auth, &team_id, code, parent.as_deref(), step.position)
                            .await
                            .map(|_| ())
                    }
                    None => {
                        let parent = step.parent.as_ref().map(|p| p.as_deref());
                        api::place_page(ctx.auth, &team_id, &step.id, parent, step.position)
                            .await
                            .map(|_| ())
                    }
                };
                if let Err(e) = result {
                    ctx.error.set(Some(error_text(&e)));
                    break;
                }
            }
            ctx.busy.set(false);
            ctx.on_changed.run(());
        });
    }
}

/// The team tree: folders and pages (not the charter), and the documents and
/// ADRs of the team (KAIROS-T-0437), with in-place create and drag-and-drop
/// for a team member or an organization admin.
#[component]
pub(crate) fn TeamTree(
    team_id: String,
    team_slug: String,
    pages: Vec<api::TeamPageNode>,
    items: Vec<api::TreeItem>,
    manage: bool,
    on_changed: Callback<()>,
) -> impl IntoView {
    let mut nodes = pages;
    nodes.extend(api::item_nodes(&items));
    let ctx = TreeCtx {
        auth: use_auth(),
        team_id: StoredValue::new(team_id.clone()),
        team_slug: StoredValue::new(team_slug),
        pages: StoredValue::new(nodes),
        manage,
        drag: RwSignal::new(None),
        rules: RwSignal::new(false),
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
                {manage.then(|| {
                    let position = ctx.pages.with_value(|pages| next_position(pages, None));
                    view! { <InlineCreate team_id parent_id=None position on_changed/> }
                })}
                // KAIROS-T-0467: a refused drop shows only the not-allowed
                // cursor; this line says the rules while a drag is on. Under
                // the tree (KAIROS-T-0512): above it, the tree moved down a
                // line as the drag began, and the pointer was no longer over
                // the gap or folder it had been over.
                {move || ctx.rules.get().then(|| view! {
                    <Text dimmed=true size="sm">
                        "Drop onto a folder to move the node in, onto a gap to reorder. A folder cannot go into itself, and the charter stays first."
                    </Text>
                })}
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
    // KAIROS-T-0512: the rules line changes the DOM, which cancels the drag
    // in Chromium when it happens inside this handler (dragend fires at
    // once, nothing drags). Show it after the handler returns, and only if
    // the drag is still on.
    set_timeout(
        move || {
            if ctx.drag.get_untracked().is_some() {
                ctx.rules.set(true);
            }
        },
        std::time::Duration::from_millis(0),
    );
}

/// The drag ended (a drop, or the browser cancelled it).
fn drag_end(ctx: TreeCtx) {
    ctx.drag.set(None);
    ctx.rules.set(false);
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
            let toggle_id = node.id.clone();
            let next = ctx
                .pages
                .with_value(|pages| next_position(pages, Some(&node.id)));
            rows.push(
                view! {
                    <details
                        class="kairos-doctree__folder"
                        open=folder_is_open(&toggle_id)
                        on:toggle=move |ev: web_sys::Event| {
                            use wasm_bindgen::JsCast;
                            if let Some(details) = ev
                                .target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                            {
                                set_folder_open(&toggle_id, details.open());
                            }
                        }
                    >
                        <summary
                            class="kairos-doctree__summary"
                            class:kairos-doctree__summary--over=move || over.get()
                            draggable=if ctx.manage { "true" } else { "false" }
                            on:dragstart=move |ev: web_sys::DragEvent| drag_start(ctx, id.clone(), &ev)
                            on:dragend=move |_| drag_end(ctx)
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
                                    position=next
                                    on_changed=ctx.on_changed
                                />
                            })}
                        </div>
                    </details>
                }
                .into_any(),
            );
        } else if let Some(item) = node.item.clone() {
            // A document or an ADR: it links to its item page; the board shows
            // where it is in its lifecycle.
            let item_href = format!("/items/{}", item.short_code);
            let kind = if item.entity_type == "adr" {
                "ADR"
            } else {
                "doc"
            };
            rows.push(
                view! {
                    <div
                        class="kairos-doctree__page kairos-doctree__item"
                        draggable=if ctx.manage { "true" } else { "false" }
                        on:dragstart=move |ev: web_sys::DragEvent| drag_start(ctx, id.clone(), &ev)
                        on:dragend=move |_| drag_end(ctx)
                    >
                        <Group gap="xs">
                            <Text dimmed=true size="xs">{kind}</Text>
                            <Anchor href=item_href>{title}</Anchor>
                            <Text dimmed=true size="xs">{format!("{} · {}", item.short_code, item.status)}</Text>
                        </Group>
                    </div>
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
                        on:dragend=move |_| drag_end(ctx)
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
    /// Where the new node goes among its siblings (after the last one).
    position: i32,
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
                position,
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
                    // KAIROS-T-0467: a title of emoji or CJK gives no slug,
                    // and the Create button is just off; say what to type.
                    {move || (!title.get().trim().is_empty() && slug.get().trim().is_empty()).then(|| view! {
                        <Text dimmed=true size="sm">"Type a slug: 2 to 63 of a-z, 0-9, - and _, a letter first."</Text>
                    })}
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
            item: None,
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
    fn a_new_node_goes_after_the_last_sibling() {
        let pages = tree();
        assert_eq!(next_position(&pages, None), 3);
        assert_eq!(next_position(&pages, Some("a")), 3);
        assert_eq!(next_position(&pages, Some("b")), 0);
        // A team with the charter only: the root starts after it.
        assert_eq!(next_position(&pages[..1], None), 1);
    }

    #[test]
    fn a_document_moves_like_a_page_and_is_no_container() {
        let mut pages = tree();
        pages.extend(api::item_nodes(&[api::TreeItem {
            item_id: "d".into(),
            short_code: "ACME-D-0001".into(),
            entity_type: "document".into(),
            title: "Plan".into(),
            status: "draft".into(),
            parent_id: None,
            position: None,
        }]));
        // Unplaced, it sits after the placed root nodes and new nodes go
        // before it.
        assert_eq!(next_position(&pages, None), 3);
        assert_eq!(
            plan_drop(&pages, "d", &DropTarget::Into("b".into())),
            Some(vec![step("d", Some(Some("b")), 0)])
        );
        assert_eq!(plan_drop(&pages, "p", &DropTarget::Into("d".into())), None);
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
