use std::sync::OnceLock;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, get_parent, is_sync_subsurface, with_states,
};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, with_fractional_scale};
use smithay::wayland::shm::{ShmHandler, ShmState};

use super::{layer_shell, xdg_shell};
use crate::State;
use crate::state::ClientState;

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Every client we accept gets a `ClientState` attached in
        // `insert_client`; this fallback only matters if that invariant is
        // ever violated, in which case we still return something valid
        // rather than panicking.
        static FALLBACK: OnceLock<CompositorClientState> = OnceLock::new();
        match client.get_data::<ClientState>() {
            Some(data) => &data.compositor_state,
            None => FALLBACK.get_or_init(CompositorClientState::default),
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.request_redraw();
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            // Looked up across every ConsoleHut, not just `self.space` — a
            // background ConsoleHut's window still needs its commit bookkeeping
            // even while it isn't the visible one.
            if let Some(window) = self.find_window_by_surface(&root) {
                window.on_commit();
            }
        }

        let window = self.find_window_by_surface(surface);
        xdg_shell::handle_commit(&mut self.popups, window, surface);
        layer_shell::handle_commit(self, surface);
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

/// `wp_fractional_scale_v1` — see `state.rs`'s `fractional_scale_manager_state`
/// doc comment. mudhuts is single-output, so there's no anvil-style
/// "which output does this surface actually scan out from" question to
/// answer here — every surface gets the one output's scale, full stop.
impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let Some(output) = self.output.as_ref() else {
            return;
        };
        let scale = output.current_scale().fractional_scale();
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional_scale| {
                fractional_scale.set_preferred_scale(scale);
            });
        });
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
