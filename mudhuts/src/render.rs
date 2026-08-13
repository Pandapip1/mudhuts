use smithay::backend::renderer::{Renderer, Texture};
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree};
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{CommitCounter, DamageBag, DamageSnapshot};
use smithay::backend::renderer::{ImportAll, ImportMem, RendererSuper};
use smithay::desktop::space::{SpaceRenderElements, space_render_elements};
use smithay::desktop::{Window, layer_map_for_output};
use smithay::output::Output;
use smithay::utils::{Buffer, Rectangle, Scale, Transform};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use crate::State;
use crate::hut::{Hut, pane_rects};
use crate::{chrome, docks, switcher, village_chrome};

/// The [`TextureRenderElement`] buffer-scale ([`Element::src`]/
/// [`Element::geometry`]'s shared `self.scale: i32`) that makes a texture
/// rendered pixel-native at real *physical* resolution (mudhuts' own
/// chrome/terminal — see this module's other doc comments on that
/// design) composite at its true on-screen size once the output's scale
/// is non-1.0.
///
/// **This has to be the buffer-scale argument, not an explicit `size`
/// override** — a first attempt at this fix passed `size:
/// Some(logical_size)` instead and looked identical to the original bug
/// (fuzzy, only the top-left corner visible) despite fixing
/// `geometry()`'s *destination* size correctly, because
/// `TextureRenderElement::src()` *also* derives the sampled *source*
/// region from `logical_size()` whenever `src` is left `None` — see
/// `.../src/backend/renderer/element/texture.rs`'s `Element::src` impl:
/// `self.src().to_buffer(self.scale as f64, transform, &logical_size)`.
/// Shrinking `logical_size` via an explicit `size` while leaving
/// `self.scale` (buffer-scale) at its default of 1 shrinks the *sampled*
/// region to match — only the top-left corner of the real texture gets
/// sampled, then stretched back up to fill the (correctly-sized now)
/// destination rect. Passing this as the buffer-scale argument instead
/// fixes both halves of the same underlying contract at once: `src()`
/// un-scales by it (recovering the *full* texture as the sample region)
/// and `geometry()` re-scales by it (landing back on the texture's real
/// physical size) — the mechanism Smithay actually designed for exactly
/// this "buffer is N× the surface's logical size" case.
///
/// Rounds to the nearest whole number (`detect_output_scale` in
/// `udev_backend.rs` only ever produces one anyway) — `scale: i32` is a
/// hard Smithay API constraint here, so a genuinely fractional host scale
/// (winit) loses sub-pixel precision at this specific call site; a real
/// fractional-native fix would need an explicit `src` *and* `size`
/// together (mirroring how `switcher.rs`'s thumbnail already sets both),
/// not attempted here since it's out of scope for the bug this exists to
/// fix.
pub(crate) fn texture_buffer_scale(scale: f64) -> i32 {
    scale.round().max(1.0) as i32
}

/// Scale a hand-tuned chrome constant (padding, margin, handle size, ...)
/// so it stays the same *apparent* size regardless of the output's real
/// DPI scale, instead of the same fixed physical-pixel count everywhere —
/// used by `chrome.rs`/`village_chrome.rs`/`docks.rs`/`switcher.rs`/this
/// module's own tile-pane border, wherever a constant was previously a
/// bare physical-pixel literal.
pub(crate) fn scaled(px: i32, scale: f64) -> i32 {
    ((px as f64) * scale).round() as i32
}

/// Tracks whether some comparable value changed since the last check,
/// bumping a [`CommitCounter`] when it has. Backs real damage tracking for
/// render elements whose *content* (not geometry) can change while kept at
/// a stable `Id` — e.g. a tab's background flipping between active/
/// inactive colors, or its label's text/color changing, all at a fixed
/// on-screen position. Smithay's per-element damage tracker only learns
/// about a content-only change via an explicit commit bump; a
/// `CommitCounter` that never advances (e.g. `CommitCounter::default()`
/// passed fresh every frame) means "never damaged again after the first
/// frame" — the same class of bug as `TextureRenderElement::from_static_texture`
/// (see `ConsoleHut::damage_tracker`'s doc comment), just for
/// `SolidColorRenderElement`/hand-tracked content instead.
pub(crate) struct ChangeTracker<T> {
    last: Option<T>,
    commit: CommitCounter,
}

