//! Phase 6's Village-level tab-strip chrome: one horizontal strip per
//! Tab-Village along the *active path* from the top-level Village down to
//! the focused Hut, stacked from the top of the screen — outermost
//! (toplevel) first — with `chrome.rs`'s own per-Hut Main-Window tab
//! strip pushed below the last of them (see `render.rs`'s
//! `build_frame_elements`, which threads the total stack height through).
//!
//! A Tile-Village (or a bare Hut) ends the walk — there's nothing further
//! to stack below it; a Tile's panes are all visible simultaneously (see
//! `render.rs`'s Tile-Village compositing), so there's no "active tab" to
//! show a strip for in the first place. A Tab-Village with only 1 child
//! doesn't get a strip either (nothing to switch between) — though in
//! practice this never happens, since [`Village::collapse_if_singleton`]
//! unwraps it immediately.

use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Physical, Point, Rectangle, Size, Transform};

use mudhuts_term::palette::Rgb;

use crate::chrome::{to_color32f, window_title};
use crate::render::{ChangeTracker, LabelCache, OutputRenderElements};
use crate::village::Village;

const TAB_PADDING: i32 = 12;
const TAB_GAP: i32 = 4;
const LEFT_MARGIN: i32 = 16;
const MAX_TITLE_CHARS: usize = 24;

const FG_ACTIVE: Rgb = [255, 255, 255];
const BG_ACTIVE: Rgb = [140, 90, 191];
const FG_INACTIVE: Rgb = [190, 190, 190];
const BG_INACTIVE: Rgb = [40, 30, 50];

type Element = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// One tab's clickable/drawable rectangle within a single Tab-Village
/// level, plus which child it is (an index into that level's `children`).
pub struct TabRect {
    pub index: usize,
    pub rect: Rectangle<i32, Physical>,
}

/// What to show on a child's tab — its currently-effective Hut's active
/// view: the active Main Window's title if it's showing one, else
/// "Terminal". Same fallback chain `chrome::window_title` already uses
/// for a single Hut's own Main-Window tabs, one level down.
fn child_label(village: &Village) -> String {
    let hut = village.focused_hut();
    if !hut.showing_terminal
        && hut.main_window_count() > 0
        && let Some(window) = hut.active_window()
    {
        let title = window_title(window);
        return if title.chars().count() > MAX_TITLE_CHARS {
            let truncated: String = title.chars().take(MAX_TITLE_CHARS.saturating_sub(1)).collect();
            format!("{truncated}\u{2026}")
        } else {
            title
        };
    }
    "Terminal".to_string()
}

fn tab_h(cell_h: i32) -> i32 {
    cell_h + TAB_PADDING * 2
}

/// One Tab-Village level's tab rects, at physical-pixel row `y` — shared
/// between rendering and click hit-testing (see this module's doc), so
/// the two can never disagree about where a tab actually is.
fn level_layout(children: &[Village], y: i32, cell_w: usize, cell_h: i32) -> Vec<TabRect> {
    let h = tab_h(cell_h);
    let mut rects = Vec::new();
    let mut x = LEFT_MARGIN;
    for (i, child) in children.iter().enumerate() {
        let label = child_label(child);
        let label_w = (label.chars().count().max(1) * cell_w) as i32;
        let tab_w = label_w + TAB_PADDING * 2;
        rects.push(TabRect {
            index: i,
            rect: Rectangle::new(Point::from((x, y)), Size::from((tab_w, h))),
        });
        x += tab_w + TAB_GAP;
    }
    rects
}

/// Total height of the Village-level tab-strip stack along the active
/// path from `village` down — `0` if `village` isn't a Tab-Village with
/// 2+ children (nothing to stack). Used by `render.rs` to know where
/// `chrome.rs`'s own strip (and, when not tiled/tabbed at all, the
/// terminal/window content itself) should start.
pub fn stack_height(village: &Village, cell_h: i32) -> i32 {
    match village {
        Village::Tab(tab) if tab.children.len() >= 2 => {
            tab_h(cell_h) + stack_height(&tab.children[tab.active], cell_h)
        }
        _ => 0,
    }
}

