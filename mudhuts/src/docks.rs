//! Phase 5's docked-handle chrome: a small labeled tab near whichever
//! edge each of the focused Hut's active Main Window's docked
//! Sub-Windows is minimized to, plus the compositor-native drag that
//! turns one into a floating window for the first time.
//!
//! A docked Sub-Window isn't mapped as a real surface at all — nothing to
//! composite, and nothing for a client's own CSD to be dragged from — so
//! there's no `xdg_toplevel.move` grab to hook into for *this* side of
//! the interaction (see `grabs.rs` for the other side: once a Sub-Window
//! is already floating, further drags go through a real `PointerGrab`).
//! Instead, the drag from a handle is tracked directly in `State`, the
//! same way `text_selecting` tracks a plain terminal-selection drag —
//! not a full `PointerGrab`, since there's no client surface/serial to
//! grab in the first place until the window is actually mapped.
//!
//! Handle layout/hit-testing (this module) is genuinely [`Physical`] —
//! the handle is mudhuts' own drawn chrome, pixel-native like
//! `chrome.rs`'s tab strip, not a real surface Smithay's `Space` knows
//! about. Only once a drag actually detaches and the Sub-Window becomes
//! a real mapped element (`Dock::Floating`, read/written by
//! `sync_visible_main_window`/`grabs.rs`'s `MoveSurfaceGrab` too) does
//! its position need to cross over into genuinely `Logical` space to
//! match `self.space`'s own contract — see [`advance_drag`]/[`finish_drag`]
//! for exactly where that conversion happens.

use smithay::backend::renderer::{Renderer, Texture};
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{CommitCounter, DamageSnapshot};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::State;
use crate::chrome::{to_color32f, window_title};
use crate::grabs::nearest_edge_within_threshold;
use crate::hut::Hut;
use crate::main_window::{Dock, Edge};
use crate::render::OutputRenderElements;

const HANDLE_W: i32 = 140;
const HANDLE_H: i32 = 28;
const HANDLE_GAP: i32 = 4;
/// Keep clear of `chrome.rs`'s tab strip and the screen corners.
const EDGE_MARGIN: i32 = 40;
const MAX_TITLE_CHARS: usize = 18;

const FG: mudhuts_term::palette::Rgb = [220, 220, 220];
const BG: mudhuts_term::palette::Rgb = [50, 50, 60];

/// How far (in physical pixels — this module's native space, see the
/// module doc) a drag has to travel from a handle before it detaches
/// into a floating window, rather than being read as just a click.
const DETACH_THRESHOLD: f64 = 12.0;

type Element = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// Tracks a click-and-drag on a docked Sub-Window's handle. Lives in
/// `State::dock_drag`; not a `PointerGrab` — see the module doc.
pub struct DockDrag {
    pub surface: WlSurface,
    /// Pointer location when the drag started, for measuring whether
    /// it's moved past [`DETACH_THRESHOLD`] yet. Physical, like
    /// everything else this module hit-tests against — see the module
    /// doc.
    pub start: Point<f64, Physical>,
    /// Whether it's already flipped to floating and been mapped this
    /// drag — once true, further motion just repositions it directly,
    /// same as a real floating-window move.
    pub detached: bool,
}

/// One docked handle's clickable/drawable rectangle, plus which surface
/// and title it's for — shared between [`build`] (drawing) and
/// `input.rs` (hit-testing clicks), so the two can never disagree about
/// where a handle actually is. Physical, like `chrome::TabRect` — see
/// the module doc.
pub struct Handle {
    pub surface: WlSurface,
    pub rect: Rectangle<i32, Physical>,
    pub title: String,
}

