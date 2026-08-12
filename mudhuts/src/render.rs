use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::{ImportAll, ImportMem, RendererSuper};
use smithay::desktop::space::SpaceRenderElements;

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
