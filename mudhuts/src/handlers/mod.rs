mod compositor;
pub(crate) mod layer_shell;
pub(crate) mod shell;
pub(crate) mod xdg_shell;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::input::dnd::DndGrabHandler;
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::foreign_toplevel_list::{ForeignToplevelListHandler, ForeignToplevelListState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};

use crate::State;

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl PointerConstraintsHandler for State {}

impl DndGrabHandler for State {}
impl WaylandDndGrabHandler for State {}

impl OutputHandler for State {}

/// Client-buffer dmabuf import (`zwp_linux_dmabuf_v1`) — lets a client hand
/// over a GPU buffer directly instead of a plain SHM buffer mudhuts would
/// otherwise have to copy/re-upload on every commit. Only ever reachable
/// under the udev/DRM backend: `dmabuf_global` (see `state.rs`) is only
/// ever set by `udev_backend.rs::init_udev`, so no client ever sees this
/// global — and therefore this handler is never invoked — under
/// `winit_backend.rs`.
impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        let Some(renderer) = self.dmabuf_renderer.as_ref() else {
            // Shouldn't happen — this handler is only ever reachable once
            // `init_udev` has both created the global and set this — but
            // stay panic-free rather than assume it.
            notifier.failed();
            return;
        };
        match renderer.borrow_mut().import_dmabuf(&dmabuf, None) {
            Ok(_texture) => {
                let _ = notifier.successful::<State>();
            }
            Err(err) => {
                tracing::warn!("failed to import client dmabuf: {err}");
                notifier.failed();
            }
        }
    }
}

/// `ext_foreign_toplevel_list_v1` — see `state.rs`'s
/// `foreign_toplevel_list_state` doc comment. No requests of its own
/// worth reacting to (it's purely "here's what's open," not
/// "make/close this") — every actual `ForeignToplevelHandle` is created
/// directly by `handlers/xdg_shell.rs`'s `new_toplevel`, not from this
/// trait.
impl ForeignToplevelListHandler for State {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

smithay::delegate_dispatch2!(State);