impl<T: PartialEq> ChangeTracker<T> {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            commit: CommitCounter::default(),
        }
    }

    /// Compare `value` against what was seen last time, bumping the
    /// commit counter if it differs, and return the (possibly
    /// just-bumped) counter to attach to this frame's render element.
    pub(crate) fn commit(&mut self, value: T) -> CommitCounter {
        if self.last.as_ref() != Some(&value) {
            self.commit.increment();
            self.last = Some(value);
        }
        self.commit
    }
}

/// Caches a rendered label texture (from `ConsoleHut::render_label`), only
/// actually re-rendering it when the value identifying its content
/// (title text, active/inactive state, ...) changes since last time.
/// `ConsoleHut::render_label` does real GPU work every call — glyph-atlas
/// lookups plus instanced draw calls into an FBO — and without this
/// cache, `chrome.rs`'s tab strip and `docks.rs`'s dock handles would
/// pay that cost on *every single frame* they're visible, even though a
/// label's text/color essentially never changes between frames. Also
/// backs real damage tracking for the resulting `TextureRenderElement`
/// (via `from_texture_with_damage`), for the same reason
/// [`ChangeTracker`] exists: a snapshot that never advances means "never
/// damaged again after the first frame".
pub(crate) struct LabelCache<T> {
    last: Option<T>,
    texture: Option<GlesTexture>,
    damage: DamageBag<i32, Buffer>,
}

impl<T: PartialEq> LabelCache<T> {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            texture: None,
            damage: DamageBag::default(),
        }
    }

    /// Whether `key` differs from what's cached (or nothing's been
    /// rendered yet) — split from [`Self::store`] (rather than a single
    /// render-and-cache call taking a closure) specifically so the
    /// caller can render a fresh texture via a `&mut self` method of its
    /// *own* (e.g. `ConsoleHut::render_label`) in between the two, without that
    /// call fighting a simultaneous mutable borrow of this cache.
    pub(crate) fn is_stale(&self, key: &T) -> bool {
        self.texture.is_none() || self.last.as_ref() != Some(key)
    }

    /// Store a freshly-rendered texture for `key` (call after
    /// [`Self::is_stale`] returned `true`), marking it fully damaged, and
    /// return it alongside the fresh snapshot.
    pub(crate) fn store(
        &mut self,
        key: T,
        texture: GlesTexture,
    ) -> (GlesTexture, DamageSnapshot<i32, Buffer>) {
        self.damage.add([Rectangle::from_size(texture.size())]);
        self.texture = Some(texture.clone());
        self.last = Some(key);
        (texture, self.damage.snapshot())
    }

    /// The already-cached texture and a damage snapshot for it, for when
    /// [`Self::is_stale`] returned `false`. `None` if nothing's been
    /// rendered yet — shouldn't happen if the caller checked
    /// `is_stale` first, but stays panic-free rather than assumed.
    pub(crate) fn cached(&self) -> Option<(GlesTexture, DamageSnapshot<i32, Buffer>)> {
        let texture = self.texture.clone()?;
        Some((texture, self.damage.snapshot()))
    }
}

// Generic over the renderer `R` (matching the same pattern Smithay's own
// `anvil` demo uses for its `OutputRenderElements`) so every variant is
// expressed in terms of the same `R` consistently — `R::TextureId` here
// resolves to `GlesTexture` once `R` is instantiated as `GlesRenderer` at
// the call site (`winit_backend.rs`), which is the only renderer this
// compositor ever actually uses (see the Phase 2.6 plan notes on why
// GLES rather than a fully renderer-agnostic abstraction).
//
// `Terminal` is also reused for the Alt-Tab popup's thumbnails
// (`switcher.rs`) — they're the same element type (a texture composited
// at an explicit size/location), just wrapping a different ConsoleHut's cached
// texture. `SolidColor` backs the popup's background panel and highlight
// border (`SolidColorRenderElement` isn't generic over `R` at all).
// `Pointer` backs the udev/DRM backend's own compositor-drawn cursor
// (see `cursor.rs`) — unused (never constructed) under the winit
// backend, which relies on the host compositor's own cursor instead.
smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem;
    Space = SpaceRenderElements<R, E>,
    Terminal = TextureRenderElement<<R as RendererSuper>::TextureId>,
    SolidColor = SolidColorRenderElement,
    Pointer = crate::cursor::PointerRenderElement<R>,
}

