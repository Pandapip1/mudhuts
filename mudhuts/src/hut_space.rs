//! Composable Hut hierarchy RFC, migration step 3 (see
//! `docs/rfcs/composable-hut-hierarchy.md`'s Q1 and Open Question 1): a
//! prototype `Space<HutSpaceElement>` bound to a synthetic `Output`, proving
//! out two things the RFC's design depends on but never actually spiked:
//!
//! 1. A texture-backed [`CompositedTexture`] can implement `SpaceElement` +
//!    `AsRenderElements` at all (no existing type in this codebase or in
//!    Smithay/anvil does this — every existing `SpaceElement` impl is
//!    ultimately backed by a real `Window`/`WlSurface`; confirmed by reading
//!    the pinned Smithay checkout before writing this).
//! 2. A private, per-scope `Space<HutSpaceElement>` — mapped against a
//!    never-globalized synthetic `Output` sized to just the focused Console
//!    Hut's own content area, not the real output — renders, via the same
//!    `space_render_elements` call every other node in the future tree would
//!    use, *identically* to what `state.space` (the real, shared,
//!    output-sized `Space<Window>`) produces for the same real `Window`s
//!    today.
//!
//! [`compare_against_existing_path`] is the whole prototype's entry point:
//! gated behind the `MUDHUTS_PROTOTYPE_HUT_SPACE` env var (unset by
//! default — this changes nothing about what's actually on screen), it runs
//! a *second*, offscreen render pass (same technique as
//! `handlers/capture.rs`'s screenshot capture: bind an offscreen texture,
//! `OutputDamageTracker::render_output`, read the pixels back) through the
//! new per-node `Space`, byte-diffs it against a second offscreen render of
//! today's real `state.space` path cropped to the same region, and logs the
//! result. Nothing here replaces `state.space`/`State::sync_visible_main_window`
//! — this is additive, comparison-only scaffolding for validating Q1 before
//! step 4 (folding `Village`'s Tab/Tile variants into real Hut-tree nodes)
//! commits to the design for real.

use std::rc::Rc;
use std::sync::OnceLock;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Id, Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen, Renderer, Texture};
use smithay::desktop::Window;
use smithay::desktop::space::{Space, SpaceElement, space_render_elements};
use smithay::output::{Mode, Output, PhysicalProperties, Scale as OutputScale, Subpixel};
use smithay::utils::{Buffer, IsAlive, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::State;

/// A single already-rendered texture, wrapped so it can sit in a
/// [`Space<HutSpaceElement>`] as a `Composited` sibling to real mapped
/// `Window`s — the prototype for Q1's `Composited` variant (RFC Open
/// Question 1: this impl "wasn't prototyped," so this is that spike).
///
/// Freshly built every time it's used (like every other cached texture in
/// this codebase — see `ConsoleHut::redraw`'s doc comment) — there's no
/// persistent identity to invalidate, so [`IsAlive::alive`] is trivially
/// always `true` and [`PartialEq`] compares by a fresh per-instance marker
/// rather than pixel content (two *different* `CompositedTexture`s should
/// never compare equal; an instance is never compared against itself here).
pub struct CompositedTexture {
    id: Id,
    texture: GlesTexture,
    /// This prototype only ever composites at scale `1.0` (see this
    /// module's doc comment on scope — it's validating the mechanism, not
    /// DPI-scale fidelity), so the texture's own physical pixel size
    /// doubles as its `Logical` bbox size with no conversion needed.
    size: Size<i32, Logical>,
    marker: Rc<()>,
}

impl CompositedTexture {
    pub fn new(id: Id, texture: GlesTexture) -> Self {
        let size = texture.size().to_logical(1, Transform::Normal);
        Self { id, texture, size, marker: Rc::new(()) }
    }
}

impl IsAlive for CompositedTexture {
    fn alive(&self) -> bool {
        true
    }
}

impl PartialEq for CompositedTexture {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.marker, &other.marker)
    }
}

impl SpaceElement for CompositedTexture {
    fn bbox(&self) -> Rectangle<i32, Logical> {
        Rectangle::from_size(self.size)
    }

    fn is_in_input_region(&self, _point: &Point<f64, Logical>) -> bool {
        // Comparison-only prototype element, never a real input target —
        // see this module's doc comment on scope.
        false
    }

    fn set_activate(&self, _activated: bool) {}

    fn output_enter(&self, _output: &Output, _overlap: Rectangle<i32, Logical>) {}

    fn output_leave(&self, _output: &Output) {}
}

