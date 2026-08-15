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
/// always come from the focused Console Hut's own `space`
/// (`ConsoleHut::space`), which is Logical throughout, so this
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
    /// The `ConsoleHut` that actually owns the window being dragged —
    /// captured once at grab-start time (`handlers/xdg_shell.rs::move_request`),
    /// not re-resolved via `data.stack.focused_mut()` on every callback:
    /// real multi-monitor's focus-follows-mouse can move input focus to
    /// a different output mid-drag (the pointer crossing onto another
    /// monitor while the button is still held), and writing the dragged
    /// window's position into *that* Hut's `space` instead of its real
    /// owner's would silently migrate it into an unrelated Hut's window
    /// set while the actual owner's own data model went stale.
    pub hut_id: u64,
    /// `hut_id`'s output at grab-start time — a fast-path hint for
    /// `motion`'s hot loop (see `GraphStack::find_mut_for`'s doc
    /// comment), not assumed to still be correct on every later
    /// callback: an output unplug/renumber mid-drag can make it stale,
    /// so every use falls back to a full `GraphStack::find_mut` rather
    /// than ever trusting a miss here as "the Hut exited."
    pub output_index: usize,
    /// `self.start_data.location`, rebased to a genuinely global
    /// (virtual-desktop) position at grab-start time — `MotionEvent::location`
    /// (what later `motion()` calls receive) is always *local* to
    /// whichever output currently has focus (`handle_pointer_motion`'s
    /// own doc comment), which can be a *different* output than the one
    /// `start_data.location` was local to if focus-follows-mouse switches
    /// mid-drag. Subtracting two locations expressed in different
    /// outputs' local origins corrupts the delta by roughly the distance
    /// between them; converting both sides to a common global frame
    /// (this field, computed once here; `motion()`'s own `event.location`
    /// rebased fresh every call, since which output it's local to can
    /// itself change mid-drag) avoids that regardless of how many times
    /// focus moves during a single drag.
    pub start_global_location: Point<f64, Logical>,
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

        // `event.location` is local to whichever output *currently* has
        // focus (see `start_global_location`'s own doc comment) — rebase
        // to the same global frame before comparing against it, fresh
        // every call, since a mid-drag focus change can make even *this*
        // rebase target a different output than the previous call's.
        let event_output_position = data
            .stack
            .outputs()
            .get(data.stack.focused_output_index())
            .map(|slot| slot.position)
            .unwrap_or_default();
        let event_global_location = event.location + event_output_position.to_f64();
        let delta = event_global_location - self.start_global_location;
        let new_location = self.initial_window_location.to_f64() + delta;
        // Fast path first (see `output_index`'s doc comment) — falls
        // back to the full graph-wide search only on a miss, so a
        // stale/unplugged-mid-drag output index can never be mistaken
        // for the Hut itself having exited.
        let Some(hut) = data.stack.find_mut_for_hint(self.output_index, self.hut_id) else {
            // The owning Hut exited mid-drag (its shell exited under the
            // dragged window) — nothing left to update.
            return;
        };
        hut.space.map_element(
            crate::space_element::HutSpaceElement::Window(self.window.clone()),
            new_location.to_i32_round(),
            true,
        );
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
        // The dragged window's real owning Hut, not `data.stack.focused()`
        // — see [`MoveSurfaceGrab::hut_id`]'s own doc comment: the
        // pointer may have crossed onto a different output mid-drag.
        let Some(hut) = data.stack.find_mut(self.hut_id) else {
            // The owning Hut exited mid-drag — nothing left to persist.
            data.request_redraw();
            return;
        };
        let Some(location) =
            hut.space.element_location(&crate::space_element::HutSpaceElement::Window(self.window.clone()))
        else {
            return;
        };

        if !self.floating_window {
            if let Some(alert) = hut.alert_mut(&self.surface) {
                alert.position = location;
            }
            data.request_redraw();
            return;
        }

        let size = self.window.geometry().size;
        // `location`/`size` are genuinely Logical (from the owning
        // Console Hut's own `space`) — compared against *that Hut's own
        // output's* Logical size, not `data.output_size_logical()`
        // (which is the *focused* output, possibly a different one by
        // now — see this method's own doc comment), to keep the
        // distance check meaningful for the output the window is
        // actually being dropped on.
        let Some(output_index) = data.stack.output_index_for_hut(self.hut_id) else {
            data.request_redraw();
            return;
        };
        let redock_edge = nearest_edge_within_threshold(data.output_size_logical_for(output_index), location, size);
        // This Hut's own output's usable area, computed before the
        // re-resolve below — needed for `sync_main_window_space` a few
        // lines down, and `data.usable_area_for` needs `&data` as a
        // whole, which the borrow checker won't allow alongside an
        // active `&mut data.stack` borrow.
        let (area_x, area_y, _, _) = data.usable_area_for(output_index);
        // Re-resolved rather than reusing `hut` above — the borrow
        // checker requires it (an immutable borrow of `data.stack` sits
        // between the two), not because the Hut could plausibly have
        // gone away in the gap. Handled gracefully rather than
        // `.expect()`-ing that assumption anyway, matching this
        // function's own no-panics handling everywhere else.
        let Some(hut) = data.stack.find_mut(self.hut_id) else {
            data.request_redraw();
            return;
        };
        if let Some(sub) = hut.floating_window_mut(&self.surface) {
            sub.dock = match redock_edge {
                Some(edge) => Dock::Docked(edge),
                None => Dock::Floating(location),
            };
        }
        // This Hut's own space, not `data.sync_visible_main_window()`
        // (which only ever rebuilds the *focused* Hut's `space` — see
        // its own doc comment): real multi-monitor's focus-follows-mouse
        // can have moved focus to a different output by the time a drag
        // is released (see `hut_id`'s own doc comment), and calling the
        // focused-only sync here left this Hut's `space` never remapped
        // to its new `Dock` state — the window stayed stuck at its old
        // position/visibility until something else happened to trigger
        // a sync for this specific Hut.
        hut.sync_main_window_space((area_x, area_y));
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
    // `.abs()`, not the raw signed distance: a window wide/tall enough to
    // extend past the *opposite* edge makes that opposite edge's distance
    // a large negative number (e.g. flush against the left edge but wider
    // than the output makes `right` very negative) — comparing raw signed
    // values would let that large-magnitude negative "win" over the
    // genuinely near edge's small positive distance, re-docking to the
    // wrong side entirely. Distance-from-touching is symmetric: an edge
    // already overlapped by 5px is just as "near" as one still 5px away.
    candidates
        .into_iter()
        .filter(|(distance, _)| distance.abs() <= REDOCK_THRESHOLD)
        .min_by_key(|(distance, _)| distance.abs())
        .map(|(_, edge)| edge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_only_edge_within_threshold() {
        let edge = nearest_edge_within_threshold((1000, 800), Point::from((10, 300)), smithay::utils::Size::from((200, 200)));
        assert_eq!(edge, Some(Edge::Left));
    }

    #[test]
    fn picks_the_closest_of_several_edges_within_threshold() {
        // Near both the left (x=5) and top (y=15) edges — left should win.
        let edge = nearest_edge_within_threshold((1000, 800), Point::from((5, 15)), smithay::utils::Size::from((200, 200)));
        assert_eq!(edge, Some(Edge::Left));
    }

    #[test]
    fn none_when_nothing_is_within_threshold() {
        let edge = nearest_edge_within_threshold((1000, 800), Point::from((400, 300)), smithay::utils::Size::from((200, 200)));
        assert_eq!(edge, None);
    }

    #[test]
    fn oversized_window_flush_against_one_edge_still_picks_that_edge() {
        // Flush against the left edge, but wide enough to overhang the
        // right edge by a lot — `right`'s raw signed distance is a large
        // negative number that must not out-rank `left`'s exact 0.
        let edge = nearest_edge_within_threshold((1000, 800), Point::from((0, 300)), smithay::utils::Size::from((1400, 200)));
        assert_eq!(edge, Some(Edge::Left));
    }
}
