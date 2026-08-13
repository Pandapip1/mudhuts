//! Phase 4's tab-strip chrome: a horizontal strip at the top of the
//! screen showing the focused Hut's "Terminal" tab plus one tab per Main
//! Window, highlighting whichever is active. Shown only when the focused
//! Hut has at least one Main Window — an empty Hut has nothing to switch
//! between, so no chrome to draw (matches `Ctrl+`` being a no-op there
//! too).

use smithay::backend::renderer::{Renderer, Texture};
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

use crate::hut::Hut;
use crate::render::OutputRenderElements;

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

pub(crate) fn to_color32f(rgb: Rgb) -> [f32; 4] {
    [
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
        1.0,
    ]
}

/// Build the tab strip's render elements in front-to-back order, or an
/// empty list if the focused Hut has no Main Windows.
pub fn build(hut: &mut Hut, renderer: &mut GlesRenderer) -> Vec<Element> {
    if hut.main_window_count() == 0 {
        return Vec::new();
    }

    let active_index = if hut.showing_terminal {
        0
    } else {
        1 + hut.active_main_window_index()
    };

    let mut labels = vec!["Terminal".to_string()];
    labels.extend(hut.main_windows().iter().map(|entry| window_title(&entry.window)));

    // Stable per-tab element ids, matching each label 1:1 — index 0 is
    // always the "Terminal" tab (`Hut`'s own cached ids), the rest follow
    // `hut.main_windows()`'s order. Must stay stable across frames (not
    // freshly generated per call) or the outer damage tracker sees a "new"
    // element every frame instead of recognizing it as the same one.
    let mut text_ids = vec![hut.terminal_tab_text_id.clone()];
    let mut bg_ids = vec![hut.terminal_tab_bg_id.clone()];
    text_ids.extend(hut.main_windows().iter().map(|entry| entry.tab_text_id.clone()));
    bg_ids.extend(hut.main_windows().iter().map(|entry| entry.tab_bg_id.clone()));

    let cell_w = hut.glyphs.cell_width().max(1);
    let cell_h = hut.glyphs.cell_height().max(1) as i32;
    let tab_h = cell_h + TAB_PADDING * 2;

    let mut elements = Vec::new();
    let mut x = LEFT_MARGIN;
    for (i, label) in labels.iter().enumerate() {
        let active = i == active_index;
        let (fg, bg) = if active {
            (FG_ACTIVE, BG_ACTIVE)
        } else {
            (FG_INACTIVE, BG_INACTIVE)
        };
        let label_w = (label.chars().count().max(1) * cell_w) as i32;
        let tab_w = label_w + TAB_PADDING * 2;

        match hut.render_label(renderer, label, fg, bg) {
            Ok(texture) => {
                // Real damage tracking, not a fixed default snapshot —
                // this texture is rebuilt fresh every call regardless of
                // whether `label`/`active` actually changed since last
                // frame; without this, the outer tracker would never see
                // this tab's text change again after its first frame (see
                // `render::TextureChangeTracker`'s doc comment).
                let texture_size = texture.size();
                let snapshot = if i == 0 {
                    hut.terminal_tab_text_snapshot(active, texture_size)
                } else {
                    hut.main_windows_mut()[i - 1].tab_text_snapshot(label, active, texture_size)
                };
                let text = TextureRenderElement::from_texture_with_damage(
                    text_ids[i].clone(),
                    renderer.context_id(),
                    ((x + TAB_PADDING) as f64, TAB_PADDING as f64),
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
            Rectangle::<i32, Physical>::new(Point::from((x, 0)), Size::from((tab_w, tab_h))),
            bg_commit,
            to_color32f(bg),
            Kind::Unspecified,
        );
        elements.push(Element::from(background));

        x += tab_w + TAB_GAP;
    }

    elements
}
