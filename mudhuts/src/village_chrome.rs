//! Phase 6's Hut-level tab-strip chrome: one horizontal strip per
//! Tab-Hut along the *active path* from the top-level Hut down to
//! the focused ConsoleHut, stacked from the top of the screen — outermost
//! (toplevel) first — with `chrome.rs`'s own per-ConsoleHut Main-Window tab
//! strip pushed below the last of them (see `render.rs`'s
//! `build_frame_elements`, which threads the total stack height through).
//!
//! A Tile-Hut (or a bare ConsoleHut) ends the walk — there's nothing further
//! to stack below it; a Tile's panes are all visible simultaneously (see
//! `render.rs`'s Tile-Hut compositing), so there's no "active tab" to
//! show a strip for in the first place. A Tab-Hut with only 1 child
//! doesn't get a strip either (nothing to switch between) — though in
//! practice this never happens, since `GraphStack::remove_exited`'s
//! collapse rule unwraps it immediately.
//!
//! Migration step 4: rewritten against the typed graph
//! (`docs/rfcs/typed-graph-hut.md`) — operates on `NodeId`/`Graph<
//! RenderEnv>` instead of `&Hut`/`&mut Hut`. `TabNode`'s own
//! `child_chrome` map plays exactly the role `TabbedHut`'s
//! `label_cache`/`tab_ids`/`bg_tracker` did.

use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Point, Transform};

use crate::chrome::{TAB_PADDING, TabRect, tab_h, tab_row_layout, to_color32f, window_title};
use crate::graph::{Graph, NodeId};
use crate::graph_nodes::{ConsoleNode, RenderEnv, TabNode};
use crate::render::OutputRenderElements;
use crate::space_element::HutSpaceRenderElement;
use crate::theme::Theme;

type Element = OutputRenderElements<GlesRenderer, HutSpaceRenderElement>;

/// What to show on a child's tab — its currently-effective ConsoleHut's active
/// view: the active Main Window's title if it's showing one, else
/// "Terminal". `window_title` already truncates and falls back exactly
/// the way a tab label needs to, so there's nothing left for this
/// function to do beyond picking which of "Terminal" or that title
/// applies.
fn child_label(graph: &Graph<RenderEnv>, child: NodeId) -> String {
    let leaf = graph.focused_leaf(child);
    let Some(console) = graph.downcast::<ConsoleNode>(leaf) else {
        return "Terminal".to_string();
    };
    let hut = &console.hut;
    if !*hut.showing_terminal
        && hut.main_window_count() > 0
        && let Some(window) = hut.active_window()
    {
        return window_title(window);
    }
    "Terminal".to_string()
}

/// Total height of the Hut-level tab-strip stack along the active
/// path from `village` down — `0` if `village` isn't a Tab node with
/// 2+ children (nothing to stack). Used by `render.rs` to know where
/// `chrome.rs`'s own strip (and, when not tiled/tabbed at all, the
/// terminal/window content itself) should start.
pub fn stack_height(graph: &Graph<RenderEnv>, village: NodeId, cell_h: i32, scale: f64) -> i32 {
    let Some(tab) = graph.downcast::<TabNode>(village) else {
        return 0;
    };
    let children = graph.hut_list_input(village, "children");
    if children.len() < 2 {
        return 0;
    }
    let next = children[(*tab.active).min(children.len() - 1)];
    tab_h(cell_h, scale) + stack_height(graph, next, cell_h, scale)
}

/// Hit-test a click position (physical pixels) against the Hut-level
/// tab-strip hierarchy, recursing down the active path the same way
/// [`build`] does. On a hit, switches that level's `active` index and
/// returns `true` (the caller should re-sync visible content/focus and
/// redraw); `false` if the click didn't land on any Hut-level tab —
/// the tile-pane/ConsoleHut-level click handling should take over instead.
pub fn handle_click(
    graph: &mut Graph<RenderEnv>,
    village: NodeId,
    pos: (i32, i32),
    y: i32,
    cell_w: usize,
    cell_h: i32,
    scale: f64,
) -> bool {
    if graph.downcast::<TabNode>(village).is_none() {
        return false;
    }
    let children = graph.hut_list_input(village, "children");
    if children.len() < 2 {
        return false;
    }
    // Only widths matter for a hit test, not the label text itself, so
    // there's no reason to collect a `Vec<String>` just to read each
    // one's length and throw it away.
    let char_counts: Vec<usize> = children.iter().map(|&c| child_label(graph, c).chars().count()).collect();
    let point = Point::from(pos);
    for TabRect { index: i, rect } in tab_row_layout(&char_counts, y, cell_w, cell_h, scale) {
        if rect.contains(point) {
            if let Some(tab) = graph.downcast_mut::<TabNode>(village) {
                *tab.active = i;
            }
            return true;
        }
    }
    let active = graph.downcast::<TabNode>(village).map(|t| *t.active).unwrap_or(0);
    let next = children[active.min(children.len() - 1)];
    handle_click(graph, next, pos, y + tab_h(cell_h, scale), cell_w, cell_h, scale)
}

