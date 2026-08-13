use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportAll, ImportMem, RendererSuper};
use smithay::desktop::Window;
use smithay::desktop::space::{SpaceRenderElements, space_render_elements};
use smithay::output::Output;
use smithay::utils::Transform;

use crate::State;
use crate::{chrome, docks, switcher};

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
// at an explicit size/location), just wrapping a different Hut's cached
// texture. `SolidColor` backs the popup's background panel and highlight
// border (`SolidColorRenderElement` isn't generic over `R` at all).
smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem;
    Space = SpaceRenderElements<R, E>,
    Terminal = TextureRenderElement<<R as RendererSuper>::TextureId>,
    SolidColor = SolidColorRenderElement,
}

/// Build one frame's worth of render elements (Alt-Tab popup, tab-strip
/// chrome, docked-handle chrome, then either the focused Hut's terminal
/// texture or its visible Main Window/Sub-Windows/Alerts via `state.space`)
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
    let show_terminal = state.showing_terminal_effective();
    let mut elements = Vec::new();

    // Only the focused Hut normally gets redrawn (see Phase 2.6's
    // damage-avoidance work) — but the Alt-Tab popup shows every Hut's
    // thumbnail, so while it's open they all need fresh cached textures.
    // Redundant with the focused Hut's own redraw further down (cheap: a
    // second `redraw` call in the same tick is a no-op cache hit, since
    // damage was already reset by the first).
    if state.stack.is_previewing() {
        for hut in state.stack.huts_mut() {
            hut.redraw(renderer);
        }
    }

    // Pushed first (frontmost — `render_output`/`render_frame` take
    // elements in front-to-back order) so the popup sits on top of
    // whatever's below, regardless of whether that's the terminal or a
    // client window; empty when no preview session is open.
    elements.extend(switcher::build(&state.stack, size, renderer));

    // Tab-strip chrome (Phase 4) — on top of the terminal/window content
    // but still below the Alt-Tab popup above. Empty when the focused
    // Hut has no Main Windows.
    elements.extend(chrome::build(state.stack.focused_mut(), renderer));

    // Docked Sub-Window handles (Phase 5) — same z-order slot as the tab
    // strip, only shown alongside the Main Window they belong to (never
    // while the terminal itself is the visible view).
    if !show_terminal {
        elements.extend(docks::build(state.stack.focused_mut(), renderer, size));
    }

    if show_terminal {
        let hut = state.stack.focused_mut();
        if let Some(texture) = hut.redraw(renderer) {
            // `from_texture_with_damage`, not `from_static_texture` — the
            // terminal's content genuinely changes every keystroke, and
            // `from_static_texture` is documented by Smithay as creating
            // an element with no damage tracking at all (see
            // `Hut::damage_tracker`'s doc comment).
            let element = TextureRenderElement::from_texture_with_damage(
                hut.element_id.clone(),
                renderer.context_id(),
                (0.0, 0.0),
                texture,
                1,
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
