pub(crate) mod capture;
mod compositor;
pub(crate) mod layer_shell;
mod session_lock;
pub(crate) mod shell;
pub(crate) mod xdg_shell;

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::input::dnd::DndGrabHandler;
use smithay::input::keyboard::LedState;
use smithay::input::pointer::CursorImageStatus;
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::foreign_toplevel_list::{ForeignToplevelListHandler, ForeignToplevelListState};
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::ext_data_control::{DataControlHandler, DataControlState};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::wayland::selection::{SelectionHandler, SelectionTarget};

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
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }

    /// Called by Smithay itself whenever a keyboard input/keymap change
    /// flips a Caps/Num/Scroll Lock LED bit (see `Keyboard::input`'s own
    /// internals) — keeps every physical keyboard's real LED in sync.
    /// `self.keyboards` is always empty under winit (nothing to update, a
    /// nested window has no hardware LEDs), so this is a no-op there.
    /// Mirrors `anvil`'s own `update_led_state` pattern exactly.
    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        for keyboard in &mut self.keyboards {
            keyboard.led_update(led_state.into());
        }
    }
}

/// `SelectionUserData` is `Arc<String>` (rather than `()`) so a
/// compositor-set selection — the terminal's own text selection, set via
/// `set_data_device_selection`/`set_primary_selection` in `input.rs` — can
/// carry the actual selected text through to [`Self::send_selection`]
/// below, which is what actually hands it to a reading client. One type
/// shared by both the clipboard and primary targets: they're the same kind
/// of compositor-owned offer, just advertised on a different protocol.
impl SelectionHandler for State {
    type SelectionUserData = Arc<String>;

    /// Fires for *any* reading client regardless of which protocol it used
    /// (`wl_data_device`, `zwp_primary_selection_v1`, or `ext_data_control`)
    /// — mudhuts only ever offers plain-text mime types (see the callers of
    /// `set_data_device_selection`/`set_primary_selection`), so the
    /// requested `mime_type` doesn't need inspecting here.
    ///
    /// Writing `fd` is spawned onto its own thread rather than done inline:
    /// this runs synchronously inside the main event-dispatch path, and a
    /// slow/stalled reader plus a selection bigger than a pipe's `PIPE_BUF`
    /// (64 KiB) could otherwise block the whole compositor on the write.
    /// Smithay has no async write helper for this, and a short-lived thread
    /// is the simplest way to avoid that without one.
    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        let text = Arc::clone(user_data);
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(fd);
            if let Err(err) = file.write_all(text.as_bytes()) {
                tracing::warn!("failed to write selection contents to reading client: {err}");
            }
        });
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

impl PointerConstraintsHandler for State {}

impl DndGrabHandler for State {}
impl WaylandDndGrabHandler for State {}

impl OutputHandler for State {}

/// mudhuts has no tablet input of its own, but `cursor-shape-v1`'s internal
/// `Dispatch2<CursorShapeDevice, D>` impl (`smithay::wayland::cursor_shape`)
/// requires `D: TabletSeatHandler` unconditionally — a `wp_cursor_shape_device_v1`
/// can be created from a `zwp_tablet_tool_v2` just as easily as from a
/// `wl_pointer`, so Smithay bounds the whole dispatch impl on it even for
/// compositors, like this one, that never advertise the tablet protocol at
/// all. `WlSurface` already implements the required `TabletToolTarget` via
/// Smithay's own blanket impl, so the default (no-op) trait methods are all
/// that's needed here.
impl TabletSeatHandler for State {
    type ToolFocus = WlSurface;
}

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

/// `keyboard-shortcuts-inhibit-unstable-v1` — see `state.rs`'s
/// `keyboard_shortcuts_inhibit_state` doc comment. Auto-grants every
/// request unconditionally (mirrors `.smithay-ref/anvil`'s own reference
/// handler, whose comment reads "Just grant the wish for everyone") —
/// simpler than building a confirmation UX, and there's no wire-level
/// "deny" in the protocol anyway (a client that's never `activate()`'d
/// just never sees an `active()` event).
impl KeyboardShortcutsInhibitHandler for State {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}

smithay::delegate_dispatch2!(State);
