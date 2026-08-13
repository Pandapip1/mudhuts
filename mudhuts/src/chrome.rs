//! Phase 4's tab-strip chrome: a horizontal strip at the top of the
//! screen showing the focused ConsoleHut's "Terminal" tab plus one tab per Main
//! Window, highlighting whichever is active. Shown only when the focused
//! ConsoleHut has at least one Main Window — an empty ConsoleHut has nothing to switch
//! between, so no chrome to draw (matches `Ctrl+`` being a no-op there
//! too).

use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::Window;
use smithay::utils::{Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use mudhuts_term::palette::Rgb;

use crate::console_hut::ConsoleHut;
use crate::render::OutputRenderElements;

/// Base sizes (scale 1.0) — scaled via `crate::render::scaled` wherever
/// they're actually used, so this chrome stays the same apparent size
/// regardless of the output's real DPI scale.
const TAB_PADDING: i32 = 12;
const TAB_GAP: i32 = 4;
const LEFT_MARGIN: i32 = 16;
const MAX_TITLE_CHARS: usize = 24;

const FG_ACTIVE: Rgb = [255, 255, 255];
const BG_ACTIVE: Rgb = [64, 115, 191];
const FG_INACTIVE: Rgb = [190, 190, 190];
const BG_INACTIVE: Rgb = [30, 30, 30];

type Element = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// Read a toplevel's title live (not cached/event-driven), falling back
/// to its app_id, then a placeholder — the same `with_states`/
/// `XdgToplevelSurfaceData` mechanism `handlers/xdg_shell.rs`'s
/// `handle_commit` already relies on for `initial_configure_sent`.
pub(crate) fn window_title(window: &Window) -> String {
    let Some(toplevel) = window.toplevel() else {
        return "(window)".to_string();
    };
    let title = with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| {
                data.lock()
                    .ok()
                    .and_then(|guard| guard.title.clone().or_else(|| guard.app_id.clone()))
            })
    })
    .unwrap_or_else(|| "(window)".to_string());

    if title.chars().count() > MAX_TITLE_CHARS {
        let truncated: String = title
            .chars()
            .take(MAX_TITLE_CHARS.saturating_sub(1))
            .collect();
        format!("{truncated}\u{2026}")
    } else {
        title
    }
}

/// Read a toplevel's app_id live — unlike [`window_title`], never falls
/// back to anything else (an empty string if the client never set one,
/// matching `ext_foreign_toplevel_list_v1`'s own convention for
/// `app_id`) — for `handlers/xdg_shell.rs`'s `new_toplevel`, which needs
/// title and app_id as two genuinely separate strings when creating this
/// Main Window's `ForeignToplevelHandle`.
pub(crate) fn window_app_id(window: &Window) -> String {
    let Some(toplevel) = window.toplevel() else {
        return String::new();
    };
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|guard| guard.app_id.clone()))
    })
    .unwrap_or_default()
}

pub(crate) fn to_color32f(rgb: Rgb) -> [f32; 4] {
    [
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
        1.0,
    ]
}

/// This tab strip's height in physical pixels — `0` if there's nothing
/// to show (no Main Windows), so callers (`render.rs`'s Hut-level
/// stacking, `input.rs`'s click hit-testing) don't need their own
/// separate "is there a strip at all" check.
pub fn strip_height(hut: &ConsoleHut, scale: f64) -> i32 {
    if hut.main_window_count() == 0 {
        return 0;
    }
    hut.glyphs.cell_height().max(1) as i32 + crate::render::scaled(TAB_PADDING, scale) * 2
}

/// One tab's clickable/drawable rectangle, plus which tab it is — `0` is
/// always the "Terminal" tab, `1..` follow `hut.main_windows()`'s order
/// (matching `build`'s own indexing). Shared between `build` (rendering)
/// and `input.rs` (click hit-testing), so the two can never disagree
/// about where a tab actually is — the same reasoning as `docks::Handle`.
pub struct TabRect {
    pub index: usize,
    pub rect: Rectangle<i32, Physical>,
}

