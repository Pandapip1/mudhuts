//! Phase 4's tab-strip chrome: a horizontal strip at the top of the
//! screen showing the focused ConsoleHut's "Terminal" tab plus one tab per Main
//! Window, highlighting whichever is active. Shown only when the focused
//! ConsoleHut has at least one Main Window — an empty ConsoleHut has nothing to switch
//! between, so no chrome to draw (matches `Ctrl+`` being a no-op there
//! too).

use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::Window;
use smithay::utils::{Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use mudhuts_term::palette::Rgb;

use crate::console_hut::ConsoleHut;
use crate::render::OutputRenderElements;
use crate::space_element::HutSpaceRenderElement;
use crate::theme::Theme;

/// Base sizes (scale 1.0) — scaled via `crate::render::scaled` wherever
/// they're actually used, so this chrome stays the same apparent size
/// regardless of the output's real DPI scale. `TAB_PADDING` alone is
/// `pub(crate)`: `village_chrome.rs`'s `build` also needs it directly
/// (to offset a label within its tab rect), while `TAB_GAP`/
/// `LEFT_MARGIN` stay private — every other consumer of this chrome's
/// exact layout goes through `tab_row_layout`, which already bakes them
/// in.
pub(crate) const TAB_PADDING: i32 = 12;
const TAB_GAP: i32 = 4;
const LEFT_MARGIN: i32 = 16;
const MAX_TITLE_CHARS: usize = 24;

type Element = OutputRenderElements<GlesRenderer, HutSpaceRenderElement>;

/// Shorten `s` to at most `max_chars` characters, replacing the last one
/// with an ellipsis if it was longer — shared by [`window_title`] (own
/// `MAX_TITLE_CHARS`, `24`) and `docks.rs::truncate` (its own narrower
/// `MAX_TITLE_CHARS`, `18` — a dock handle has less width to work with
/// than a tab), which independently reimplemented the identical
/// char-count-then-ellipsis algorithm before review caught the
/// duplication.
pub(crate) fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}\u{2026}")
    } else {
        s.to_string()
    }
}

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

    truncate_with_ellipsis(&title, MAX_TITLE_CHARS)
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
    tab_h(hut.glyphs.cell_height().max(1) as i32, scale)
}

/// One tab's clickable/drawable rectangle, plus which tab it is — `0` is
/// always the "Terminal" tab, `1..` follow `hut.main_windows()`'s order
/// (matching `build`'s own indexing). Shared between `build` (rendering)
/// and `input.rs` (click hit-testing), so the two can never disagree
/// about where a tab actually is — the same reasoning as `docks::Handle`.
/// Also reused as-is by `village_chrome.rs`'s Hut-level tab strips
/// (`tab_row_layout`), which lay out the exact same shape of thing one
/// level up the Tab-Hut hierarchy.
pub struct TabRect {
    pub index: usize,
    pub rect: Rectangle<i32, Physical>,
}

/// One tab-strip row's height for a strip whose cells are `cell_h`
/// physical pixels tall — shared by `chrome.rs`'s own per-ConsoleHut
/// strip (`strip_height`) and `village_chrome.rs`'s per-Tab-Hut-level
/// strips (`village_chrome::stack_height`), which must agree on this
/// exactly or the two kinds of strip would visibly mismatch in height
/// when stacked on top of each other.
pub(crate) fn tab_h(cell_h: i32, scale: f64) -> i32 {
    cell_h + crate::render::scaled(TAB_PADDING, scale) * 2
}

/// Lay out one row of tabs, left to right starting at physical-pixel row
/// `y`, sized to fit each tab's label — given as `char_counts` rather
/// than the label text itself, since layout only ever needs a label's
/// length, never its content — at `cell_w` pixels/char plus padding.
/// The shared arithmetic behind both `tab_layout` (this module, one row
/// per ConsoleHut's Main Windows) and `village_chrome.rs`'s `build`/
/// `handle_click` (one row per Tab-Hut level's children), which need to
/// lay out visually-identical strips over two different label sources.
/// Empty `char_counts` produces an empty result.
pub(crate) fn tab_row_layout(char_counts: &[usize], y: i32, cell_w: usize, cell_h: i32, scale: f64) -> Vec<TabRect> {
    let h = tab_h(cell_h, scale);
    let padding = crate::render::scaled(TAB_PADDING, scale);
    let gap = crate::render::scaled(TAB_GAP, scale);
    let mut rects = Vec::new();
    let mut x = crate::render::scaled(LEFT_MARGIN, scale);
    for (i, &count) in char_counts.iter().enumerate() {
        let label_w = (count.max(1) * cell_w) as i32;
        let tab_w = label_w + padding * 2;
        rects.push(TabRect {
            index: i,
            rect: Rectangle::new(Point::from((x, y)), Size::from((tab_w, h))),
        });
        x += tab_w + gap;
    }
    rects
}

