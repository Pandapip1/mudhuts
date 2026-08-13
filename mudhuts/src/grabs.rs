//! Interactive move for floating Floating Windows/Alerts, via the real
//! `xdg_toplevel.move` request — mudhuts doesn't negotiate server-side
//! decorations, so a client draws its own CSD title bar and calls this
//! itself on drag. Ported from `.smithay-ref/smallvil/src/grabs/
//! move_grab.rs` (this project's own forking base), panic-free per the
//! project's no-panics rule (the reference impl's `move_request`/
//! `check_grab` use several `.unwrap()`s). Bare Main Windows are always
//! fullscreen and never go through here — see the plan's Phase 5 notes.

use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
    PointerInnerHandle, RelativeMotionEvent,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};

use crate::State;
use crate::main_window::{Dock, Edge};

/// How close (in genuinely Logical pixels — `location`/`size` below
/// always come from `self.space`, which is Logical throughout, so this
/// threshold has to be too, unlike `docks.rs`'s physical-native
/// `DETACH_THRESHOLD`) to an output edge a released Floating Window needs to
/// be to snap back to docked, rather than staying floating where it was
/// dropped.
const REDOCK_THRESHOLD: i32 = 40;

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<State>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    /// The moving window's own surface — used on release to look its
    /// entry back up in the owning ConsoleHut's data model (the actual source
    /// of truth for position/dock state; `space` itself gets rebuilt
    /// from it on every `sync_visible_main_window` call).
    pub surface: WlSurface,
    /// Whether this is a Floating Window (checks edge-proximity to re-dock on
    /// release) vs. an Alert (always ends up floating).
    pub floating_window: bool,
}

impl PointerGrab<State> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);

        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;
        data.space
            .map_element(self.window.clone(), new_location.to_i32_round(), true);
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        // Linux kernel's linux/input-event-codes.h BTN_LEFT.
        const BTN_LEFT: u32 = 0x110;

        if !handle.current_pressed().contains(&BTN_LEFT) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    /// Persist the drop location back into the owning ConsoleHut's own data
    /// model — the real source of truth for position/dock state, which
    /// `State::sync_visible_main_window` rebuilds `space` from on every
    /// focus/visibility change. Without this, the very next such sync
    /// would snap the window right back to wherever it was before the
    /// drag.
    fn unset(&mut self, data: &mut State) {
        let Some(location) = data.space.element_location(&self.window) else {
            return;
        };

        if !self.floating_window {
            if let Some(alert) = data.stack.focused_mut().alert_mut(&self.surface) {
                alert.position = location;
            }
            data.request_redraw();
            return;
        }

        let size = self.window.geometry().size;
        // `location`/`size` are genuinely Logical (from `self.space`) —
        // compared against the output's Logical size, not
        // `data.output_size` (physical), to keep both sides of the
        // distance check in the same space (see
        // `State::output_size_logical`'s doc comment).
        let redock_edge = nearest_edge_within_threshold(data.output_size_logical(), location, size);
        if let Some(sub) = data.stack.focused_mut().floating_window_mut(&self.surface) {
            sub.dock = match redock_edge {
                Some(edge) => Dock::Docked(edge),
                None => Dock::Floating(location),
            };
        }
        data.sync_visible_main_window();
        data.request_redraw();
    }
}

/// The closest of the 4 output edges to a window at `location`/`size`, if
/// within [`REDOCK_THRESHOLD`] — used on release to decide whether a
/// dragged-out Floating Window snaps back to docked.
pub(crate) fn nearest_edge_within_threshold(
    output_size: (i32, i32),
    location: Point<i32, Logical>,
    size: smithay::utils::Size<i32, Logical>,
) -> Option<Edge> {
    let (output_w, output_h) = output_size;
    let left = location.x;
    let right = output_w - (location.x + size.w);
    let top = location.y;
    let bottom = output_h - (location.y + size.h);

    let candidates = [
        (left, Edge::Left),
        (right, Edge::Right),
        (top, Edge::Top),
        (bottom, Edge::Bottom),
    ];
    candidates
        .into_iter()
        .filter(|(distance, _)| *distance <= REDOCK_THRESHOLD)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, edge)| edge)
}
