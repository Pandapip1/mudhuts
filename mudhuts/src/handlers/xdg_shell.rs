use smithay::desktop::{
    PopupKind, PopupManager, Window, find_popup_root_surface, get_popup_toplevel_coords,
};
use smithay::input::Seat;
use smithay::input::pointer::{Focus, GrabStartData as PointerGrabStartData};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::{wl_seat, wl_surface::WlSurface};
use smithay::utils::{Logical, Serial, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};

use crate::State;
use crate::grabs::MoveSurfaceGrab;
use crate::ownership;

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Every mudhuts window is fullscreen — hint that on the initial
        // configure so clients that respect it (most toolkits) draw at the
        // output's size from the start, rather than some arbitrary default
        // the compositor would otherwise have to center/letterbox.
        //
        // Also permanently hint `Activated` (focused). There's no real
        // per-window focus tracking yet — that needs the floating
        // Sub-Window/Alert system (Phase 5) — and since only ever one
        // thing is shown at a time in the meantime, toggling this off and
        // on would just cost clients unnecessary redraws for a distinction
        // that isn't meaningful yet. Never touched again after this.
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        if let Some(output) = self.space.outputs().next()
            && let Some(geo) = self.space.output_geometry(output)
        {
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.size = Some(geo.size);
            });
        }

        // Default Hut assignment needs no protocol: walk the connecting
        // client's process ancestry back to a known Hut's shell PID (see
        // the plan's Phase 4 notes). Falls back to the focused Hut if the
        // client's credentials aren't available or no ancestor matches
        // (e.g. it wasn't actually launched from one of our shells).
        let owning_hut_id = surface
            .wl_surface()
            .client()
            .and_then(|client| client.get_credentials(&self.display_handle).ok())
            .and_then(|creds| ownership::find_owning_hut(creds.pid as u32, &self.stack))
            .unwrap_or_else(|| self.stack.focused().id);

        let window = Window::new_wayland_window(surface);
        let wl_surface = window.toplevel().map(|t| t.wl_surface().clone());

        let is_focused_hut = owning_hut_id == self.stack.focused().id;
        let was_empty = self
            .stack
            .find_mut(owning_hut_id)
            .is_some_and(|hut| hut.main_window_count() == 0);
        // Advertised via `ext_foreign_toplevel_list_v1` from the moment
        // it becomes a Main Window — gives it a stable identifier a
        // trusted helper program can later use to tag it (Phase 5b's
        // `mudhuts_shell_authority_v1` — see `handlers/shell.rs`).
        let foreign_handle = self
            .foreign_toplevel_list_state
            .new_toplevel::<State>(&crate::chrome::window_title(&window), &crate::chrome::window_app_id(&window));
        if let Some(hut) = self.stack.find_mut(owning_hut_id) {
            hut.push_main_window(window, was_empty, foreign_handle);
        }

        // Nothing else was showing yet for this Hut specifically, so the
        // newly launched window becomes visible immediately rather than
        // staying hidden behind the terminal until a manual Ctrl+` — but
        // only when it's the focused Hut; a window launched from a
        // background Hut's shell just joins that Hut's own tab strip
        // without stealing focus (generalizes the original Phase 2.5
        // "was empty" rule to be per-Hut).
        let should_show_now = is_focused_hut && was_empty;
        if should_show_now {
            self.stack.focused_mut().showing_terminal = false;
        }
        self.sync_visible_main_window();

        // A new window should be able to receive keyboard input as soon as
        // it's visible, not only after the user clicks it — matters
        // especially for the should_show_now case, where there was never
        // an existing window to click away from focus in the first place.
        if should_show_now && let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                wl_surface,
                smithay::utils::SERIAL_COUNTER.next_serial(),
            );
        }
        self.request_redraw();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        let was_focused_hut_visible_window = self
            .stack
            .focused()
            .active_window()
            .is_some_and(|w| w.toplevel().is_some_and(|t| t.wl_surface() == wl_surface));
        for hut in self.stack.all_huts_mut() {
            if hut.remove_window(wl_surface) {
                break;
            }
        }
        if was_focused_hut_visible_window {
            self.sync_visible_main_window();
        }
        self.request_redraw();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    /// Real interactive move, for floating Sub-Windows/Alerts only — bare
    /// Main Windows are always fullscreen, so there's nothing to drag for
    /// those (matches the plan's Phase 5 notes: mudhuts assumes CSD, so
    /// this is what a client's own title bar actually calls).
    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let wl_surface = surface.wl_surface().clone();

        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let Some(start_data) = check_grab(&seat, &wl_surface, serial) else {
            return;
        };
        let Some(pointer) = seat.get_pointer() else {
            return;
        };

        let hut = self.stack.focused_mut();
        let is_sub_window = hut.sub_window_mut(&wl_surface).is_some();
        let is_alert = !is_sub_window && hut.alert_mut(&wl_surface).is_some();
        if !is_sub_window && !is_alert {
            return;
        }

        let Some(window) = self.find_window_by_surface(&wl_surface) else {
            return;
        };
        let Some(initial_window_location) = self.space.element_location(&window) else {
            return;
        };

        let grab = MoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
            surface: wl_surface,
            sub_window: is_sub_window,
        };

        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
        // mudhuts windows are always fullscreen; the compositor drives size.
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // TODO popup grabs
    }
}

