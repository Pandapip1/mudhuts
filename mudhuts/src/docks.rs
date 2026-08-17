//! Phase 5's docked-handle chrome: a small labeled tab near whichever
//! edge each of the focused ConsoleHut's active Main Window's docked
//! Floating Windows is minimized to, plus the compositor-native drag that
//! turns one into a floating window for the first time.
//!
//! A docked Floating Window isn't mapped as a real surface at all — nothing to
//! composite, and nothing for a client's own CSD to be dragged from — so
//! there's no `xdg_toplevel.move` grab to hook into for *this* side of
//! the interaction (see `grabs.rs` for the other side: once a Floating Window
//! is already floating, further drags go through a real `PointerGrab`).
//! Instead, the drag from a handle is tracked directly in `State`, the
//! same way `text_selecting` tracks a plain terminal-selection drag —
//! not a full `PointerGrab`, since there's no client surface/serial to
//! grab in the first place until the window is actually mapped.
//!
//! Handle layout/hit-testing (this module) is genuinely [`Physical`] —
//! the handle is mudhuts' own drawn chrome, pixel-native like
//! `chrome.rs`'s tab strip, not a real surface Smithay's `Space` knows
//! about. Only once a drag actually detaches and the Floating Window becomes
//! a real mapped element (`Dock::Floating`, read/written by
//! `sync_visible_main_window`/`grabs.rs`'s `MoveSurfaceGrab` too) does
//! its position need to cross over into genuinely `Logical` space to
//! match `ConsoleHut::space`'s own contract — see
//! [`advance_drag`]/[`finish_drag`] for exactly where that conversion
//! happens.
//!
//! First real adopter of `crate::redraw`'s `Redrawable`/`HitTestable`
//! traits (composable Hut hierarchy RFC, migration step 1) — chosen as
//! the proof point since its render/hit-test rect math ([`handle_layout`])
//! was already shared between drawing and clicking before either trait
//! existed. [`DockDrag`] holds a `RedrawHandle` instead of `finish_drag`
//! calling `State::request_redraw()` by hand; [`DockHandles`] wraps
//! `handle_layout` behind `HitTestable` for [`start_drag`]'s own hit-test.

use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{CommitCounter, DamageSnapshot};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::State;
use crate::chrome::{to_color32f, window_title};
use crate::console_hut::ConsoleHut;
use crate::grabs::nearest_edge_within_threshold;
use crate::main_window::{Dock, Edge};
use crate::redraw::{Hit, HitTestable, Redrawable, RedrawHandle};
use crate::render::OutputRenderElements;
use crate::space_element::HutSpaceRenderElement;

/// Base sizes (scale 1.0) — scaled via `crate::render::scaled` (or, for
/// the `f64` threshold, a plain multiply) wherever they're actually used,
/// so this chrome stays the same apparent size regardless of the output's
/// real DPI scale.
const HANDLE_W: i32 = 140;
const HANDLE_H: i32 = 28;
const HANDLE_GAP: i32 = 4;
/// Keep clear of `chrome.rs`'s tab strip and the screen corners.
const EDGE_MARGIN: i32 = 40;
const MAX_TITLE_CHARS: usize = 18;

/// How far (in physical pixels — this module's native space, see the
/// module doc) a drag has to travel from a handle before it detaches
/// into a floating window, rather than being read as just a click.
const DETACH_THRESHOLD: f64 = 12.0;

type Element = OutputRenderElements<GlesRenderer, HutSpaceRenderElement>;

