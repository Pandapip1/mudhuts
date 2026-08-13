//! Phase 6's Village-level tab-strip chrome: a horizontal strip along the
//! *bottom* of the screen, one tab per child, shown only when the
//! focused top-level Village is a Tab-Village with 2+ children — distinct
//! from `chrome.rs`'s own per-Hut Main-Window tab strip (always at the
//! *top*), which this can be shown alongside without colliding (matches
//! the established "chrome overlays whatever's underneath, doesn't
//! reflow it" pattern already used everywhere else — see `chrome.rs`'s
//! module doc).
//!
//! Only a Tab-Village gets a strip — a Tile-Village's panes are all
//! visible simultaneously (see `render.rs`'s Tile-Village compositing),
//! so there's nothing to switch *to* the way a tab implies.

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
use crate::stack::HutStack;
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

/// What to show on a child's tab — its currently-effective Hut's active
/// view: the active Main Window's title if it's showing one, else
/// "Terminal". Same fallback chain `chrome::window_title` already uses
/// for a single Hut's own Main-Window tabs, one level up.
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

/// Build the Village-level tab strip's render elements, or an empty list
/// if the focused top-level Village isn't a Tab-Village with 2+ children.
pub fn build(stack: &mut HutStack, renderer: &mut GlesRenderer, output_size: (i32, i32)) -> Vec<Element> {
    let cell_w = stack.focused().glyphs.cell_width().max(1);
    let cell_h = stack.focused().glyphs.cell_height().max(1) as i32;

    let Village::Tab(tab) = stack.focused_village_mut() else {
        return Vec::new();
    };
    if tab.children.len() < 2 {
        return Vec::new();
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

    let tab_h = cell_h + TAB_PADDING * 2;
    let y = output_size.1 - tab_h;

    let mut elements = Vec::new();
    let mut x = LEFT_MARGIN;
    for i in 0..tab.children.len() {
        let active = i == tab.active;
        let (fg, bg) = if active {
            (FG_ACTIVE, BG_ACTIVE)
        } else {
            (FG_INACTIVE, BG_INACTIVE)
        };
        let label = child_label(&tab.children[i]);
        let label_w = (label.chars().count().max(1) * cell_w) as i32;
        let tab_w = label_w + TAB_PADDING * 2;

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
                    ((x + TAB_PADDING) as f64, (y + TAB_PADDING) as f64),
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
        let background = SolidColorRenderElement::new(
            bg_id,
            Rectangle::<i32, Physical>::new(Point::from((x, y)), Size::from((tab_w, tab_h))),
            bg_commit,
            to_color32f(bg),
            Kind::Unspecified,
        );
        elements.push(Element::from(background));

        x += tab_w + TAB_GAP;
    }

    elements
}