type Element = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// This output's layer-shell surfaces (`handlers/layer_shell.rs`), split
/// into the same two halves Smithay's own `space_render_elements` splits
/// them into internally: `upper` (Top + Overlay, meant to render *above*
/// normal content) and `lower` (Background + Bottom, *below* it).
/// `space_render_elements` already handles this automatically for the
/// Main-Window-visible branch below (its own doc comment confirms it:
/// "this will include layer-shell surfaces added to this output's
/// LayerMap") — this helper is only for the *other* two branches
/// (showing the terminal directly, or a Tile-Hut), which don't go
/// through a `Space` at all, so nothing else would ever composite layer
/// surfaces for them. Kept split (not flattened into one Vec) so the
/// caller can insert its own content — a terminal texture, or a whole
/// tile's worth of panes — into exactly the z-order slot a normal
/// toplevel would otherwise occupy, between the two.
fn layer_elements(
    state: &State,
    renderer: &mut GlesRenderer,
) -> (Vec<Element>, Vec<Element>) {
    let Some(output) = state.space.outputs().next() else {
        return (Vec::new(), Vec::new());
    };
    // Layer geometry (`layer_geometry`) is genuinely Logical (see
    // `handlers/layer_shell.rs`'s module doc) — has to be converted to
    // physical here, same as `space_render_elements` already does
    // internally for the Main-Window-visible branch this function stands
    // in for (see this function's own doc comment).
    let scale = state.output_scale();
    let map = layer_map_for_output(output);
    let (lower, upper): (Vec<_>, Vec<_>) = map
        .layers()
        .rev()
        .partition(|s| matches!(s.layer(), WlrLayer::Background | WlrLayer::Bottom));

    let mut render = |surfaces: Vec<&smithay::desktop::LayerSurface>| -> Vec<Element> {
        surfaces
            .into_iter()
            .filter_map(|s| map.layer_geometry(s).map(|geo| (geo.loc, s)))
            .flat_map(|(loc, s)| {
                let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = s.render_elements(
                    renderer,
                    loc.to_physical_precise_round(scale),
                    Scale::from(scale),
                    1.0,
                );
                elems
                    .into_iter()
                    .map(|e| Element::from(SpaceRenderElements::Surface(e)))
            })
            .collect()
    };

    (render(upper), render(lower))
}

/// Everything mudhuts draws while `state.locked` is set (see
/// `handlers/session_lock.rs`'s module doc) — the *only* thing drawn in
/// that state, replacing every other branch of [`build_frame_elements`]
/// rather than being layered on top of it, since none of that (Alt-Tab
/// popup, chrome, terminal/window content) should be visible, or even
/// get a fresh texture, while the session is locked.
///
/// The locking client's own [`LockSurface`](smithay::wayland::session_lock::LockSurface),
/// once mapped and alive, wins; otherwise (nothing mapped yet, or a
/// stale one that's gone) a plain opaque rectangle covering the whole
/// output, so the screen actually goes blank the instant `state.locked`
/// flips true rather than leaving the last real frame on screen until
/// some client gets around to mapping a lock surface — the protocol
/// requires exactly that blank-or-locked frame to have been presented
/// before `locked()` can even be sent to the client (see
/// `handlers/session_lock.rs`'s `lock` doc comment).
fn lock_screen_elements(
    state: &State,
    renderer: &mut GlesRenderer,
    size: (i32, i32),
) -> Vec<Element> {
    if let Some(surface) = &state.lock_surface
        && surface.alive()
    {
        let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = render_elements_from_surface_tree(
            renderer,
            surface.wl_surface(),
            smithay::utils::Point::<i32, smithay::utils::Physical>::from((0, 0)),
            Scale::from(state.output_scale()),
            1.0,
            Kind::Unspecified,
        );
        if !elems.is_empty() {
            return elems
                .into_iter()
                .map(|e| Element::from(SpaceRenderElements::Surface(e)))
                .collect();
        }
    }

    let background = SolidColorRenderElement::new(
        state.lock_backdrop_id.clone(),
        Rectangle::<i32, smithay::utils::Physical>::new(
            smithay::utils::Point::from((0, 0)),
            smithay::utils::Size::from(size),
        ),
        CommitCounter::default(),
        [0.0, 0.0, 0.0, 1.0],
        Kind::Unspecified,
    );
    vec![Element::from(background)]
}