/// Tracks a click-and-drag on a docked Floating Window's handle. Lives in
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
    /// Set via [`Redrawable::attach_redraw_handle`] right after this
    /// `DockDrag` is constructed in [`start_drag`] — `None` only for the
    /// instant between the two, never observed by anything else.
    redraw: Option<RedrawHandle>,
    /// The `ConsoleHut` that actually owns the dragged handle — captured
    /// once at drag-start time, not re-resolved via
    /// `state.stack.focused_mut()` on every callback. Mirrors
    /// `grabs.rs`'s `MoveSurfaceGrab::hut_id`: real multi-monitor's
    /// focus-follows-mouse can move input focus to a different output
    /// mid-drag, and writing the drag's position/scale lookups against
    /// *that* output instead of the drag's real owner would silently
    /// migrate it into an unrelated Hut/output.
    hut_id: u64,
    /// The real `Output` this drag's Hut lives on, captured at drag-start
    /// — resolved back to a live index fresh on every use
    /// (`GraphStack::output_index_for`, an O(outputs) scan — cheap at
    /// realistic monitor counts) rather than caching an index at all.
    /// An earlier round cached `output_index: usize` alongside this same
    /// `Output` as a fast-path hint, checking the two against each other
    /// before trusting the index — but a cached index buys nothing
    /// `output_index_for` doesn't already give for free every time, and
    /// the two-source-of-truth setup was itself a bug: nothing enforced
    /// them staying in sync, and forgetting a bounds-vs-identity check on
    /// one path silently rebased position/scale math against a real but
    /// wrong output. Storing only the stable `Output` handle makes that
    /// class of bug structurally impossible — there's no second, staler
    /// value to disagree with.
    ///
    /// `output_index_for` failing does *not* always mean `hut_id`'s Hut
    /// is gone, though — a real, previously-believed-sound argument here
    /// ("`remove_output` always destroys a removed output's own Huts
    /// together with it") turned out to miss one case: `remove_output`
    /// refuses to ever remove the very last remaining slot, so on a
    /// single-output machine, unplugging and replugging the one
    /// connector mid-drag leaves the Hut (and this drag) alive while
    /// `GraphStack::set_output` swaps in a brand new `Output` Arc for
    /// that slot underneath it, permanently breaking identity-based
    /// lookups for the handle captured here. `advance_drag` falls back
    /// to `output_index_for_hut(hut_id)` on a miss (mirroring
    /// `finish_drag`'s own resolution, which was never affected by this
    /// bug since it always resolved by hut id) and self-heals this field
    /// back to the freshly-resolved `Output` once that happens, so the
    /// fallback is only ever paid once per reconnect, not for the rest
    /// of the drag.
    output: Output,
}

impl Redrawable for DockDrag {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.redraw = Some(handle);
    }
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

/// Where the `step`-th (0-based) handle stacked along `edge` sits, in
/// physical pixels — `output_size`/`handle_size` are `(w, h)`, `gap` is
/// the spacing between consecutive stacked handles, `margin` keeps clear
/// of screen corners/`chrome.rs`'s tab strip (see [`EDGE_MARGIN`]'s own
/// doc comment). Right/Bottom subtract from the output dimension (flush
/// against the *far* edge) while Left/Top don't (flush against `0`) —
/// pulled out as a pure function over primitives specifically so that
/// asymmetry, and the `step * (size + gap)` stacking offset, are directly
/// testable: a flipped sign or swapped axis here would silently misplace
/// every docked handle on screen, and nothing short of eyes-on visual
/// inspection could catch it before this existed.
fn handle_position(edge: Edge, step: i32, output_size: (i32, i32), handle_size: (i32, i32), gap: i32, margin: i32) -> (i32, i32) {
    let (output_w, output_h) = output_size;
    let (handle_w, handle_h) = handle_size;
    match edge {
        Edge::Left => (0, margin + step * (handle_h + gap)),
        Edge::Right => (output_w - handle_w, margin + step * (handle_h + gap)),
        Edge::Top => (margin + step * (handle_w + gap), 0),
        Edge::Bottom => (margin + step * (handle_w + gap), output_h - handle_h),
    }
}