/// Compute where each of the focused Hut's active Main Window's docked
/// Sub-Window handles currently are. Empty if the terminal is showing or
/// there's no active Main Window — handles only make sense alongside the
/// Main Window they belong to.
pub fn handle_layout(hut: &Hut, output_size: (i32, i32)) -> Vec<Handle> {
    let Some(entry) = hut.active_main_window_entry() else {
        return Vec::new();
    };
    let (output_w, output_h) = output_size;

    let mut handles = Vec::new();
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let docked_on_edge = entry
            .sub_windows
            .iter()
            .filter(|sub| matches!(sub.dock, Dock::Docked(e) if e == edge));
        for (n, sub) in docked_on_edge.enumerate() {
            let step = n as i32;
            let (x, y) = match edge {
                Edge::Left => (0, EDGE_MARGIN + step * (HANDLE_H + HANDLE_GAP)),
                Edge::Right => (output_w - HANDLE_W, EDGE_MARGIN + step * (HANDLE_H + HANDLE_GAP)),
                Edge::Top => (EDGE_MARGIN + step * (HANDLE_W + HANDLE_GAP), 0),
                Edge::Bottom => (EDGE_MARGIN + step * (HANDLE_W + HANDLE_GAP), output_h - HANDLE_H),
            };
            let Some(toplevel) = sub.window.toplevel() else {
                continue;
            };
            handles.push(Handle {
                surface: toplevel.wl_surface().clone(),
                rect: Rectangle::new(Point::from((x, y)), Size::from((HANDLE_W, HANDLE_H))),
                title: window_title(&sub.window),
            });
        }
    }
    handles
}

fn truncate(title: &str) -> String {
    if title.chars().count() > MAX_TITLE_CHARS {
        let truncated: String = title.chars().take(MAX_TITLE_CHARS.saturating_sub(1)).collect();
        format!("{truncated}\u{2026}")
    } else {
        title.to_string()
    }
}

/// Build the docked-handle chrome's render elements, or an empty list if
/// there's nothing docked right now.
pub fn build(hut: &mut Hut, renderer: &mut GlesRenderer, output_size: (i32, i32), scale: f64) -> Vec<Element> {
    let handles = handle_layout(hut, output_size);
    let mut elements = Vec::new();

    for handle in &handles {
        let title = truncate(&handle.title);

        // Stable id + real damage tracking, not a fresh `Id::new()`
        // wrapped in a never-tracked `from_static_texture` — the
        // handle's title text can change (the Sub-Window's own title
        // updates). Also only actually re-renders (real GPU work) when
        // the title changed since last frame — see
        // `render::LabelCache`'s doc comment.
        let stale = hut
            .sub_window_mut(&handle.surface)
            .map(|sub| sub.handle_text_cache.is_stale(&title))
            .unwrap_or(true);

        let rendered: Option<(Id, GlesTexture, DamageSnapshot<i32, Buffer>)> = if stale {
            match hut.render_label(renderer, &title, FG, BG) {
                Ok(texture) => hut.sub_window_mut(&handle.surface).map(|sub| {
                    let (texture, snapshot) = sub.handle_text_cache.store(title.clone(), texture);
                    (sub.handle_text_id.clone(), texture, snapshot)
                }),
                Err(err) => {
                    tracing::warn!("failed to render dock handle label: {err}");
                    None
                }
            }
        } else {
            hut.sub_window_mut(&handle.surface).and_then(|sub| {
                sub.handle_text_cache
                    .cached()
                    .map(|(texture, snapshot)| (sub.handle_text_id.clone(), texture, snapshot))
            })
        };

        if let Some((text_id, texture, snapshot)) = rendered {
            let texture_size = texture.size();
            let logical_size = crate::render::physical_element_size(texture_size.w, texture_size.h, scale);
            let text = TextureRenderElement::from_texture_with_damage(
                text_id,
                renderer.context_id(),
                (
                    (handle.rect.loc.x + 6) as f64,
                    (handle.rect.loc.y + 6) as f64,
                ),
                texture,
                1,
                Transform::Normal,
                None,
                None,
                Some(logical_size),
                None,
                snapshot,
                Kind::Unspecified,
            );
            elements.push(Element::from(text));
        }

        // The handle's background color never changes (no active/
        // inactive state, unlike a tab) — genuinely static content, so a
        // fixed commit is correct here; it just needs a stable id (a
        // fresh one every frame would otherwise look "new" to the outer
        // tracker every time, forcing needless redraws).
        let bg_id = hut
            .sub_window_mut(&handle.surface)
            .map(|sub| sub.handle_bg_id.clone())
            .unwrap_or_else(Id::new);
        let background = SolidColorRenderElement::new(
            bg_id,
            handle.rect,
            CommitCounter::default(),
            to_color32f(BG),
            Kind::Unspecified,
        );
        elements.push(Element::from(background));
    }

    elements
}