/// Build one frame's worth of render elements (Alt-Tab popup, tab-strip
/// chrome, docked-handle chrome, then either the focused ConsoleHut's terminal
/// texture or its visible Main Window/Floating Windows/Alerts via `state.space`)
/// in front-to-back order — shared by every backend
/// (`winit_backend.rs`/`udev_backend.rs`) so the element-assembly logic
/// itself is written once. Backend-specific concerns (binding a
/// framebuffer, damage tracking vs. DRM's `render_frame`, submitting/
/// queuing the result) stay in each backend's own module.
pub fn build_frame_elements(
    state: &mut State,
    renderer: &mut GlesRenderer,
    output: &Output,
    size: (i32, i32),
) -> Vec<OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> {
    // Checked before every other branch below (switcher, Hut chrome,
    // terminal/window content): a locked session must render *nothing*
    // else, not even layered underneath a lock surface — see
    // `handlers/session_lock.rs`'s module doc on why the protocol
    // requires this before it can even tell the client the lock
    // succeeded.
    if state.locked {
        return lock_screen_elements(state, renderer, size);
    }

    let show_terminal = state.showing_terminal_effective();
    let scale = state.output_scale();
    let mut elements = Vec::new();

    // Only the focused ConsoleHut normally gets redrawn (see Phase 2.6's
    // damage-avoidance work) — but the Alt-Tab popup shows every ConsoleHut's
    // thumbnail, so while it's open they all need fresh cached textures.
    // Redundant with the focused ConsoleHut's own redraw further down (cheap: a
    // second `redraw` call in the same tick is a no-op cache hit, since
    // damage was already reset by the first).
    if state.stack.is_previewing() {
        for hut in state.stack.top_level_huts_mut() {
            hut.redraw(renderer);
        }
    }

    // Pushed first (frontmost — `render_output`/`render_frame` take
    // elements in front-to-back order) so the popup sits on top of
    // whatever's below, regardless of whether that's the terminal or a
    // client window; empty when no preview session is open.
    elements.extend(switcher::build(&state.stack, size, renderer, scale));

    // Tile-Hut (Phase 6) — bypasses the normal single-ConsoleHut chrome/
    // docks/terminal-or-space pipeline entirely: every pane is visible
    // simultaneously, side by side, each always showing its own ConsoleHut's
    // terminal (never a Main Window — see `village.rs`'s module doc on
    // why that's this pass's deliberate scope). A 1-child (or empty)
    // Tile never actually exists (`Hut::collapse_if_singleton`
    // unwraps it immediately), but the length check stays as a defensive
    // fallback to the normal pipeline rather than assumed.
    if matches!(state.stack.focused_top_level(), Hut::Tile(tile) if tile.children.len() >= 2) {
        let (layer_upper, layer_lower) = layer_elements(state, renderer);
        elements.extend(layer_upper);
        elements.extend(build_tile_elements(state, renderer));
        elements.extend(layer_lower);
        return elements;
    }

    // Hut-level tab strip(s) (Phase 6) — one per Tab-Hut along
    // the active path, stacked from the top of the screen, outermost
    // first (see `village_chrome.rs`'s module doc). Empty unless the
    // focused top-level Hut actually is a Tab-Hut with 2+
    // children; `next_y` is unchanged (0) in that case.
    let cell_w = state.stack.focused().glyphs.cell_width().max(1);
    let cell_h = state.stack.focused().glyphs.cell_height().max(1) as i32;
    let (village_tab_elements, next_y) =
        village_chrome::build(state.stack.focused_top_level_mut(), renderer, 0, cell_w, cell_h, scale);
    elements.extend(village_tab_elements);

    // Tab-strip chrome (Phase 4) — pushed below any Hut-level strips
    // above it, still on top of the terminal/window content and still
    // below the Alt-Tab popup above. Empty when the focused ConsoleHut has no
    // Main Windows.
    elements.extend(chrome::build(state.stack.focused_mut(), renderer, next_y, scale));

    // Docked Floating Window handles (Phase 5) — same z-order slot as the tab
    // strip, only shown alongside the Main Window they belong to (never
    // while the terminal itself is the visible view).
    if !show_terminal {
        elements.extend(docks::build(state.stack.focused_mut(), renderer, size, scale));
    }

    if show_terminal {
        // Layer-shell surfaces around the terminal texture — the
        // Main-Window-visible branch below gets this automatically from
        // `space_render_elements`; this branch doesn't go through a
        // `Space` at all, so it needs the hand-rolled equivalent (see
        // `layer_elements`'s doc comment).
        let (layer_upper, layer_lower) = layer_elements(state, renderer);
        elements.extend(layer_upper);
        let scale = state.output_scale();
        let hut = state.stack.focused_mut();
        if let Some(texture) = hut.redraw(renderer) {
            // `from_texture_with_damage`, not `from_static_texture` — the
            // terminal's content genuinely changes every keystroke, and
            // `from_static_texture` is documented by Smithay as creating
            // an element with no damage tracking at all (see
            // `ConsoleHut::damage_tracker`'s doc comment).
            //
            // Buffer scale, not an explicit `size` — see
            // `texture_buffer_scale`'s doc comment for why leaving this
            // `1` double-applies the output's scale once it's non-1.0,
            // and why an explicit `size` alone isn't the right fix either.
            let element = TextureRenderElement::from_texture_with_damage(
                hut.element_id.clone(),
                renderer.context_id(),
                (0.0, 0.0),
                texture,
                texture_buffer_scale(scale),
                Transform::Normal,
                None,
                None,
                None,
                None,
                hut.element_damage_snapshot(),
                Kind::Unspecified,
            );
            elements.push(OutputRenderElements::from(element));
        }
        elements.extend(layer_lower);
    } else {
        match space_render_elements::<_, Window, _>(renderer, [&state.space], output, 1.0) {
            Ok(space_elements) => {
                elements.extend(space_elements.into_iter().map(OutputRenderElements::from))
            }
            Err(err) => tracing::warn!("failed to collect space elements: {err}"),
        }
    }

    elements
}