/// Compute where each of the focused ConsoleHut's active Main Window's docked
/// Floating Window handles currently are. Empty if the terminal is showing or
/// there's no active Main Window — handles only make sense alongside the
/// Main Window they belong to.
pub fn handle_layout(hut: &ConsoleHut, output_size: (i32, i32), scale: f64) -> Vec<Handle> {
    let Some(entry) = hut.active_main_window_entry() else {
        return Vec::new();
    };
    let handle_w = crate::render::scaled(HANDLE_W, scale);
    let handle_h = crate::render::scaled(HANDLE_H, scale);
    let handle_gap = crate::render::scaled(HANDLE_GAP, scale);
    let edge_margin = crate::render::scaled(EDGE_MARGIN, scale);

    let mut handles = Vec::new();
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let docked_on_edge = entry
            .floating_windows
            .iter()
            .filter(|sub| matches!(sub.dock, Dock::Docked(e) if e == edge));
        for (n, sub) in docked_on_edge.enumerate() {
            let (x, y) =
                handle_position(edge, n as i32, output_size, (handle_w, handle_h), handle_gap, edge_margin);
            let Some(toplevel) = sub.window.toplevel() else {
                continue;
            };
            handles.push(Handle {
                surface: toplevel.wl_surface().clone(),
                rect: Rectangle::new(Point::from((x, y)), Size::from((handle_w, handle_h))),
                title: window_title(&sub.window),
            });
        }
    }
    handles
}

/// Borrowed view over the focused ConsoleHut's currently-docked handles,
/// for hit-testing a click against them — see [`start_drag`]. Exists so
/// that hit-test (this impl) and rendering ([`build`]) both go through
/// [`handle_layout`], the same way `chrome::tab_layout`/`build` already
/// share layout — never two independently-derived rect computations.
struct DockHandles<'a> {
    hut: &'a ConsoleHut,
    output_size: (i32, i32),
    scale: f64,
}

