use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::desktop::space::SpaceRenderElements;

smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem;
    Space = SpaceRenderElements<R, E>,
    Terminal = MemoryRenderBufferRenderElement<R>,
}
