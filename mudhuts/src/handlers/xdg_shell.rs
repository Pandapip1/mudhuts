use smithay::desktop::{
    PopupKind, PopupManager, Window, find_popup_root_surface, get_popup_toplevel_coords,
};
use smithay::input::Seat;
use smithay::input::pointer::{Focus, GrabStartData as PointerGrabStartData};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::{wl_seat, wl_surface::WlSurface};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Serial, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::dialog::{ToplevelDialogHint, XdgDialogHandler};
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
        // Floating Window/Alert system (Phase 5) — and since only ever one
        // thing is shown at a time in the meantime, toggling this off and
        // on would just cost clients unnecessary redraws for a distinction
        // that isn't meaningful yet. Never touched again after this.
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        // Default ConsoleHut assignment needs no protocol: walk the connecting
        // client's process ancestry back to a known ConsoleHut's shell PID (see
        // the plan's Phase 4 notes). Falls back to the focused ConsoleHut if the
        // client's credentials aren't available or no ancestor matches
        // (e.g. it wasn't actually launched from one of our shells).
        //
        // Resolved before the initial-configure sizing below (not after)
        // so that sizing can use this window's own real owning output —
        // possibly a backgrounded one with a different size/scale than
        // the focused output — instead of baking in the wrong size on
        // the client's very first frame.
        let owning_hut_id = surface
            .wl_surface()
            .client()
            .and_then(|client| client.get_credentials(&self.display_handle).ok())
            .and_then(|creds| ownership::find_owning_hut(creds.pid as u32, &self.stack))
            .unwrap_or_else(|| self.stack.focused().id);

        if let Some(output_index) = self.stack.output_index_for_hut(owning_hut_id) {
            // Sized to the *usable* area, not the raw output geometry —
            // shrunk by any layer-shell surface's exclusive zone (a
            // status bar, say) — see `State::usable_area`'s doc comment.
            // Logical, not physical: this is a real `xdg_toplevel`
            // configure, which Wayland always expresses in logical
            // coordinates — see `State::usable_area_logical`'s doc
            // comment. Per this window's own owning output, not
            // `self.usable_area_logical()` (the focused one) — see this
            // block's own reordering note above.
            let (_, _, usable_w, usable_h) = self.usable_area_logical_for(output_index);
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.size = Some(smithay::utils::Size::from((usable_w, usable_h)));
            });
        }

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

        // Nothing else was showing yet for this ConsoleHut specifically, so the
        // newly launched window becomes visible immediately rather than
        // staying hidden behind the terminal until a manual Ctrl+` — but
        // only when it's the focused ConsoleHut; a window launched from a
        // background ConsoleHut's shell just joins that ConsoleHut's own tab strip
        // without stealing focus (generalizes the original Phase 2.5
        // "was empty" rule to be per-ConsoleHut).
        let should_show_now = is_focused_hut && was_empty;
        if should_show_now {
            *self.stack.focused_mut().showing_terminal = false;
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

    /// Real interactive move, for floating Floating Windows/Alerts only — bare
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
        let is_floating_window = hut.floating_window_mut(&wl_surface).is_some();
        let is_alert = !is_floating_window && hut.alert_mut(&wl_surface).is_some();
        if !is_floating_window && !is_alert {
            return;
        }
        // The *currently focused* Hut really does own this window right
        // now — a client only ever issues `xdg_toplevel.move` in
        // response to real user interaction with its own (necessarily
        // visible/focused) title bar. Captured once, here, and carried
        // in the grab: real multi-monitor's focus-follows-mouse means
        // `self.stack.focused()` can point at a completely different
        // output's Hut by the time `motion`/`unset` run later, if the
        // pointer crosses onto another monitor mid-drag.
        let hut_id = self.stack.focused().id;
        let output_index = self.stack.focused_output_index();

        let Some(window) = self.find_window_by_surface(&wl_surface) else {
            return;
        };
        let Some(initial_window_location) = self
            .stack
            .focused()
            .space
            .element_location(&crate::space_element::HutSpaceElement::Window(window.clone()))
        else {
            return;
        };

        let grab = MoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
            surface: wl_surface,
            floating_window: is_floating_window,
            hut_id,
            output_index,
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

/// `xdg-wm-dialog-v1` — a toolkit calling `get_xdg_dialog`/`set_modal` only
/// ever does so *before* a toplevel's first commit (see `handle_commit`'s
/// doc comment for why that's the only point the fullscreen hint can be
/// safely revised), so by the time this fires the initial configure
/// hasn't gone out yet in the common case and there's nothing to correct
/// here — `handle_commit` reads the same `dialog_hint` field fresh, right
/// before that first `send_configure()`, so it always sees this change.
/// Left a no-op rather than trying to react here too: doing so would just
/// duplicate that check for a case (a *second*, corrective configure
/// after the first already went out fullscreen) that no known toolkit
/// actually triggers, since they set the hint before ever committing.
impl XdgDialogHandler for State {
    fn dialog_hint_changed(&mut self, _toplevel: ToplevelSurface, _hint: ToplevelDialogHint) {}
}

/// Should be called on `WlSurface::commit`.
///
/// This is the right (and only reliable) place to decide whether a
/// toplevel is a dialog, not `new_toplevel`: `new_toplevel` fires
/// synchronously inside the client's `xdg_surface.get_toplevel` request,
/// before a well-behaved client has had a chance to send
/// `xdg_toplevel.set_parent` or `xdg_wm_dialog_v1.get_xdg_dialog`/
/// `set_modal` — those all arrive later in the same initial request
/// batch, but strictly before the client's first `wl_surface.commit`.
/// Right before the initial configure goes out is the first point both
/// signals are reliably populated.
pub fn handle_commit(popups: &mut PopupManager, window: Option<Window>, surface: &WlSurface) {
    if let Some(window) = window {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let (initial_configure_sent, dialog_hint) = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .map(|data| match data.lock() {
                    Ok(guard) => (guard.initial_configure_sent, guard.dialog_hint),
                    Err(_) => (true, ToplevelDialogHint::Unknown),
                })
                .unwrap_or((true, ToplevelDialogHint::Unknown))
        });

        if !initial_configure_sent {
            // A dialog (whether flagged via the newer xdg-dialog-v1, or
            // just via the older, coarser `set_parent` a dialog-aware
            // toolkit sets regardless) shouldn't come up fullscreen like
            // a normal Main Window — `new_toplevel` already staged that
            // hint for every toplevel unconditionally (it can't tell the
            // difference yet at that point — see this function's doc
            // comment), so undo it here now that we can actually tell.
            // Checked in addition to, not instead of, `dialog_hint`:
            // plenty of dialog-ish toolkits set a parent without ever
            // touching xdg-dialog-v1.
            if toplevel.parent().is_some() || dialog_hint != ToplevelDialogHint::Unknown {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                    state.size = None;
                });
            }
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
/// Windows are fullscreen — Floating Windows/Alerts float at whatever size
/// their own CSD/content wants, so they're left alone here.
pub(crate) fn resize_all_main_windows(stack: &crate::graph_stack::GraphStack, size: Size<i32, Logical>) {
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
        // Looked up across every ConsoleHut (not just the focused one's own
        // `space`, which only ever holds whichever single Main Window is
        // currently visible) — a popup's parent window doesn't have to be
        // the visible one. `output_index_for_window_surface` returning
        // `None` already covers "no Hut owns a window matching this
        // surface" (the same condition `find_window_by_surface`'s own
        // presence check would test) — one scan instead of two.
        //
        // Also this root window's own output, not `self.real_output_geometry()`/
        // `self.output_scale()` (the focused output) — a popup's parent
        // window doesn't have to be on the focused monitor, and a
        // backgrounded monitor can have a different size/scale entirely.
        let Some(output_index) = self.output_index_for_window_surface(&root) else {
            return;
        };
        let Some(output_geo) = self.real_output_geometry_for(output_index) else {
            return;
        };

        // `State::leaf_absolute_rect` (composable Hut hierarchy RFC's Open
        // Question 3 resolution) gives this root's actual on-screen rect
        // if it's a Main Window reachable through the Hut tree — narrower
        // than the whole output once a Tile-Hut pane can show a Main
        // Window (still out of v1 scope today, so this is currently
        // always the same as `output_geo`, but stops being a no-op the
        // moment that lands). Falls back to the full output rect for a
        // Floating Window/Alert root (never Tile-Hut-paned) or a Main
        // Window that isn't actually visible right now — both match
        // today's pre-existing behavior exactly.
        //
        // `PositionerState::get_unconstrained_geometry`'s own doc comment
        // requires `target` to be expressed relative to the *root
        // toplevel's own geometry origin*, not absolute output space —
        // confirmed against Smithay's own `anvil`/`smallvil` example
        // compositors, which both additionally subtract the window's own
        // absolute location (`target.loc -= window_geo.loc`) after
        // subtracting `get_popup_toplevel_coords`. mudhuts was missing
        // that step entirely, leaving `target.loc` at (approximately) the
        // window's own absolute on-screen position instead of near
        // `(0, 0)` — which shifted every unconstrained popup by roughly
        // that amount (a right-click context menu opening far from the
        // click, but still clickable/mapped correctly, since mapping
        // doesn't depend on this constraint math at all).
        let mut target = match self.leaf_absolute_rect(&root) {
            Some((_, _, w, h)) => {
                let physical = Rectangle::<i32, Physical>::new(Point::from((0, 0)), Size::from((w, h)));
                physical.to_f64().to_logical(Scale::from(self.output_scale_for(output_index))).to_i32_round()
            }
            None => output_geo,
        };
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