/// Compute this ConsoleHut's tab strip layout, starting at physical-pixel row
/// `y` (pushed down by however many Hut-level tab strips are stacked
/// above it — see `village_chrome.rs`'s module doc) — empty if there's
/// nothing to show. Only needs each label's length (for `tab_row_layout`),
/// never its text, so callers that only want hit-test/geometry rects
/// (`input.rs`) don't pay for a `Vec<String>` they'd throw away; `build`
/// below computes labels itself instead of going through this, since it
/// actually needs the text too and doing both in the same pass avoids
/// calling `window_title` twice per window.
pub fn tab_layout(hut: &ConsoleHut, y: i32, scale: f64) -> Vec<TabRect> {
    if hut.main_window_count() == 0 {
        return Vec::new();
    }
    let cell_w = hut.glyphs.cell_width().max(1);
    let cell_h = hut.glyphs.cell_height().max(1) as i32;

    let mut char_counts = vec!["Terminal".chars().count()];
    char_counts.extend(hut.main_windows().iter().map(|entry| window_title(&entry.window).chars().count()));

    tab_row_layout(&char_counts, y, cell_w, cell_h, scale)
}

/// Build the tab strip's render elements in front-to-back order, starting
/// at physical-pixel row `y`, or an empty list if the focused ConsoleHut has no
/// Main Windows.
pub fn build(hut: &mut ConsoleHut, renderer: &mut GlesRenderer, y: i32, scale: f64, theme: &Theme) -> Vec<Element> {
    if hut.main_window_count() == 0 {
        return Vec::new();
    }
    let cell_w = hut.glyphs.cell_width().max(1);
    let cell_h = hut.glyphs.cell_height().max(1) as i32;

    // Computed once here (not via `tab_layout`, which would need to call
    // `window_title` again just to get lengths) and reused below both for
    // layout and for the actual rendered text — `window_title` reads
    // Wayland toplevel surface state via `with_states`, not free, so it's
    // worth not paying for it twice per window per frame.
    let mut labels = vec!["Terminal".to_string()];
    labels.extend(hut.main_windows().iter().map(|entry| window_title(&entry.window)));
    let char_counts: Vec<usize> = labels.iter().map(|label| label.chars().count()).collect();

    let rects = tab_row_layout(&char_counts, y, cell_w, cell_h, scale);
    if rects.is_empty() {
        return Vec::new();
    }
    let padding = crate::render::scaled(TAB_PADDING, scale);

    let active_index = if *hut.showing_terminal {
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
        let label = labels[i].clone();
        let active = i == active_index;
        let (fg, bg) = if active {
            (theme.tab_active_fg, theme.tab_active_bg)
        } else {
            (theme.tab_inactive_fg, theme.tab_inactive_bg)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_h_adds_padding_on_both_sides_of_the_cell_height() {
        assert_eq!(tab_h(20, 1.0), 20 + TAB_PADDING * 2);
    }

    #[test]
    fn tab_h_scales_the_padding_but_not_the_cell_height() {
        // `cell_h` already comes in as physical pixels for the actual
        // (already-scaled) font — only the constant padding around it
        // needs its own separate scaling.
        assert_eq!(tab_h(20, 2.0), 20 + TAB_PADDING * 2 * 2);
    }

    #[test]
    fn tab_row_layout_of_no_labels_is_empty() {
        assert!(tab_row_layout(&[], 0, 8, 20, 1.0).is_empty());
    }

    #[test]
    fn tab_row_layout_starts_at_the_left_margin_and_the_given_row() {
        let rects = tab_row_layout(&[1], 40, 8, 20, 1.0);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].index, 0);
        assert_eq!(rects[0].rect.loc, Point::from((LEFT_MARGIN, 40)));
        assert_eq!(rects[0].rect.size.h, tab_h(20, 1.0));
    }

    #[test]
    fn tab_row_layout_widens_for_longer_labels() {
        let rects = tab_row_layout(&[1, "much longer label".chars().count()], 0, 8, 20, 1.0);
        assert!(rects[1].rect.size.w > rects[0].rect.size.w);
    }

    #[test]
    fn tab_row_layout_places_each_tab_after_the_previous_ones_gap() {
        let rects = tab_row_layout(&[1, 1], 0, 8, 20, 1.0);
        let first = &rects[0].rect;
        let second = &rects[1].rect;
        assert_eq!(second.loc.x, first.loc.x + first.size.w + TAB_GAP);
    }

    #[test]
    fn tab_row_layout_indexes_rects_in_label_order() {
        let rects = tab_row_layout(&[1, 1, 1], 0, 8, 20, 1.0);
        assert_eq!(rects.iter().map(|r| r.index).collect::<Vec<_>>(), vec![0, 1, 2]);
    }
}