/// Compute this ConsoleHut's tab strip layout, starting at physical-pixel row
/// `y` (pushed down by however many Hut-level tab strips are stacked
/// above it — see `village_chrome.rs`'s module doc) — empty if there's
/// nothing to show.
pub fn tab_layout(hut: &ConsoleHut, y: i32, scale: f64) -> Vec<TabRect> {
    if hut.main_window_count() == 0 {
        return Vec::new();
    }
    let cell_w = hut.glyphs.cell_width().max(1);
    let tab_h = strip_height(hut, scale);
    let padding = crate::render::scaled(TAB_PADDING, scale);
    let gap = crate::render::scaled(TAB_GAP, scale);

    let mut labels = vec!["Terminal".to_string()];
    labels.extend(hut.main_windows().iter().map(|entry| window_title(&entry.window)));

    let mut rects = Vec::new();
    let mut x = crate::render::scaled(LEFT_MARGIN, scale);
    for (i, label) in labels.iter().enumerate() {
        let label_w = (label.chars().count().max(1) * cell_w) as i32;
        let tab_w = label_w + padding * 2;
        rects.push(TabRect {
            index: i,
            rect: Rectangle::new(Point::from((x, y)), Size::from((tab_w, tab_h))),
        });
        x += tab_w + gap;
    }
    rects
}

/// Build the tab strip's render elements in front-to-back order, starting
/// at physical-pixel row `y`, or an empty list if the focused ConsoleHut has no
/// Main Windows.
pub fn build(hut: &mut ConsoleHut, renderer: &mut GlesRenderer, y: i32, scale: f64) -> Vec<Element> {
    let rects = tab_layout(hut, y, scale);
    if rects.is_empty() {
        return Vec::new();
    }
    let padding = crate::render::scaled(TAB_PADDING, scale);

    let active_index = if hut.showing_terminal {
        0
    } else {
        1 + hut.active_main_window_index()
    };

    // Stable per-tab element ids, matching each label 1:1 — index 0 is
    // always the "Terminal" tab (`ConsoleHut`'s own cached ids), the rest follow
    // `hut.main_windows()`'s order. Must stay stable across frames (not
    // freshly generated per call) or the outer damage tracker sees a "new"
    // element every frame instead of recognizing it as the same one.
    let mut text_ids = vec![hut.terminal_tab_text_id.clone()];
    let mut bg_ids = vec![hut.terminal_tab_bg_id.clone()];
    text_ids.extend(hut.main_windows().iter().map(|entry| entry.tab_text_id.clone()));
    bg_ids.extend(hut.main_windows().iter().map(|entry| entry.tab_bg_id.clone()));

    let mut elements = Vec::new();
    for TabRect { index: i, rect } in rects {
        let label = if i == 0 {
            "Terminal".to_string()
        } else {
            window_title(&hut.main_windows()[i - 1].window)
        };
        let active = i == active_index;
        let (fg, bg) = if active {
            (FG_ACTIVE, BG_ACTIVE)
        } else {
            (FG_INACTIVE, BG_INACTIVE)
        };

        // Only actually re-renders (real GPU work: glyph-atlas lookups
        // plus instanced draw calls into an FBO) when this tab's
        // label/active-state changed since last frame — every other
        // frame reuses the cached texture instead of paying that cost
        // again for a label that hasn't visibly changed (see
        // `render::LabelCache`'s doc comment).
        let label_texture = if i == 0 {
            hut.terminal_tab_label(renderer, active, fg, bg)
        } else {
            let idx = i - 1;
            let key = (label.clone(), active);
            if hut.main_windows()[idx].tab_text_cache.is_stale(&key) {
                hut.render_label(renderer, &label, fg, bg)
                    .map(|texture| hut.main_windows_mut()[idx].tab_text_cache.store(key, texture))
            } else {
                match hut.main_windows()[idx].tab_text_cache.cached() {
                    Some(cached) => Ok(cached),
                    None => hut
                        .render_label(renderer, &label, fg, bg)
                        .map(|texture| hut.main_windows_mut()[idx].tab_text_cache.store(key, texture)),
                }
            }
        };

        match label_texture {
            Ok((texture, snapshot)) => {
                let text = TextureRenderElement::from_texture_with_damage(
                    text_ids[i].clone(),
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
            Err(err) => tracing::warn!("failed to render tab label {label:?}: {err}"),
        }

        // Same reasoning as above: a fixed `CommitCounter::default()`
        // would mean this tab's background never visibly updates again
        // after its first frame, even when it flips active/inactive.
        let bg_commit = if i == 0 {
            hut.terminal_tab_bg_commit(active)
        } else {
            hut.main_windows_mut()[i - 1].tab_bg_commit(active)
        };
        let background = SolidColorRenderElement::new(
            bg_ids[i].clone(),
            rect,
            bg_commit,
            to_color32f(bg),
            Kind::Unspecified,
        );
        elements.push(Element::from(background));
    }

    elements
}