/// Build a Tile-Hut's panes side by side: each child's own terminal
/// texture (already sized to its pane by `Hut::resize_to_pixels` —
/// see that method and [`pane_rects`]), composited at its pane's screen
/// position, plus a highlight border around whichever pane currently has
/// keyboard focus. Only called once the caller's confirmed the focused
/// top-level Hut really is a `Hut::Tile` with 2+ children.
///
/// Panes are normal content — like the terminal-visible branch above,
/// sized and positioned against [`State::usable_area`], not the raw
/// output rect (matches `Hut::resize_to_pixels`'s own sizing via
/// `Stack::resize_all`, and `State::active_pane_offset`'s identical
/// rect computation for mouse-interaction routing — both must agree with
/// what's actually drawn here).
fn build_tile_elements(
    state: &mut State,
    renderer: &mut GlesRenderer,
) -> Vec<OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> {
    let (area_x, area_y, area_w, area_h) = state.usable_area();
    let scale = state.output_scale();
    let Hut::Tile(tile) = state.stack.focused_top_level_mut() else {
        return Vec::new();
    };
    let rects: Vec<_> = pane_rects(tile.axis, tile.children.iter().map(|(_, frac)| *frac), (area_w, area_h))
        .into_iter()
        .map(|(x, y, w, h)| (x + area_x, y + area_y, w, h))
        .collect();
    let active = tile.active;
    let highlight_ids = tile.highlight_ids.clone();

    let mut elements = Vec::new();

    // Pushed first — frontmost, per this module's front-to-back push
    // order (index 0 renders on top) — so the border actually renders
    // *above* the active pane's content instead of being hidden behind
    // it. A border, not a filled rect: four thin solid-color strips
    // around the pane's edges, since a single filled rectangle would
    // just hide its content instead of framing it.
    if let Some(&(x, y, w, h)) = rects.get(active) {
        const BASE_BORDER: i32 = 3;
        let border = scaled(BASE_BORDER, scale).max(1);
        let color = [0.3, 0.6, 1.0, 1.0];
        let strips = [
            (x, y, w, border),                // top
            (x, y + h - border, w, border),   // bottom
            (x, y, border, h),                // left
            (x + w - border, y, border, h),   // right
        ];
        for (id, (sx, sy, sw, sh)) in highlight_ids.into_iter().zip(strips) {
            let background = SolidColorRenderElement::new(
                id,
                Rectangle::<i32, smithay::utils::Physical>::new(
                    smithay::utils::Point::from((sx, sy)),
                    smithay::utils::Size::from((sw, sh)),
                ),
                CommitCounter::default(),
                color,
                Kind::Unspecified,
            );
            elements.push(OutputRenderElements::from(background));
        }
    }

    for ((child, _), (x, y, _, _)) in tile.children.iter_mut().zip(rects) {
        let hut = child.focused_hut_mut();
        let Some(texture) = hut.redraw(renderer) else {
            continue;
        };
        let element = TextureRenderElement::from_texture_with_damage(
            hut.element_id.clone(),
            renderer.context_id(),
            (x as f64, y as f64),
            texture,
            texture_buffer_scale(scale),
            Transform::Normal,
            None,
            None,
            None,
            None,
            hut.element_damage_snapshot(),
            Kind::Unspecified,
        );
        elements.push(OutputRenderElements::from(element));
    }

    elements
}