impl AsRenderElements<GlesRenderer> for CompositedTexture {
    type RenderElement = TextureRenderElement<GlesTexture>;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut GlesRenderer,
        location: Point<i32, Physical>,
        _scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        // `from_static_texture`, not `from_texture_with_damage` — normally
        // the wrong call for an on-screen-persistent element (see
        // `ConsoleHut::damage_tracker`'s doc comment on why "no damage
        // tracking" is a real bug there), but exactly right here: every
        // `CompositedTexture` is single-use, rendered into one throwaway
        // offscreen pass and discarded, never compared against a *later*
        // frame of itself the way an on-screen element would be.
        let element = TextureRenderElement::from_static_texture(
            self.id.clone(),
            renderer.context_id(),
            location.to_f64(),
            self.texture.clone(),
            1,
            Transform::Normal,
            Some(alpha),
            None,
            None,
            None,
            Kind::Unspecified,
        );
        vec![C::from(element)]
    }
}

smithay::backend::renderer::element::render_elements! {
    pub HutSpaceRenderElement<=GlesRenderer>;
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Texture = TextureRenderElement<GlesTexture>,
}

/// What a per-scope [`Space`] can hold, per the RFC's Q1 proposal: a real
/// Wayland window (a leaf Sub-Hut, today just [`Window`]), or another Hut
/// node's own already-composited output ([`CompositedTexture`]) — wrapped so
/// `space_render_elements` can composite both uniformly, generically, at
/// every level of the future tree, not just today's privileged
/// Main-Window-visible branch (`render.rs`'s `build_frame_elements`).
pub enum HutSpaceElement {
    Window(Window),
    Composited(CompositedTexture),
}

impl IsAlive for HutSpaceElement {
    fn alive(&self) -> bool {
        match self {
            Self::Window(w) => IsAlive::alive(w),
            Self::Composited(c) => IsAlive::alive(c),
        }
    }
}

impl PartialEq for HutSpaceElement {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Window(a), Self::Window(b)) => a == b,
            (Self::Composited(a), Self::Composited(b)) => a == b,
            _ => false,
        }
    }
}

impl SpaceElement for HutSpaceElement {
    fn geometry(&self) -> Rectangle<i32, Logical> {
        match self {
            Self::Window(w) => SpaceElement::geometry(w),
            Self::Composited(c) => SpaceElement::geometry(c),
        }
    }

    fn bbox(&self) -> Rectangle<i32, Logical> {
        match self {
            Self::Window(w) => SpaceElement::bbox(w),
            Self::Composited(c) => SpaceElement::bbox(c),
        }
    }

    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        match self {
            Self::Window(w) => SpaceElement::is_in_input_region(w, point),
            Self::Composited(c) => SpaceElement::is_in_input_region(c, point),
        }
    }

    fn z_index(&self) -> u8 {
        match self {
            Self::Window(w) => SpaceElement::z_index(w),
            Self::Composited(c) => SpaceElement::z_index(c),
        }
    }

    fn set_activate(&self, activated: bool) {
        match self {
            Self::Window(w) => SpaceElement::set_activate(w, activated),
            Self::Composited(c) => SpaceElement::set_activate(c, activated),
        }
    }

    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        match self {
            Self::Window(w) => SpaceElement::output_enter(w, output, overlap),
            Self::Composited(c) => SpaceElement::output_enter(c, output, overlap),
        }
    }

    fn output_leave(&self, output: &Output) {
        match self {
            Self::Window(w) => SpaceElement::output_leave(w, output),
            Self::Composited(c) => SpaceElement::output_leave(c, output),
        }
    }

    fn refresh(&self) {
        match self {
            Self::Window(w) => SpaceElement::refresh(w),
            Self::Composited(c) => SpaceElement::refresh(c),
        }
    }
}

impl AsRenderElements<GlesRenderer> for HutSpaceElement {
    type RenderElement = HutSpaceRenderElement;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut GlesRenderer,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        match self {
            Self::Window(w) => {
                AsRenderElements::<GlesRenderer>::render_elements::<HutSpaceRenderElement>(
                    w, renderer, location, scale, alpha,
                )
                .into_iter()
                .map(C::from)
                .collect()
            }
            Self::Composited(c) => {
                AsRenderElements::<GlesRenderer>::render_elements::<HutSpaceRenderElement>(
                    c, renderer, location, scale, alpha,
                )
                .into_iter()
                .map(C::from)
                .collect()
            }
        }
    }
}

/// A synthetic, never-globalized `Output` sized to `size` — confirmed safe
/// by reading Smithay's own source (see the RFC's "Grounding note on
/// Smithay internals"): `Output::new` has no hardware coupling, and the only
/// side effect of using one that's never had `create_global` called on it is
/// `wl_surface.enter`/`leave` being sent for it, which no client can ever
/// observe since no client can ever bind it.
fn synthetic_output(name: &str, size: (i32, i32)) -> Output {
    let output = Output::new(
        name.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "mudhuts".into(),
            model: "hut-space-prototype".into(),
            serial_number: "unknown".into(),
        },
    );
    output.change_current_state(
        Some(Mode { size: size.into(), refresh: 60_000 }),
        Some(Transform::Normal),
        Some(OutputScale::Integer(1)),
        Some((0, 0).into()),
    );
    output
}