/// Build the Hut-level tab-strip stack's render elements, recursing
/// down the active path (see this module's doc) — empty if `village`
/// isn't a Tab node with 2+ children. Returns the elements plus the Y
/// where whatever's next (a deeper level, or `chrome.rs`'s own strip)
/// should start.
pub fn build(
    graph: &mut Graph<RenderEnv>,
    village: NodeId,
    renderer: &mut GlesRenderer,
    y: i32,
    cell_w: usize,
    cell_h: i32,
    scale: f64,
    theme: &Theme,
) -> (Vec<Element>, i32) {
    if graph.downcast::<TabNode>(village).is_none() {
        return (Vec::new(), y);
    }
    let children = graph.hut_list_input(village, "children");
    if children.len() < 2 {
        return (Vec::new(), y);
    }

    // Unlike `handle_click`, the label text itself is needed below (for
    // rendering/cache keys), so it's computed once here and reused for
    // both the layout pass and the render pass instead of being
    // recomputed.
    let labels: Vec<String> = children.iter().map(|&c| child_label(graph, c)).collect();
    let char_counts: Vec<usize> = labels.iter().map(|label| label.chars().count()).collect();
    let rects = tab_row_layout(&char_counts, y, cell_w, cell_h, scale);
    let padding = crate::render::scaled(TAB_PADDING, scale);

    let mut elements = Vec::new();
    let mut active_index = 0;

    // `with_node_mut`, not a plain `downcast_mut` — this loop needs
    // simultaneous mutable access to *two* different nodes at once
    // (`village`'s own `TabNode` for its label cache, and each child's
    // own `ConsoleNode` to actually render a label onto it) — the same
    // "temporarily remove, call, put back" mechanism `Graph::
    // resolve_output`/`with_node_mut` itself is built on.
    graph.with_node_mut(village, |node, graph| {
        let tab = node
            .as_any_mut()
            .downcast_mut::<TabNode>()
            .expect("already confirmed to be a TabNode above");
        active_index = *tab.active;

        for TabRect { index: i, rect } in &rects {
            let i = *i;
            let child_id = children[i];
            // Lazily creates this child's own chrome entry on first
            // access — see `TabNode::child_chrome`'s own doc comment for
            // why this replaced a grow-only `while ... .len() < ...`
            // loop over 3 separate parallel `Vec`s.
            let chrome = tab.child_chrome.entry(child_id).or_insert_with(crate::graph_nodes::TabChildChrome::new);
            let active = active_index == i;
            let (fg, bg) = if active {
                (theme.hut_tab_active_fg, theme.hut_tab_active_bg)
            } else {
                (theme.hut_tab_inactive_fg, theme.hut_tab_inactive_bg)
            };
            let label = labels[i].clone();
            let key = (label.clone(), active);

            let child_leaf = graph.focused_leaf(child_id);
            let mut render = |graph: &mut Graph<RenderEnv>| {
                graph
                    .downcast_mut::<ConsoleNode>(child_leaf)
                    .ok_or_else(|| "no ConsoleHut for this tab".to_string())
                    .and_then(|console| console.hut.render_label(renderer, &label, fg, bg))
            };
            let texture = if chrome.label_cache.is_stale(&key) {
                render(graph).map(|texture| chrome.label_cache.store(key, texture))
            } else {
                match chrome.label_cache.cached() {
                    Some(cached) => Ok(cached),
                    None => render(graph).map(|texture| chrome.label_cache.store(key, texture)),
                }
            };

            let (text_id, bg_id) = chrome.tab_ids.clone();
            match texture {
                Ok((texture, snapshot)) => {
                    let text = TextureRenderElement::from_texture_with_damage(
                        text_id,
                        renderer.context_id(),
                        ((rect.loc.x + padding) as f64, (rect.loc.y + padding) as f64),
                        texture,
                        crate::render::texture_buffer_scale(scale),
                        Transform::Normal,
                        None,
                        None,
                        None,
                        None,
                        snapshot,
                        Kind::Unspecified,
                    );
                    elements.push(Element::from(text));
                }
                Err(err) => tracing::warn!("failed to render Hut tab label {label:?}: {err}"),
            }

            let bg_commit = chrome.bg_tracker.commit(active);
            let background =
                SolidColorRenderElement::new(bg_id, *rect, bg_commit, to_color32f(bg), Kind::Unspecified);
            elements.push(Element::from(background));
        }
    });

    let next = children[active_index.min(children.len() - 1)];
    let (deeper_elements, next_y) =
        build(graph, next, renderer, y + tab_h(cell_h, scale), cell_w, cell_h, scale, theme);
    elements.extend(deeper_elements);
    (elements, next_y)
}
