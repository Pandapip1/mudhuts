//! The Alt-Tab preview popup: a horizontal strip of thumbnails, one per
//! Hut in The Stack, with a highlight border around whichever one is
//! currently previewed — see the plan's Phase 3.5 notes. Reuses each
//! Hut's already-rendered cached texture ([`Hut::cached_texture`]) scaled
//! down via an explicit `size` override, rather than a new rendering
//! pipeline: `TextureRenderElement` composites a texture at whatever
//! size/location it's given regardless of the texture's native size.

use smithay::backend::renderer::{Renderer, Texture};
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::render::OutputRenderElements;
use crate::stack::HutStack;

const THUMB_SIZE: (i32, i32) = (220, 140);
const GAP: i32 = 20;
const PADDING: i32 = 24;
const HIGHLIGHT_MARGIN: i32 = 6;

type Element = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// Build the popup's render elements in front-to-back order (see
/// `winit_backend.rs`, which pushes these ahead of the normal background
/// elements — index 0 renders on top), or an empty list if no preview
/// session is open.
pub fn build(stack: &HutStack, output_size: (i32, i32), renderer: &GlesRenderer) -> Vec<Element> {
    if !stack.is_previewing() {
        return Vec::new();
    }

    let count = stack.len().max(1) as i32;
    let panel_w = count * THUMB_SIZE.0 + (count - 1).max(0) * GAP + 2 * PADDING;
    let panel_h = THUMB_SIZE.1 + 2 * PADDING;
    let panel_x = (output_size.0 - panel_w) / 2;
    let panel_y = (output_size.1 - panel_h) / 2;

    let preview_index = stack.preview_index();
    let mut elements = Vec::new();

    for (i, hut) in stack.huts().enumerate() {
        let x = panel_x + PADDING + i as i32 * (THUMB_SIZE.0 + GAP);
        let y = panel_y + PADDING;

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
            let element = TextureRenderElement::from_static_texture(
                Id::new(),
                renderer.context_id(),
                (x as f64, y as f64),
                texture,
                1,
                Transform::Normal,
                None,
                Some(src),
                Some(Size::from(THUMB_SIZE)),
                None,
                Kind::Unspecified,
            );
            elements.push(Element::from(element));
        }

        if i == preview_index {
            let geometry = Rectangle::<i32, Physical>::new(
                Point::from((x - HIGHLIGHT_MARGIN, y - HIGHLIGHT_MARGIN)),
                Size::from((
                    THUMB_SIZE.0 + 2 * HIGHLIGHT_MARGIN,
                    THUMB_SIZE.1 + 2 * HIGHLIGHT_MARGIN,
                )),
            );
            let highlight = SolidColorRenderElement::new(
                Id::new(),
                geometry,
                smithay::backend::renderer::utils::CommitCounter::default(),
                [0.3, 0.6, 1.0, 1.0],
                Kind::Unspecified,
            );
            elements.push(Element::from(highlight));
        }
    }

    let panel = SolidColorRenderElement::new(
        Id::new(),
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