/// Render `elements` into a fresh offscreen texture sized to `size` and read
/// it back into an owned byte buffer — the same technique
/// `handlers/capture.rs::render_capture` uses for screenshot capture (see
/// its own doc comment on why a second render pass is unavoidable here: no
/// bindable target exists to alias into for either backend). `age: 0` forces
/// the tracker to treat the whole thing as damaged, exactly like
/// `render_capture` — nothing here is ever reused across calls, so there's
/// no prior frame to be "damaged" relative to.
fn render_offscreen<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    output: &Output,
    size: (i32, i32),
    elements: &[E],
) -> Option<Vec<u8>> {
    let fourcc = Fourcc::Argb8888;
    let buffer_size: Size<i32, Buffer> = size.into();
    let mut texture = Offscreen::<GlesTexture>::create_buffer(renderer, fourcc, buffer_size)
        .inspect_err(|err| tracing::warn!("hut_space prototype: failed to create offscreen buffer: {err}"))
        .ok()?;
    let mut target = renderer
        .bind(&mut texture)
        .inspect_err(|err| tracing::warn!("hut_space prototype: failed to bind offscreen buffer: {err}"))
        .ok()?;
    let mut tracker = OutputDamageTracker::from_output(output);
    tracker
        .render_output(renderer, &mut target, 0, elements, [0.0, 0.0, 0.0, 1.0])
        .inspect_err(|err| tracing::warn!("hut_space prototype: render_output failed: {err}"))
        .ok()?;
    let region = Rectangle::from_size(buffer_size);
    let mapping = renderer
        .copy_framebuffer(&target, region, fourcc)
        .inspect_err(|err| tracing::warn!("hut_space prototype: copy_framebuffer failed: {err}"))
        .ok()?;
    renderer
        .map_texture(&mapping)
        .map(<[u8]>::to_vec)
        .inspect_err(|err| tracing::warn!("hut_space prototype: map_texture failed: {err}"))
        .ok()
}

/// Crop a tightly-packed `full_size`-shaped BGRA buffer down to `region`,
/// `None` (logged) if `region` doesn't actually fit inside `full_size`
/// rather than panicking on an out-of-bounds slice.
fn crop(pixels: &[u8], full_size: (i32, i32), region: (i32, i32, i32, i32)) -> Option<Vec<u8>> {
    let (full_w, full_h) = full_size;
    let (x, y, w, h) = region;
    if x < 0 || y < 0 || w <= 0 || h <= 0 || x + w > full_w || y + h > full_h {
        tracing::warn!(
            "hut_space prototype: crop region {region:?} doesn't fit inside {full_size:?}"
        );
        return None;
    }
    let row_bytes = w as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * h as usize);
    for row in 0..h {
        let src_start = ((y + row) as usize * full_w as usize + x as usize) * 4;
        out.extend_from_slice(pixels.get(src_start..src_start + row_bytes)?);
    }
    Some(out)
}