/// Start dragging `handle`'s Sub-Window out from its dock, if the pointer
/// just went down on it. Called from `input.rs`'s `PointerButton` press
/// handling, before it falls through to normal click-to-focus. `pos` is
/// physical, like [`Handle::rect`] — `input.rs` converts the seat's
/// (genuinely Logical) pointer position before calling this.
pub fn start_drag(state: &mut State, pos: Point<f64, Physical>) -> bool {
    let handles = handle_layout(state.stack.focused(), state.output_size);
    let Some(handle) = handles.into_iter().find(|h| h.rect.to_f64().contains(pos)) else {
        return false;
    };
    state.dock_drag = Some(DockDrag {
        surface: handle.surface,
        start: pos,
        detached: false,
    });
    true
}

/// Advance an in-progress handle drag on pointer motion: flips the
/// Sub-Window to floating and maps it for the first time once the drag
/// crosses [`DETACH_THRESHOLD`], then just repositions it directly on
/// every motion after that. `pos` is physical (see [`start_drag`]) —
/// converted to genuinely Logical right before it's written anywhere
/// `self.space` will read it back from ([`Dock::Floating`],
/// `map_element`), since those are Smithay's own contract, not this
/// module's.
pub fn advance_drag(state: &mut State, pos: Point<f64, Physical>) {
    let Some(drag) = &state.dock_drag else {
        return;
    };
    let surface = drag.surface.clone();
    let start = drag.start;
    let detached = drag.detached;

    if !detached {
        let delta = pos - start;
        if delta.x.hypot(delta.y) <= DETACH_THRESHOLD {
            return;
        }
        let logical = pos.to_logical(Scale::from(state.output_scale())).to_i32_round();
        if let Some(sub) = state.stack.focused_mut().sub_window_mut(&surface) {
            sub.dock = Dock::Floating(logical);
        }
        if let Some(drag) = &mut state.dock_drag {
            drag.detached = true;
        }
        state.sync_visible_main_window();
        return;
    }

    if let Some(window) = state.find_window_by_surface(&surface) {
        let logical = pos.to_logical(Scale::from(state.output_scale())).to_i32_round();
        state.space.map_element(window, logical, true);
    }
}

/// Finish an in-progress handle drag on pointer release: persist the
/// drop location (re-docking if it landed near an edge, same threshold
/// `grabs.rs`'s floating-window move uses), or leave `dock_drag` cleared
/// with no other effect if the drag never actually detached (a plain
/// click on a handle does nothing — there's no defined behavior for it
/// yet).
pub fn finish_drag(state: &mut State) {
    let Some(drag) = state.dock_drag.take() else {
        return;
    };
    if !drag.detached {
        return;
    }

    let Some(window) = state.find_window_by_surface(&drag.surface) else {
        return;
    };
    let Some(location) = state.space.element_location(&window) else {
        return;
    };
    let size = window.geometry().size;
    // `location`/`size` come from `self.space`, so they're genuinely
    // Logical — compared against the output's Logical size here, not
    // `state.output_size` (physical), to keep both sides of every
    // distance check in the same space (see `State::output_size_logical`'s
    // doc comment).
    let redock_edge = nearest_edge_within_threshold(state.output_size_logical(), location, size);
    if let Some(sub) = state.stack.focused_mut().sub_window_mut(&drag.surface) {
        sub.dock = match redock_edge {
            Some(edge) => Dock::Docked(edge),
            None => Dock::Floating(location),
        };
    }
    state.sync_visible_main_window();
    state.request_redraw();
}