/// Should be called on `WlSurface::commit`.
pub fn handle_commit(popups: &mut PopupManager, window: Option<Window>, surface: &WlSurface) {
    if let Some(window) = window {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .map(|data| match data.lock() {
                    Ok(guard) => guard.initial_configure_sent,
                    Err(_) => true,
                })
                .unwrap_or(true)
        });

        if !initial_configure_sent {
            toplevel.send_configure();
        }
    }

    popups.commit(surface);
    if let Some(PopupKind::Xdg(xdg)) = popups.find_popup(surface)
        && !xdg.is_initial_configure_sent()
    {
        let _ = xdg.send_configure();
    }
}

/// Called whenever the output resizes (`winit_backend.rs`'s
/// `WinitEvent::Resized`) — `new_toplevel`'s fullscreen size hint above is
/// only ever sent once, at creation, so already-mapped Main Windows need
/// an explicit fresh configure to actually resize; xdg_shell doesn't
/// propagate a compositor-driven size change on its own. Only bare Main
/// Windows are fullscreen — Sub-Windows/Alerts float at whatever size
/// their own CSD/content wants, so they're left alone here.
pub(crate) fn resize_all_main_windows(stack: &crate::stack::HutStack, size: Size<i32, Logical>) {
    for hut in stack.all_huts() {
        for entry in hut.main_windows() {
            let Some(toplevel) = entry.window.toplevel() else {
                continue;
            };
            toplevel.with_pending_state(|state| {
                state.size = Some(size);
            });
            toplevel.send_configure();
        }
    }
}

/// Confirm `seat`'s pointer actually has an active click grab on `surface`
/// for `serial` before letting a `move_request` start a real drag —
/// mirrors `.smithay-ref/smallvil`'s reference `check_grab` (ported
/// panic-free per the project's no-panics rule: the reference uses
/// `.unwrap()` throughout, replaced here with `?`/early-return).
fn check_grab(
    seat: &Seat<State>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<State>> {
    let pointer = seat.get_pointer()?;

    if !pointer.has_grab(serial) {
        return None;
    }

    let start_data = pointer.grab_start_data()?;

    let (focus, _) = start_data.focus.as_ref()?;
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

impl State {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        // Looked up across every Hut (not just `self.space`, which now
        // only ever holds whichever single Main Window is currently
        // visible) — a popup's parent window doesn't have to be the
        // visible one. Every Main Window is fullscreen at the output's
        // origin by construction, mapped or not, so its geometry is
        // always just the output's — no per-window geometry lookup needed.
        if self.find_window_by_surface(&root).is_none() {
            return;
        };

        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