impl HitTestable for DockHandles<'_> {
    fn hit_test(&self, point: Point<i32, Physical>) -> Option<Hit> {
        handle_layout(self.hut, self.output_size, self.scale)
            .into_iter()
            .find(|h| h.rect.contains(point))
            .map(|h| Hit::DockHandle(h.surface))
    }
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
pub fn build(
    hut: &mut ConsoleHut,
    renderer: &mut GlesRenderer,
    output_size: (i32, i32),
    scale: f64,
    theme: &crate::theme::Theme,
) -> Vec<Element> {
    let handles = handle_layout(hut, output_size, scale);
    let text_inset = crate::render::scaled(6, scale);
    let mut elements = Vec::new();

    for handle in &handles {
        let title = truncate(&handle.title);

        // Stable id + real damage tracking, not a fresh `Id::new()`
        // wrapped in a never-tracked `from_static_texture` — the
        // handle's title text can change (the Floating Window's own title
        // updates). Also only actually re-renders (real GPU work) when
        // the title changed since last frame — see
        // `render::LabelCache`'s doc comment.
        let stale = hut
            .floating_window_mut(&handle.surface)
            .map(|sub| sub.handle_text_cache.is_stale(&title))
            .unwrap_or(true);

        let rendered: Option<(Id, GlesTexture, DamageSnapshot<i32, Buffer>)> = if stale {
            match hut.render_label(renderer, &title, theme.dock_fg, theme.dock_bg) {
                Ok(texture) => hut.floating_window_mut(&handle.surface).map(|sub| {
                    let (texture, snapshot) = sub.handle_text_cache.store(title.clone(), texture);
                    (sub.handle_text_id.clone(), texture, snapshot)
                }),
                Err(err) => {
                    tracing::warn!("failed to render dock handle label: {err}");
                    None
                }
            }
        } else {
            hut.floating_window_mut(&handle.surface).and_then(|sub| {
                sub.handle_text_cache
                    .cached()
                    .map(|(texture, snapshot)| (sub.handle_text_id.clone(), texture, snapshot))
            })
        };

        if let Some((text_id, texture, snapshot)) = rendered {
            let text = TextureRenderElement::from_texture_with_damage(
                text_id,
                renderer.context_id(),
                (
                    (handle.rect.loc.x + text_inset) as f64,
                    (handle.rect.loc.y + text_inset) as f64,
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

        // The handle's background color never changes (no active/
        // inactive state, unlike a tab) — genuinely static content, so a
        // fixed commit is correct here; it just needs a stable id (a
        // fresh one every frame would otherwise look "new" to the outer
        // tracker every time, forcing needless redraws).
        let bg_id = hut
            .floating_window_mut(&handle.surface)
            .map(|sub| sub.handle_bg_id.clone())
            .unwrap_or_else(Id::new);
        let background = SolidColorRenderElement::new(
            bg_id,
            handle.rect,
            CommitCounter::default(),
            to_color32f(theme.dock_bg),
            Kind::Unspecified,
        );
        elements.push(Element::from(background));
    }

    elements
}

/// Start dragging `handle`'s Floating Window out from its dock, if the pointer
/// just went down on it. Called from `input.rs`'s `PointerButton` press
/// handling, before it falls through to normal click-to-focus. `pos` is
/// physical, like [`Handle::rect`] — `input.rs` converts the seat's
/// (genuinely Logical) pointer position before calling this.
pub fn start_drag(state: &mut State, pos: Point<f64, Physical>) -> bool {
    let handles = DockHandles {
        hut: state.stack.focused(),
        output_size: state.output_size,
        scale: state.focused_output_scale(),
    };
    let point = Point::from((pos.x.round() as i32, pos.y.round() as i32));
    let Some(Hit::DockHandle(surface)) = handles.hit_test(point) else {
        return false;
    };
    // Checked, not `state.stack.outputs()[state.stack.focused_output_index()]`
    // — always in-bounds today per `focused_output_index()`'s own
    // invariants, but an unchecked index is a new panic surface the
    // project's no-panics rule doesn't allow, and every sibling accessor
    // already degrades gracefully instead.
    let Some(output) =
        state.stack.outputs().get(state.stack.focused_output_index()).map(|slot| slot.output.clone())
    else {
        return false;
    };
    let mut drag = DockDrag {
        surface,
        start: pos,
        detached: false,
        redraw: None,
        hut_id: state.stack.focused().id,
        output,
    };
    drag.attach_redraw_handle(state.redraw_handle());
    state.dragging_hut_id = Some(drag.hut_id);
    state.dock_drag = Some(drag);
    true
}

/// `global_pos` (genuinely global Logical — see [`advance_drag`]'s own
/// doc comment) rebased to Physical, local to `output_position` — the
/// exact Logical-vs-Physical-at-fractional-scale conversion class that's
/// caused real, shipped bugs elsewhere in this codebase (the auto-hide
/// tab strip and Main Window positioning work both hit variants of this).
/// Pulled out as a pure function over `Point`/`Scale` so this specific
/// conversion — subtract, *then* convert to physical, not the other order
/// — is directly testable at fractional scales without a live drag/State.
fn rebase_to_output_physical(
    global_pos: Point<f64, Logical>,
    output_position: Point<i32, Logical>,
    scale: f64,
) -> Point<f64, Physical> {
    (global_pos - output_position.to_f64()).to_physical(Scale::from(scale))
}

/// Whether a drag has moved far enough from its start (`delta = pos -
/// start`, both physical pixels) to detach into a floating window rather
/// than being read as just a click — `scale` keeps [`DETACH_THRESHOLD`]'s
/// apparent size the same regardless of the output's real DPI, matching
/// every other hand-tuned chrome constant in this crate. Pulled out as a
/// pure function over primitives so this comparison is directly testable
/// without a live drag/State — an inclusive vs. exclusive threshold
/// mistake, or a missed `* scale`, would silently change how far a drag
/// has to travel before it detaches, differently per monitor.
fn exceeds_detach_threshold(delta: Point<f64, Physical>, scale: f64) -> bool {
    delta.x.hypot(delta.y) > DETACH_THRESHOLD * scale
}

/// Advance an in-progress handle drag on pointer motion: flips the
/// Floating Window to floating and maps it for the first time once the drag
/// crosses [`DETACH_THRESHOLD`], then just repositions it directly on
/// every motion after that. `global_pos` is genuinely global Logical
/// (`State::pointer_location`'s own doc comment) — rebased below to
/// Physical, local to the *drag's own* output (`drag.output`), not
/// whichever output currently has focus: this runs on every
/// pointer-motion sample for the whole drag, and a mid-drag
/// focus-follows-mouse switch (the pointer crossing onto another
/// monitor while the handle is still held) would otherwise corrupt
/// [`DockDrag::start`]-relative deltas by roughly the distance between
/// the two outputs the instant that happened — same bug class,
/// `grabs.rs`'s `MoveSurfaceGrab::start_global_location` fixes it via an
/// explicit global reference; pinning `pos` itself to the drag's own
/// output achieves the same thing here without needing one, since
/// [`DockDrag::start`] was already captured local to that same output.
pub fn advance_drag(state: &mut State, global_pos: Point<f64, Logical>) {
    let Some(drag) = &state.dock_drag else {
        return;
    };
    let surface = drag.surface.clone();
    let start = drag.start;
    let detached = drag.detached;
    let hut_id = drag.hut_id;
    // Resolved fresh, not cached — see [`DockDrag::output`]'s own doc
    // comment on why. Falls back to a hut-id-based search
    // (`output_index_for_hut`, mirroring `finish_drag`'s own resolution
    // and `find_mut_for_hint`'s fast-path/fallback shape) if the cached
    // `Output` handle has gone stale: `GraphStack::remove_output` never
    // removes the very last remaining slot, so on a single-output
    // machine, unplugging and replugging the one connector mid-drag
    // leaves this drag's own Hut alive while `set_output` swaps in a
    // brand new `Output` Arc for that slot underneath it — permanently
    // breaking `Output`-identity lookups for a handle captured before
    // the swap, even though the Hut (and the drag) are both still very
    // much alive. Only actually means the drag's Hut is gone if BOTH
    // resolutions fail.
    let output_index =
        state.stack.output_index_for(&drag.output).or_else(|| state.stack.output_index_for_hut(hut_id));
    let Some(output_index) = output_index else {
        return;
    };
    // Self-heal the cached `Output` handle once it's known stale, so
    // later calls this same drag hit the cheap identity fast path again
    // instead of paying for the hut-id fallback on every remaining
    // motion sample.
    if let Some(resolved) = state.stack.outputs().get(output_index).map(|slot| slot.output.clone())
        && let Some(drag) = &mut state.dock_drag
        && drag.output != resolved
    {
        drag.output = resolved;
    }
    let scale = state.output_scale_for(output_index);
    let output_position = state.stack.output_position(output_index);
    let pos = rebase_to_output_physical(global_pos, output_position, scale);

    if !detached {
        let delta = pos - start;
        if !exceeds_detach_threshold(delta, scale) {
            return;
        }
        let logical = pos.to_logical(Scale::from(scale)).to_i32_round();
        // This Hut's own output's usable area — needed for
        // `sync_main_window_space` below; computed before the mutable
        // `hut` borrow for the same borrow-checker reason as `unset`'s
        // own equivalent line in grabs.rs. `_logical_for`, not the
        // physical-pixel `usable_area_for` — see
        // `State::sync_visible_main_window`'s own doc comment: `space`
        // is a real `Space<HutSpaceElement>`, which requires a genuinely
        // Logical origin.
        let area_origin = state.usable_area_logical_for(output_index).loc;
        // Fast path first (`output_index` was just freshly resolved
        // above, so this is never stale) — falls back to the full
        // graph-wide search only on a miss.
        let Some(hut) = state.stack.find_mut_for_hint(output_index, hut_id) else {
            // The owning Hut exited mid-drag — nothing left to update.
            return;
        };
        if let Some(sub) = hut.floating_window_mut(&surface) {
            sub.dock = Dock::Floating(logical);
        }
        // This Hut's own space, not `state.sync_visible_main_window()`
        // (focused-output-only — see grabs.rs's `unset` for the same
        // fix and its fuller reasoning): the drag's owning Hut may not
        // be the focused one by now.
        hut.sync_main_window_space(area_origin);
        if let Some(drag) = &mut state.dock_drag {
            drag.detached = true;
        }
        return;
    }

    // The owning Hut's own window list, not `state.find_window_by_surface`
    // (a full graph-wide scan) — this runs on every pointer-motion sample
    // for the rest of the drag, same hot-loop reasoning as the
    // `find_mut_for` fast path above.
    let Some(hut) = state.stack.find_mut_for_hint(output_index, hut_id) else {
        return;
    };
    if let Some(window) = hut.floating_window_mut(&surface).map(|sub| sub.window.clone()) {
        let logical = pos.to_logical(Scale::from(scale)).to_i32_round();
        hut.space_raw_mut()
            .map_element(crate::space_element::HutSpaceElement::Window(window), logical, true);
    }
}

/// Finish an in-progress handle drag on pointer release: persist the
/// drop location (re-docking if it landed near an edge, same threshold
/// `grabs.rs`'s floating-window move uses), or leave `dock_drag` cleared
/// with no other effect if the drag never actually detached (a plain
/// click on a handle does nothing — there's no defined behavior for it
/// yet).
///
/// KNOWN DUPLICATION with `grabs.rs`'s `MoveSurfaceGrab::unset` — see its
/// own doc comment, not repeated here.
pub fn finish_drag(state: &mut State) {
    let Some(drag) = state.dock_drag.take() else {
        return;
    };
    state.dragging_hut_id = None;
    if !drag.detached {
        return;
    }

    let Some(window) = state.find_window_by_surface(&drag.surface) else {
        return;
    };
    // The drag's own owning Hut, not `state.stack.focused()` — see
    // [`DockDrag::hut_id`]'s doc comment: the pointer may have crossed
    // onto a different output mid-drag.
    let Some(hut) = state.stack.find_mut(drag.hut_id) else {
        // The owning Hut exited mid-drag — nothing left to persist.
        return;
    };
    let Some(location) =
        hut.space()
            .element_location(&crate::space_element::HutSpaceElement::Window(window.clone()))
    else {
        return;
    };
    let size = window.geometry().size;
    // `location`/`size` come from the drag's own owning Hut's `space`,
    // so they're genuinely Logical — compared against *that Hut's own
    // output's* Logical size, not `state.focused_output_size_logical()` (the
    // focused output, possibly a different one by now), to keep the
    // distance check meaningful for the output the window is actually
    // being dropped on.
    let Some(output_index) = state.stack.output_index_for_hut(drag.hut_id) else {
        return;
    };
    let redock_edge = nearest_edge_within_threshold(state.output_size_logical_for(output_index), location, size);
    // `_logical_for`, not the physical-pixel `usable_area_for` — see
    // `State::sync_visible_main_window`'s own doc comment: `space`'s
    // positions (like `location` above) are genuinely Logical.
    let area_origin = state.usable_area_logical_for(output_index).loc;
    let Some(hut) = state.stack.find_mut(drag.hut_id) else {
        return;
    };
    if let Some(sub) = hut.floating_window_mut(&drag.surface) {
        sub.dock = match redock_edge {
            Some(edge) => Dock::Docked(edge),
            None => Dock::Floating(location),
        };
    }
    // This Hut's own space, not `state.sync_visible_main_window()` — see
    // `advance_drag`/`grabs.rs`'s `unset` for the same fix.
    hut.sync_main_window_space(area_origin);
    // Via the handle attached in `start_drag`, not `state.request_redraw()`
    // directly — see `crate::redraw`'s module doc.
    if let Some(redraw) = &drag.redraw {
        redraw.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_and_top_sit_flush_against_the_zero_origin() {
        assert_eq!(handle_position(Edge::Left, 0, (1000, 800), (140, 28), 4, 40), (0, 40));
        assert_eq!(handle_position(Edge::Top, 0, (1000, 800), (140, 28), 4, 40), (40, 0));
    }

    #[test]
    fn right_and_bottom_sit_flush_against_the_far_edge() {
        assert_eq!(handle_position(Edge::Right, 0, (1000, 800), (140, 28), 4, 40), (1000 - 140, 40));
        assert_eq!(handle_position(Edge::Bottom, 0, (1000, 800), (140, 28), 4, 40), (40, 800 - 28));
    }

    #[test]
    fn stacking_offset_accumulates_by_size_plus_gap() {
        // Third handle (step 2) stacked down the Left edge.
        assert_eq!(handle_position(Edge::Left, 2, (1000, 800), (140, 28), 4, 40), (0, 40 + 2 * (28 + 4)));
        // Third handle (step 2) stacked along the Top edge — stacks by
        // width, not height, since Top/Bottom stack horizontally.
        assert_eq!(handle_position(Edge::Top, 2, (1000, 800), (140, 28), 4, 40), (40 + 2 * (140 + 4), 0));
    }

    #[test]
    fn left_and_top_ignore_the_far_output_dimension() {
        // Regression guard against a copy-paste swap of which dimension
        // gets subtracted: Left's x/Top's y must stay 0 regardless of
        // the output's own size.
        assert_eq!(handle_position(Edge::Left, 0, (1000, 800), (140, 28), 4, 40).0, 0);
        assert_eq!(handle_position(Edge::Left, 0, (5000, 8000), (140, 28), 4, 40).0, 0);
        assert_eq!(handle_position(Edge::Top, 0, (1000, 800), (140, 28), 4, 40).1, 0);
        assert_eq!(handle_position(Edge::Top, 0, (5000, 8000), (140, 28), 4, 40).1, 0);
    }

    #[test]
    fn exactly_at_the_threshold_does_not_yet_exceed() {
        assert!(!exceeds_detach_threshold(Point::from((DETACH_THRESHOLD, 0.0)), 1.0));
    }

    #[test]
    fn one_unit_past_the_threshold_exceeds() {
        assert!(exceeds_detach_threshold(Point::from((DETACH_THRESHOLD + 1.0, 0.0)), 1.0));
    }

    #[test]
    fn scale_multiplies_the_effective_threshold() {
        // 20px is past the base threshold (12) but under the scale=2.0
        // effective one (24) — must not exceed.
        assert!(!exceeds_detach_threshold(Point::from((20.0, 0.0)), 2.0));
        // 13px is past the base threshold and scale=1.0 doesn't stretch
        // it any further — must exceed.
        assert!(exceeds_detach_threshold(Point::from((13.0, 0.0)), 1.0));
    }

    #[test]
    fn diagonal_delta_uses_hypot_not_either_axis_alone() {
        // Each axis alone (9) is under the threshold (12), but the real
        // Euclidean distance (~12.73) is past it.
        assert!(exceeds_detach_threshold(Point::from((9.0, 9.0)), 1.0));
    }

    #[test]
    fn rebase_subtracts_output_position_then_converts_to_physical() {
        let global = Point::<f64, Logical>::from((150.0, 250.0));
        let output_position = Point::<i32, Logical>::from((100, 200));
        let physical = rebase_to_output_physical(global, output_position, 2.0);
        // Local logical: (50, 50); at scale 2.0: (100, 100).
        assert_eq!(physical, Point::<f64, Physical>::from((100.0, 100.0)));
    }

    #[test]
    fn truncate_leaves_a_title_exactly_at_the_limit_unchanged() {
        let title = "x".repeat(MAX_TITLE_CHARS);
        assert_eq!(truncate(&title), title);
    }

    #[test]
    fn truncate_shortens_a_title_one_over_the_limit() {
        let title = "x".repeat(MAX_TITLE_CHARS + 1);
        let result = truncate(&title);
        assert_eq!(result.chars().count(), MAX_TITLE_CHARS);
        assert!(result.ends_with('\u{2026}'));
        assert_eq!(result.chars().filter(|&c| c == 'x').count(), MAX_TITLE_CHARS - 1);
    }

    #[test]
    fn truncate_leaves_an_empty_title_unchanged() {
        assert_eq!(truncate(""), "");
    }

    #[test]
    fn truncate_counts_multi_byte_characters_not_bytes() {
        // Each of these is a multi-byte UTF-8 scalar but a single
        // `char` — a byte-length-based truncation would cut mid-
        // character or truncate far earlier than intended.
        let title = "🦀".repeat(MAX_TITLE_CHARS + 1);
        let result = truncate(&title);
        assert_eq!(result.chars().count(), MAX_TITLE_CHARS);
        assert!(result.ends_with('\u{2026}'));
    }
}
