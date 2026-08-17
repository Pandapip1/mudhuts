//! Compositor-drawn mouse cursor for the udev/DRM backend (the "real
//! xcursor-theme cursor rendering" gap the Phase 7 plan notes deliberately
//! deferred past the initial pass). Loads the user's configured Xcursor
//! theme (respecting `XCURSOR_THEME`/`XCURSOR_SIZE`, same env vars every
//! X11/Wayland app already honors) and composites the current frame's
//! image at the tracked pointer position.
//!
//! Ported from Smithay's own `anvil` demo (`anvil/src/cursor.rs` and
//! `anvil/src/drawing.rs`'s `PointerElement`/`PointerRenderElement`),
//! with its `.unwrap()`/`.expect()` calls replaced by skip-and-log
//! fallbacks per this project's standing no-panics rule.
//!
//! Not used by `winit_backend.rs` at all: under that backend mudhuts is
//! nested inside a host compositor, which already draws a normal cursor
//! for the window — drawing our own on top would be redundant.

use std::time::Duration;

use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::{ImportAll, ImportMem, Renderer, Texture};
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::utils::{Physical, Point, Scale};
use xcursor::CursorTheme;
use xcursor::parser::{Image, parse_xcursor};

/// A tiny procedurally-drawn arrow, used only if the configured Xcursor
/// theme can't be loaded at all (e.g. no theme installed anywhere on the
/// system). Every pixel is grayscale (R=G=B), so getting the exact
/// channel order backwards wouldn't be visually distinguishable —
/// sidesteps needing to verify that precisely for this rarely-hit
/// fallback path.
fn fallback_image() -> Image {
    const SIZE: u32 = 24;
    let mut pixels_rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        let body_width = (y * 2 / 3 + 2).min(SIZE - 2);
        for x in 0..SIZE {
            let inside = x <= body_width;
            let idx = ((y * SIZE + x) * 4) as usize;
            let (gray, alpha) = if !inside {
                (0, 0)
            } else if x == 0 || y == 0 || x == body_width {
                (0, 255)
            } else {
                (255, 255)
            };
            pixels_rgba[idx] = gray;
            pixels_rgba[idx + 1] = gray;
            pixels_rgba[idx + 2] = gray;
            pixels_rgba[idx + 3] = alpha;
        }
    }
    Image {
        size: SIZE,
        width: SIZE,
        height: SIZE,
        xhot: 0,
        yhot: 0,
        delay: 1,
        pixels_rgba,
        pixels_argb: Vec::new(),
    }
}

fn load_icon(theme: &CursorTheme, icon_name: &str) -> Result<Vec<Image>, String> {
    let icon_path = theme
        .load_icon(icon_name)
        .ok_or_else(|| format!("theme has no {icon_name:?} cursor"))?;
    let cursor_data = std::fs::read(&icon_path).map_err(|err| format!("{icon_path:?}: {err}"))?;
    parse_xcursor(&cursor_data).ok_or_else(|| "failed to parse Xcursor file".to_string())
}

/// Loads and picks frames from the user's configured Xcursor theme.
pub struct Cursor {
    icons: Vec<Image>,
    size: u32,
}

impl Cursor {
    /// Loads the theme's image set for a single named shape (a
    /// `cursor-shape-v1` request, or the `Default` shape any freshly
    /// created pointer starts with per `CursorImageStatus::default_named`).
    ///
    /// `icon.name()` is the canonical W3C/CSS name (e.g. `"pointer"`,
    /// `"text"`) that current Xcursor themes ship under, but plenty of
    /// themes still only carry the legacy X11 names (`"hand2"`,
    /// `"xterm"`, ...) `icon.alt_names()` lists — tried in order so a
    /// theme only needs to provide *one* of them. If the configured
    /// theme has none of the names for this particular shape (common for
    /// small/incomplete themes — most ship `"default"` but not every
    /// resize cursor), this falls back to the tiny built-in arrow rather
    /// than leaving the shape unrendered.
    pub fn load(icon: CursorIcon) -> Self {
        let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_string());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        let theme = CursorTheme::load(&theme_name);
        let mut tried = Vec::new();
        let icons = std::iter::once(icon.name())
            .chain(icon.alt_names().iter().copied())
            .find_map(|name| match load_icon(&theme, name) {
                Ok(icons) => Some(icons),
                Err(err) => {
                    tried.push(format!("{name:?}: {err}"));
                    None
                }
            })
            .unwrap_or_else(|| {
                tracing::warn!(
                    "failed to load any of {icon}'s xcursor names from theme {theme_name:?}, \
                     using a built-in fallback cursor ({})",
                    tried.join("; ")
                );
                vec![fallback_image()]
            });

        Self { icons, size }
    }

    /// The frame to show at `time` for the given output `scale` — its
    /// index within this `Cursor`'s own fixed `icons` list alongside the
    /// image itself, so callers can cheaply cache render buffers keyed by
    /// `(icon, index)` instead of comparing full `Image`s (which include
    /// their raw pixel buffers — see `udev_backend.rs`'s
    /// `pointer_image_cache`). `None` if this `Cursor` has no icons at
    /// all — shouldn't happen (`load` always ensures at least the
    /// fallback is present), but stays skip-and-log-safe rather than
    /// assuming that invariant.
    pub fn frame(&self, scale: u32, time: Duration) -> Option<(usize, &Image)> {
        frame(time.as_millis() as u32, self.size * scale, &self.icons)
    }
}