/// Hit-test a click position (physical pixels) against the Village-level
/// tab-strip hierarchy, recursing down the active path the same way
/// [`build`] does. On a hit, switches that level's `active` index and
/// returns `true` (the caller should re-sync visible content/focus and
/// redraw); `false` if the click didn't land on any Village-level tab —
/// the tile-pane/Hut-level click handling should take over instead.
pub fn handle_click(village: &mut Village, pos: (i32, i32), y: i32, cell_w: usize, cell_h: i32) -> bool {
    let Village::Tab(tab) = village else {
        return false;
    };
    if tab.children.len() < 2 {
        return false;
    }
    let point = Point::from(pos);
    for TabRect { index: i, rect } in level_layout(&tab.children, y, cell_w, cell_h) {
        if rect.contains(point) {
            tab.active = i;
            return true;
        }
    }
    handle_click(&mut tab.children[tab.active], pos, y + tab_h(cell_h), cell_w, cell_h)
}

/// Build the Village-level tab-strip stack's render elements, recursing
/// down the active path (see this module's doc) — empty if `village`
/// isn't a Tab-Village with 2+ children. Returns the elements plus the Y
/// where whatever's next (a deeper level, or `chrome.rs`'s own strip)
/// should start.
pub fn build(village: &mut Village, renderer: &mut GlesRenderer, y: i32, cell_w: usize, cell_h: i32) -> (Vec<Element>, i32) {
    let Village::Tab(tab) = village else {
        return (Vec::new(), y);
    };
    if tab.children.len() < 2 {
        return (Vec::new(), y);
    }

    // Grow the per-child label cache/ids/bg-tracker lazily to match
    // `children` — see `TabVillage::label_cache`'s doc comment; only
    // ever grows here (shrinking happens in `Village::remove_child_hut`,
    // kept in lockstep with `children` itself).
    while tab.label_cache.len() < tab.children.len() {
        tab.label_cache.push(LabelCache::new());
        tab.tab_ids.push((Id::new(), Id::new()));
        tab.bg_tracker.push(ChangeTracker::new());
    }

    let rects = level_layout(&tab.children, y, cell_w, cell_h);
    let mut elements = Vec::new();
    for TabRect { index: i, rect } in rects {
        let active = i == tab.active;
        let (fg, bg) = if active {
            (FG_ACTIVE, BG_ACTIVE)
        } else {
            (FG_INACTIVE, BG_INACTIVE)
        };
        let label = child_label(&tab.children[i]);

        let key = (label.clone(), active);
        let texture = if tab.label_cache[i].is_stale(&key) {
            tab.children[i]
                .focused_hut_mut()
                .render_label(renderer, &label, fg, bg)
                .map(|texture| tab.label_cache[i].store(key, texture))
        } else {
            match tab.label_cache[i].cached() {
                Some(cached) => Ok(cached),
                None => tab.children[i]
                    .focused_hut_mut()
                    .render_label(renderer, &label, fg, bg)
                    .map(|texture| tab.label_cache[i].store(key, texture)),
            }
        };

        let (text_id, bg_id) = tab.tab_ids[i].clone();
        match texture {
            Ok((texture, snapshot)) => {
                let text = TextureRenderElement::from_texture_with_damage(
                    text_id,
                    renderer.context_id(),
                    (
                        (rect.loc.x + TAB_PADDING) as f64,
                        (rect.loc.y + TAB_PADDING) as f64,
                    ),
                    texture,
                    1,
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
            Err(err) => tracing::warn!("failed to render Village tab label {label:?}: {err}"),
        }

        let bg_commit = tab.bg_tracker[i].commit(active);
        let background = SolidColorRenderElement::new(bg_id, rect, bg_commit, to_color32f(bg), Kind::Unspecified);
        elements.push(Element::from(background));
    }

    let (deeper_elements, next_y) = build(&mut tab.children[tab.active], renderer, y + tab_h(cell_h), cell_w, cell_h);
    elements.extend(deeper_elements);
    (elements, next_y)
}
