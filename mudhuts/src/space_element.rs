//! [`HutSpaceElement`] — what a Hut-tree node's own `Space` can hold, per the
//! composable Hut hierarchy RFC's Q1 proposal (see
//! `docs/rfcs/composable-hut-hierarchy.md`): a real Wayland window (a leaf
//! Sub-Hut, today just [`Window`]), or another Hut node's own
//! already-composited output ([`CompositedTexture`]) — wrapped so
//! `space_render_elements` can composite both uniformly, generically, at
//! every level of the tree, not just one privileged branch the way
//! `render.rs`'s pre-redesign `build_frame_elements` used to.
//!
//! Originally built and byte-for-byte live-verified as a gated, comparison-
//! only prototype (migration step 3, `git log` for this file's earlier
//! `hut_space.rs` history has the full spike/verification notes) — promoted
//! to real production types here for migration step 5 sub-step 2
//! (`ConsoleHut` getting its own `Space<HutSpaceElement>`), once there was
//! real code to use them instead of just a comparison harness to prove they
//! work.

use std::rc::Rc;

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::DamageSnapshot;
use smithay::backend::renderer::{Renderer, Texture};
use smithay::backend::renderer::element::Id;
use smithay::desktop::Window;
use smithay::desktop::space::SpaceElement;
use smithay::output::{Mode, Output, PhysicalProperties, Scale as OutputScale, Subpixel};
use smithay::utils::{Buffer, IsAlive, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

/// A single already-rendered texture, wrapped so it can sit in a
/// [`Space<HutSpaceElement>`](smithay::desktop::space::Space) as a
/// `Composited` sibling to real mapped `Window`s.
///
/// Freshly built every time it's used (like every other cached texture in
/// this codebase — see `ConsoleHut::redraw`'s doc comment) — there's no
/// persistent identity to invalidate, so [`IsAlive::alive`] is trivially
/// always `true` and [`PartialEq`] compares by a fresh per-instance marker
/// rather than pixel content (two *different* `CompositedTexture`s should
/// never compare equal; an instance is never compared against itself here).
#[derive(Clone)]
pub struct CompositedTexture {
    id: Id,
    texture: GlesTexture,
    size: Size<i32, Logical>,
    /// The integer buffer-scale used to derive `size` from `texture`'s own
    /// physical-pixel size, and passed to
    /// `TextureRenderElement::from_texture_with_damage` below — has to be
    /// the *same* value in both places, or the two disagree about this
    /// element's real on-screen size (see `render::texture_buffer_scale`'s
    /// own doc comment: the buffer-scale argument, not an explicit `size`
    /// override, is what avoids double-applying the output's scale).
    buffer_scale: i32,
    /// Real damage tracking, threaded through from whatever produced
    /// `texture` (e.g. `ConsoleHut::element_damage_snapshot`) — *not*
    /// `from_static_texture`'s implicit "no damage" snapshot, which is
    /// only correct for genuinely static content. A `CompositedTexture`
    /// commonly wraps something that changes every frame (a terminal
    /// grid); `from_static_texture` here would silently break the outer,
    /// per-element damage tracker under the udev/DRM backend specifically
    /// (`DrmCompositor` — the winit backend's simpler single-buffer
    /// tracker wouldn't show the bug, but it's real all the same; see
    /// `ConsoleHut::damage_tracker`'s own doc comment for the exact same
    /// trap in a different guise).
    damage: DamageSnapshot<i32, Buffer>,
    marker: Rc<()>,
}

impl CompositedTexture {
    /// `scale` is the real output scale (the same fractional value
    /// `State::output_scale()` reports), rounded to the nearest integer
    /// buffer-scale internally via `render::texture_buffer_scale` —
    /// matching every other texture element in this codebase. Getting this
    /// wrong (e.g. hardcoding `1`, as an earlier prototype version of this
    /// type deliberately did — see the RFC's step 3 notes, "this prototype
    /// only ever composites at scale 1.0") would silently mis-scale
    /// whatever this wraps on any real, non-1.0-scale display.
    pub fn new(id: Id, texture: GlesTexture, scale: f64, damage: DamageSnapshot<i32, Buffer>) -> Self {
        let buffer_scale = crate::render::texture_buffer_scale(scale);
        let size = texture.size().to_logical(buffer_scale, Transform::Normal);
        Self { id, texture, size, buffer_scale, damage, marker: Rc::new(()) }
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
        // No `HutSpaceElement::Composited` is a real click target on its
        // own — whatever produced this texture already has its own
        // `HitTestable` impl (or is one of chrome's non-Hut-tree
        // implementors) for that; this element only carries pixels.
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
        // `from_texture_with_damage`, not `from_static_texture` — see
        // `Self::damage`'s doc comment on why real damage tracking matters
        // here, the same reasoning as `ConsoleHut::damage_tracker`'s.
        let element = TextureRenderElement::from_texture_with_damage(
            self.id.clone(),
            renderer.context_id(),
            location.to_f64(),
            self.texture.clone(),
            self.buffer_scale,
            Transform::Normal,
            Some(alpha),
            None,
            None,
            None,
            self.damage.clone(),
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

/// What a Hut-tree node's own [`Space`](smithay::desktop::space::Space) can
/// hold — see this module's doc comment.
#[derive(Clone)]
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
///
/// Always `Transform::Normal`/scale `1` — a Hut-tree node's own `Space`
/// composites at whatever `scale: Scale<f64>` `space_render_elements`
/// itself is called with (an explicit parameter, not derived from the
/// output), exactly like `render.rs`'s pre-redesign `state.space` call
/// already did (`space_render_elements(..., output, 1.0)`) — client buffer
/// scale is handled internally by each `Window`'s own `AsRenderElements`
/// impl regardless of what this synthetic output reports.
pub fn synthetic_output(name: &str, size: (i32, i32)) -> Output {
    let output = Output::new(
        name.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "mudhuts".into(),
            model: "hut-space".into(),
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
