use std::sync::OnceLock;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::reexports::wayland_server::{Client, Resource};
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
        // `find_window_by_surface` walks every ConsoleHut across every
        // output — called on *every* wl_surface.commit from *every*
        // client, so the common case (no subsurfaces, `root == surface`)
        // reuses one lookup for both purposes below instead of paying it
        // twice per commit.
        let mut window_for_surface = None;
        let mut resolved_for_surface = false;
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            // Looked up across every ConsoleHut, not just the focused one's
            // own `space` — a background ConsoleHut's window still needs
            // its commit bookkeeping even while it isn't the visible one.
            let window_for_root = self.find_window_by_surface(&root);
            if let Some(window) = &window_for_root {
                window.on_commit();
            }
            if &root == surface {
                window_for_surface = window_for_root;
                resolved_for_surface = true;
            }
        }

        let window = if resolved_for_surface {
            window_for_surface
        } else {
            self.find_window_by_surface(surface)
        };
        xdg_shell::handle_commit(&mut self.popups, window, surface);
        layer_shell::handle_commit(self, surface);
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

/// `wp_fractional_scale_v1` — see `state.rs`'s `fractional_scale_manager_state`
/// doc comment.
impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // No real output exists yet (a client can race the compositor's
        // own connector enumeration at startup) — send nothing rather
        // than falling through to `output_scale_for`'s own out-of-range
        // default of `1.0`: this handler never re-pushes a corrected
        // scale later (see this field's own doc comment), so eagerly
        // sending a possibly-wrong `1.0` here would permanently lock the
        // client to it once a real, differently-scaled output shows up.
        if self.output.is_none() {
            return;
        }
        // This surface's own owning Hut/output, not `self.output` (the
        // focused one) — real multi-monitor, same as everywhere else. A
        // `wp_fractional_scale_v1` object is typically created right
        // after the surface itself, often before it's even a mapped
        // toplevel `find_window_by_surface` could resolve, so this
        // resolves ownership the same way `handlers/xdg_shell.rs`'s
        // `new_toplevel` picks a default Hut for a brand-new surface: via
        // the client's process ancestry, falling back to the focused
        // output only if that doesn't resolve to a known Hut.
        //
        // KNOWN TRADEOFF, not yet addressed: `find_owning_hut`'s ancestry
        // walk can do blocking `/proc/<pid>/{environ,stat}` reads, and
        // most modern toolkits request a fractional-scale object for
        // nearly every surface (toplevels, popups, subsurfaces, cursor
        // surfaces) — a client creating many surfaces in a burst (e.g.
        // session restore) pays repeated synchronous I/O on this single-
        // threaded event loop, uncached, duplicating the identical walk
        // `handlers/xdg_shell.rs`'s `new_toplevel` already does for the
        // same client. Deliberately not cached here: a naive
        // PID-keyed cache would return a *wrong* Hut for an unrelated
        // later client if the OS recycles that PID after the original
        // process exits, and there's no existing per-client cleanup hook
        // to invalidate it safely. Worth a real fix (e.g. keyed by
        // `Client` identity instead of raw PID, with cleanup on client
        // disconnect) if this ever shows up as real, measured latency.
        let output_index = surface
            .client()
            .and_then(|client| client.get_credentials(&self.display_handle).ok())
            .and_then(|creds| crate::ownership::find_owning_hut(creds.pid as u32, &self.stack))
            .and_then(|hut_id| self.stack.output_index_for_hut(hut_id))
            .unwrap_or_else(|| self.stack.focused_output_index());
        let scale = self.output_scale_for(output_index);
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
