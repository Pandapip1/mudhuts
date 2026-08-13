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
//! practice this never happens, since [`Hut::collapse_if_singleton`]
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
use crate::hut::Hut;

/// Base sizes (scale 1.0) — scaled via `crate::render::scaled` wherever
/// they're actually used, matching `chrome.rs`'s own constants.
const TAB_PADDING: i32 = 12;
const TAB_GAP: i32 = 4;
const LEFT_MARGIN: i32 = 16;
const MAX_TITLE_CHARS: usize = 24;

const FG_ACTIVE: Rgb = [255, 255, 255];
const BG_ACTIVE: Rgb = [140, 90, 191];
const FG_INACTIVE: Rgb = [190, 190, 190];
const BG_INACTIVE: Rgb = [40, 30, 50];

type Element = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// One tab's clickable/drawable rectangle within a single Tab-Hut
/// level, plus which child it is (an index into that level's `children`).
pub struct TabRect {
    pub index: usize,
    pub rect: Rectangle<i32, Physical>,
}

/// What to show on a child's tab — its currently-effective ConsoleHut's active
/// view: the active Main Window's title if it's showing one, else
/// "Terminal". Same fallback chain `chrome::window_title` already uses
/// for a single ConsoleHut's own Main-Window tabs, one level down.
fn child_label(village: &Hut) -> String {
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

fn tab_h(cell_h: i32, scale: f64) -> i32 {
    cell_h + crate::render::scaled(TAB_PADDING, scale) * 2
}

/// One Tab-Hut level's tab rects, at physical-pixel row `y` — shared
/// between rendering and click hit-testing (see this module's doc), so
/// the two can never disagree about where a tab actually is.
fn level_layout(children: &[Hut], y: i32, cell_w: usize, cell_h: i32, scale: f64) -> Vec<TabRect> {
    let h = tab_h(cell_h, scale);
    let padding = crate::render::scaled(TAB_PADDING, scale);
    let gap = crate::render::scaled(TAB_GAP, scale);
    let mut rects = Vec::new();
    let mut x = crate::render::scaled(LEFT_MARGIN, scale);
    for (i, child) in children.iter().enumerate() {
        let label = child_label(child);
        let label_w = (label.chars().count().max(1) * cell_w) as i32;
        let tab_w = label_w + padding * 2;
        rects.push(TabRect {
            index: i,
            rect: Rectangle::new(Point::from((x, y)), Size::from((tab_w, h))),
        });
        x += tab_w + gap;
    }
    rects
}

/// Total height of the Hut-level tab-strip stack along the active
/// path from `village` down — `0` if `village` isn't a Tab-Hut with
/// 2+ children (nothing to stack). Used by `render.rs` to know where
/// `chrome.rs`'s own strip (and, when not tiled/tabbed at all, the
/// terminal/window content itself) should start.
pub fn stack_height(village: &Hut, cell_h: i32, scale: f64) -> i32 {
    match village {
        Hut::Tab(tab) if tab.children.len() >= 2 => {
            tab_h(cell_h, scale) + stack_height(&tab.children[tab.active], cell_h, scale)
        }
        _ => 0,
    }
}

/// Hit-test a click position (physical pixels) against the Hut-level
/// tab-strip hierarchy, recursing down the active path the same way
/// [`build`] does. On a hit, switches that level's `active` index and
/// returns `true` (the caller should re-sync visible content/focus and
/// redraw); `false` if the click didn't land on any Hut-level tab —
/// the tile-pane/ConsoleHut-level click handling should take over instead.
pub fn handle_click(
    village: &mut Hut,
    pos: (i32, i32),
    y: i32,
    cell_w: usize,
    cell_h: i32,
    scale: f64,
) -> bool {
    let Hut::Tab(tab) = village else {
        return false;
    };
    if tab.children.len() < 2 {
        return false;
    }
    let point = Point::from(pos);
    for TabRect { index: i, rect } in level_layout(&tab.children, y, cell_w, cell_h, scale) {
        if rect.contains(point) {
            tab.active = i;
            return true;
        }
    }
    handle_click(
        &mut tab.children[tab.active],
        pos,
        y + tab_h(cell_h, scale),
        cell_w,
        cell_h,
        scale,
    )
}

/// Build the Hut-level tab-strip stack's render elements, recursing
/// down the active path (see this module's doc) — empty if `village`
/// isn't a Tab-Hut with 2+ children. Returns the elements plus the Y
/// where whatever's next (a deeper level, or `chrome.rs`'s own strip)
/// should start.
pub fn build(
    village: &mut Hut,
    renderer: &mut GlesRenderer,
    y: i32,
    cell_w: usize,
    cell_h: i32,
    scale: f64,
) -> (Vec<Element>, i32) {
    let Hut::Tab(tab) = village else {
        return (Vec::new(), y);
    };
    if tab.children.len() < 2 {
        return (Vec::new(), y);
    }

    // Grow the per-child label cache/ids/bg-tracker lazily to match
    // `children` — see `TabVillage::label_cache`'s doc comment; only
    // ever grows here (shrinking happens in `Hut::remove_child_hut`,
    // kept in lockstep with `children` itself).
    while tab.label_cache.len() < tab.children.len() {
        tab.label_cache.push(LabelCache::new());
        tab.tab_ids.push((Id::new(), Id::new()));
        tab.bg_tracker.push(ChangeTracker::new());
    }

    let rects = level_layout(&tab.children, y, cell_w, cell_h, scale);
    let padding = crate::render::scaled(TAB_PADDING, scale);
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
                        (rect.loc.x + padding) as f64,
                        (rect.loc.y + padding) as f64,
                    ),
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

        let bg_commit = tab.bg_tracker[i].commit(active);
        let background = SolidColorRenderElement::new(bg_id, rect, bg_commit, to_color32f(bg), Kind::Unspecified);
        elements.push(Element::from(background));
    }

    let (deeper_elements, next_y) = build(
        &mut tab.children[tab.active],
        renderer,
        y + tab_h(cell_h, scale),
        cell_w,
        cell_h,
        scale,
    );
    elements.extend(deeper_elements);
    (elements, next_y)
}
