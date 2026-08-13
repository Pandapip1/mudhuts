//! The Alt-Tab preview popup: a horizontal strip of thumbnails, one per
//! Hut in The Stack, with a highlight border around whichever one is
//! currently previewed — see the plan's Phase 3.5 notes. Reuses each
//! Hut's already-rendered cached texture ([`Hut::cached_texture`]) scaled
//! down via an explicit `size` override, rather than a new rendering
//! pipeline: `TextureRenderElement` composites a texture at whatever
//! size/location it's given regardless of the texture's native size.

use smithay::backend::renderer::{Renderer, Texture};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::render::OutputRenderElements;
use crate::stack::HutStack;

/// Base sizes (scale 1.0) — scaled via `crate::render::scaled` wherever
/// they're actually used, so the popup stays the same apparent size
/// regardless of the output's real DPI scale.
const THUMB_SIZE: (i32, i32) = (220, 140);
const GAP: i32 = 20;
const PADDING: i32 = 24;
const HIGHLIGHT_MARGIN: i32 = 6;

type Element = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// Build the popup's render elements in front-to-back order (see
/// `winit_backend.rs`, which pushes these ahead of the normal background
/// elements — index 0 renders on top), or an empty list if no preview
/// session is open.
pub fn build(stack: &HutStack, output_size: (i32, i32), renderer: &GlesRenderer, scale: f64) -> Vec<Element> {
    if !stack.is_previewing() {
        return Vec::new();
    }

    // Physical-pixel math throughout this function (panel/thumbnail
    // positions, the highlight border) — `crate::render::scaled` applied
    // directly, since none of it flows through an `Element::geometry`
    // call that would otherwise re-apply the output scale on its own
    // (unlike the thumbnail's own `size` below).
    let thumb_w = crate::render::scaled(THUMB_SIZE.0, scale);
    let thumb_h = crate::render::scaled(THUMB_SIZE.1, scale);
    let gap = crate::render::scaled(GAP, scale);
    let padding = crate::render::scaled(PADDING, scale);
    let highlight_margin = crate::render::scaled(HIGHLIGHT_MARGIN, scale);

    let count = stack.len().max(1) as i32;
    let panel_w = count * thumb_w + (count - 1).max(0) * gap + 2 * padding;
    let panel_h = thumb_h + 2 * padding;
    let panel_x = (output_size.0 - panel_w) / 2;
    let panel_y = (output_size.1 - panel_h) / 2;

    let preview_index = stack.preview_index();
    let mut elements = Vec::new();

    for (i, hut) in stack.top_level_huts().enumerate() {
        let x = panel_x + padding + i as i32 * (thumb_w + gap);
        let y = panel_y + padding;

        if let Some(texture) = hut.cached_texture() {
            // `size` below is the *destination* size (the thumbnail) —
            // without an explicit `src`, `TextureRenderElement` defaults
            // the source rect to that same override size rather than the
            // texture's real dimensions, so it'd sample only the
            // thumbnail-sized top-left corner of the full-resolution
            // terminal texture (cropped, not scaled). Pass the real size
            // explicitly so `size` is free to scale independently.
            let src = Rectangle::<f64, Logical>::from_size(Size::from((
                texture.width() as f64,
                texture.height() as f64,
            )));
            // `from_texture_with_damage`, not `from_static_texture` — this
            // wraps the same ever-changing terminal content as the main
            // render element (see `Hut::damage_tracker`'s doc comment),
            // just scaled down.
            //
            // The *base* (unscaled) `THUMB_SIZE`, not `thumb_w`/`thumb_h`
            // above — this `size` is a `Logical` destination override, and
            // `Element::geometry(scale)` re-applies the output's scale to
            // it automatically. Passing the already-scaled physical size
            // here would double-apply `scale` (see
            // `render::texture_buffer_scale`'s doc comment for the same
            // trap in a different guise). The surrounding physical-pixel
            // math above (`thumb_w`/`thumb_h`, used for *position* only)
            // has to stay scaled itself since `location` is taken
            // literally, unlike `size`.
            let thumb_size = Size::from(THUMB_SIZE);
            let element = TextureRenderElement::from_texture_with_damage(
                hut.thumbnail_id.clone(),
                renderer.context_id(),
                (x as f64, y as f64),
                texture,
                1,
                Transform::Normal,
                None,
                Some(src),
                Some(thumb_size),
                None,
                hut.element_damage_snapshot(),
                Kind::Unspecified,
            );
            elements.push(Element::from(element));
        }

        if i == preview_index {
            let geometry = Rectangle::<i32, Physical>::new(
                Point::from((x - highlight_margin, y - highlight_margin)),
                Size::from((thumb_w + 2 * highlight_margin, thumb_h + 2 * highlight_margin)),
            );
            let highlight = SolidColorRenderElement::new(
                hut.thumbnail_highlight_id.clone(),
                geometry,
                smithay::backend::renderer::utils::CommitCounter::default(),
                [0.3, 0.6, 1.0, 1.0],
                Kind::Unspecified,
            );
            elements.push(Element::from(highlight));
        }
    }

    let panel = SolidColorRenderElement::new(
        stack.panel_id(),
        Rectangle::<i32, Physical>::new(
            Point::from((panel_x, panel_y)),
            Size::from((panel_w, panel_h)),
        ),
        smithay::backend::renderer::utils::CommitCounter::default(),
        [0.05, 0.05, 0.05, 0.85],
        Kind::Unspecified,
    );
    elements.push(Element::from(panel));

    elements
}
