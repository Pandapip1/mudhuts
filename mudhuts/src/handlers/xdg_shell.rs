use smithay::desktop::{
    PopupKind, PopupManager, Space, Window, find_popup_root_surface, get_popup_toplevel_coords,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::{wl_seat, wl_surface::WlSurface};
use smithay::utils::Serial;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};

use crate::State;

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

        // Phase 1: no Main Window / role organization yet (Phase 4+); just
        // composite it on top of the terminal.
        let was_empty = self.space.elements().next().is_none();
        let window = Window::new_wayland_window(surface);
        let wl_surface = window.toplevel().map(|t| t.wl_surface().clone());
        self.space.map_element(window, (0, 0), false);
        if was_empty {
            // Nothing else to show yet, so a newly launched window should
            // become visible immediately rather than staying hidden behind
            // the terminal until the user manually hits Ctrl+`.
            self.showing_terminal = false;
        }
        // A new window should be able to receive keyboard input as soon as
        // it's visible, not only after the user clicks it — matters
        // especially for the was_empty case above, where there was never
        // an existing window to click away from focus in the first place.
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                wl_surface,
                smithay::utils::SERIAL_COUNTER.next_serial(),
            );
        }
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

    fn move_request(&mut self, _surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // mudhuts windows are always fullscreen; there's nothing to drag.
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
pub fn handle_commit(popups: &mut PopupManager, space: &Space<Window>, surface: &WlSurface) {
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        .cloned()
    {
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

impl State {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        else {
            return;
        };

        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(window) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