fn nearest_images(size: u32, images: &[Image]) -> Vec<(usize, &Image)> {
    let Some((_, nearest)) = images
        .iter()
        .enumerate()
        .min_by_key(|(_, image)| (size as i32 - image.size as i32).abs())
    else {
        return Vec::new();
    };
    images
        .iter()
        .enumerate()
        .filter(|(_, image)| image.width == nearest.width && image.height == nearest.height)
        .collect()
}

fn frame(mut millis: u32, size: u32, images: &[Image]) -> Option<(usize, &Image)> {
    let candidates = nearest_images(size, images);
    if candidates.is_empty() {
        return None;
    }
    let total: u32 = candidates.iter().map(|(_, image)| image.delay).sum();
    if total == 0 {
        return candidates.first().copied();
    }
    millis %= total;
    for &(idx, image) in &candidates {
        if millis < image.delay {
            return Some((idx, image));
        }
        millis -= image.delay;
    }
    // Unreachable in practice (the loop above always finds a match once
    // `millis` has been reduced modulo `total`) — falling back to the
    // first candidate instead of an `unreachable!()` panic costs nothing.
    candidates.first().copied()
}

smithay::backend::renderer::element::render_elements! {
    pub PointerRenderElement<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Memory = MemoryRenderBufferRenderElement<R>,
}

/// Tracks the pointer's current on-screen appearance: a client-requested
/// [`CursorImageStatus`] plus (for the `Named` case) the current
/// xcursor-theme frame's uploaded texture — set once per render pass by
/// `udev_backend.rs`, which owns the frame-to-buffer cache.
pub struct PointerElement {
    buffer: Option<MemoryRenderBuffer>,
    status: CursorImageStatus,
}

impl Default for PointerElement {
    fn default() -> Self {
        Self {
            buffer: None,
            status: CursorImageStatus::default_named(),
        }
    }
}

impl PointerElement {
    pub fn set_status(&mut self, status: CursorImageStatus) {
        self.status = status;
    }

    pub fn set_buffer(&mut self, buffer: MemoryRenderBuffer) {
        self.buffer = Some(buffer);
    }
}

impl<T, R> AsRenderElements<R> for PointerElement
where
    T: Texture + Clone + Send + 'static,
    R: Renderer<TextureId = T> + ImportAll + ImportMem,
{
    type RenderElement = PointerRenderElement<R>;

    fn render_elements<E>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<E>
    where
        E: From<PointerRenderElement<R>>,
    {
        match &self.status {
            CursorImageStatus::Hidden => Vec::new(),
            // The client wants the compositor's own default cursor —
            // that's the xcursor-theme buffer `udev_backend.rs` keeps
            // updated via `set_buffer`.
            CursorImageStatus::Named(_) => {
                let Some(buffer) = self.buffer.as_ref() else {
                    return Vec::new();
                };
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location.to_f64(),
                    buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(element) => vec![E::from(PointerRenderElement::<R>::from(element))],
                    Err(err) => {
                        tracing::warn!("failed to import the cursor buffer: {err:?}");
                        Vec::new()
                    }
                }
            }
            CursorImageStatus::Surface(surface) => {
                render_elements_from_surface_tree(renderer, surface, location, scale, alpha, Kind::Cursor)
                    .into_iter()
                    .map(|el: PointerRenderElement<R>| E::from(el))
                    .collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(size: u32, width: u32, height: u32, delay: u32) -> Image {
        Image {
            size,
            width,
            height,
            xhot: 0,
            yhot: 0,
            delay,
            pixels_rgba: Vec::new(),
            pixels_argb: Vec::new(),
        }
    }

    #[test]
    fn nearest_images_of_no_images_is_empty() {
        assert!(nearest_images(24, &[]).is_empty());
    }

    #[test]
    fn nearest_images_picks_the_closest_size() {
        let images = vec![img(16, 16, 16, 0), img(32, 32, 32, 0), img(48, 48, 48, 0)];
        let nearest = nearest_images(30, &images);
        assert_eq!(nearest.len(), 1);
        assert_eq!(nearest[0].0, 1);
    }

    #[test]
    fn nearest_images_includes_every_frame_matching_the_nearest_dimensions() {
        // An animated cursor's frames all share the same size but have
        // distinct delays — the whole point of returning a `Vec` rather
        // than a single nearest match.
        let images = vec![
            img(24, 24, 24, 100),
            img(24, 24, 24, 100),
            img(48, 48, 48, 100),
        ];
        let nearest = nearest_images(20, &images);
        assert_eq!(nearest.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn frame_of_no_images_is_none() {
        assert!(frame(0, 24, &[]).is_none());
    }

    #[test]
    fn frame_picks_the_candidate_whose_delay_window_contains_the_elapsed_time() {
        let images = vec![img(24, 24, 24, 100), img(24, 24, 24, 100), img(24, 24, 24, 100)];
        assert_eq!(frame(0, 24, &images).map(|(i, _)| i), Some(0));
        assert_eq!(frame(150, 24, &images).map(|(i, _)| i), Some(1));
        assert_eq!(frame(250, 24, &images).map(|(i, _)| i), Some(2));
    }

    #[test]
    fn frame_wraps_around_via_modulo_of_the_total_delay() {
        let images = vec![img(24, 24, 24, 100), img(24, 24, 24, 100)];
        // Total delay is 200ms; 250ms should behave the same as 50ms.
        assert_eq!(frame(250, 24, &images).map(|(i, _)| i), frame(50, 24, &images).map(|(i, _)| i));
    }

    #[test]
    fn frame_falls_back_to_the_first_candidate_when_total_delay_is_zero() {
        let images = vec![img(24, 24, 24, 0), img(24, 24, 24, 0)];
        assert_eq!(frame(9999, 24, &images).map(|(i, _)| i), Some(0));
    }
}