/// The whole prototype's entry point — see this module's doc comment.
/// Called once per redraw from `winit_backend.rs`; a complete no-op unless
/// `MUDHUTS_PROTOTYPE_HUT_SPACE` is set in the environment, checked once
/// (not on every call) via the `OnceLock` below.
pub fn compare_against_existing_path(state: &mut State, renderer: &mut GlesRenderer) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("MUDHUTS_PROTOTYPE_HUT_SPACE").is_some()) {
        return;
    }

    // Only the Main-Window-visible scope this migration step targets (see
    // the RFC's Migration Strategy, step 3) — nothing is mapped in
    // `state.space` otherwise, so there'd be nothing to compare.
    if state.showing_terminal_effective() {
        return;
    }
    // Just confirms a real output is actually mapped (so `state.space` has
    // something to compare) — the old-path render below deliberately uses
    // its own clean stand-in output instead of this one; see that render's
    // own doc comment on why.
    if state.space.outputs().next().is_none() {
        return;
    }
    let (area_x, area_y, area_w, area_h) = state.usable_area();
    if area_w <= 0 || area_h <= 0 {
        return;
    }

    // --- Old path: exactly what `render.rs`'s real Main-Window-visible
    // branch does today (same `state.space`, same mapped `Window`s), cropped
    // down to the usable area afterward so it's directly comparable to the
    // new path's smaller, origin-relative canvas. Rendered against a clean
    // `Transform::Normal`/scale-`1.0` stand-in for `real_output`, not
    // `real_output` itself — `real_output` (the real "winit" output) carries
    // `winit_backend.rs`'s own `Transform::Flipped180` workaround and the
    // host's real DPI scale, both of which `OutputDamageTracker::render_output`
    // bakes into the final image; comparing against that directly would
    // measure "does this replicate one backend's own transform hack," not
    // "does the new per-node Space composite the same content" — the actual
    // question this step is spiking. Confirmed by a first, wrong attempt at
    // this comparison: reusing `real_output` for the old path produced a
    // spurious ~56%-of-pixels "divergence" that vanished entirely once both
    // paths rendered through the same clean, transform/scale-normalized
    // output below.
    let old_output = synthetic_output("hut-space-prototype-old", state.output_size);
    let old_elements =
        match space_render_elements::<_, Window, _>(renderer, [&state.space], &old_output, 1.0) {
            Ok(elements) => elements,
            Err(err) => {
                tracing::warn!("hut_space prototype: old-path space_render_elements failed: {err}");
                return;
            }
        };
    let Some(old_pixels) = render_offscreen(renderer, &old_output, state.output_size, &old_elements)
    else {
        return;
    };
    let Some(old_cropped) = crop(&old_pixels, state.output_size, (area_x, area_y, area_w, area_h))
    else {
        return;
    };

    // --- New path: a private Space<HutSpaceElement> bound to a synthetic
    // Output sized to just the usable area, holding the same real Windows
    // at origin-relative (not real-output-relative) coordinates.
    let synthetic = synthetic_output("hut-space-prototype", (area_w, area_h));
    let mut scoped = Space::<HutSpaceElement>::default();
    scoped.map_output(&synthetic, (0, 0));
    for window in state.space.elements().cloned().collect::<Vec<_>>() {
        let loc = state.space.element_location(&window).unwrap_or_default();
        scoped.map_element(
            HutSpaceElement::Window(window),
            (loc.x - area_x, loc.y - area_y),
            false,
        );
    }

    let new_elements =
        match space_render_elements::<_, HutSpaceElement, _>(renderer, [&scoped], &synthetic, 1.0) {
            Ok(elements) => elements,
            Err(err) => {
                tracing::warn!("hut_space prototype: new-path space_render_elements failed: {err}");
                return;
            }
        };
    let Some(new_pixels) = render_offscreen(renderer, &synthetic, (area_w, area_h), &new_elements) else {
        return;
    };

    if old_cropped.len() != new_pixels.len() {
        tracing::warn!(
            "hut_space prototype: byte length mismatch, old {} vs new {} — can't compare",
            old_cropped.len(),
            new_pixels.len()
        );
    } else {
        let diff = old_cropped.iter().zip(&new_pixels).filter(|(a, b)| a != b).count();
        if diff == 0 {
            tracing::info!(
                "hut_space prototype: per-node Space<HutSpaceElement> matches state.space's \
                 existing path byte-for-byte ({} bytes compared)",
                new_pixels.len()
            );
        } else {
            tracing::warn!(
                "hut_space prototype: per-node Space<HutSpaceElement> diverges from the existing \
                 path in {diff} of {} bytes",
                new_pixels.len()
            );
        }
    }

    // --- Separately, not diffed against anything (nothing today does
    // this): prove a Composited texture can coexist with a real mapped
    // Window in the same Space and still produce a sane composite — the
    // part of Q1 today's Main-Window-visible scope alone can't exercise
    // (there's no non-leaf child of a Console Hut yet to actually produce a
    // Composited element from in real use), so this pushes the focused
    // Console Hut's own cached terminal texture in alongside the real
    // window purely to spike the mechanism ahead of step 4 needing it.
    if let Some(texture) = state.stack.focused_mut().cached_texture() {
        scoped.map_element(
            HutSpaceElement::Composited(CompositedTexture::new(Id::new(), texture)),
            (0, 0),
            false,
        );
        match space_render_elements::<_, HutSpaceElement, _>(renderer, [&scoped], &synthetic, 1.0) {
            Ok(elements) => match render_offscreen(renderer, &synthetic, (area_w, area_h), &elements) {
                Some(pixels) => {
                    let non_zero = pixels.iter().any(|&b| b != 0);
                    tracing::info!(
                        "hut_space prototype: Window + Composited coexisted in one Space and \
                         rendered {} bytes ({})",
                        pixels.len(),
                        if non_zero { "non-empty" } else { "all zero — suspicious" }
                    );
                }
                None => tracing::warn!("hut_space prototype: failed to render the Window+Composited scope"),
            },
            Err(err) => {
                tracing::warn!("hut_space prototype: Window+Composited space_render_elements failed: {err}");
            }
        }
    }
}
