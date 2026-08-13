//! Generalized redraw/hit-test capabilities for anything renderable or
//! interactive, whether or not it's a node in the [`crate::hut::Hut`] tree —
//! part of the composable Hut hierarchy redesign (see
//! `docs/rfcs/composable-hut-hierarchy.md`'s cross-cutting section). A Hut
//! node will implement these the same way chrome (dock handles, a tab
//! strip, the switcher popup) does, so redraw/hit-test dispatch never has
//! to special-case "is this a tree node or not."
//!
//! [`RedrawHandle`] replaces the pattern of a mutating method's caller
//! remembering to call `State::request_redraw()` afterward — a real bug
//! class this project has hit before (`Action::ToggleTerminal` once forgot
//! it). An implementor stores the handle it's given and calls
//! [`RedrawHandle::mark_dirty`] itself, from inside whatever code already
//! mutates its state, so the call can't be forgotten by a caller who never
//! sees the flag exists.

use smithay::reexports::calloop::ping::Ping;
use smithay::utils::{Physical, Point};

/// A cheap, cloneable handle onto the same `Ping` [`crate::State::redraw_ping`]
/// already uses — no new redraw primitive, just handed out more widely than
/// only `State` being able to trigger one.
#[derive(Clone)]
pub struct RedrawHandle(Ping);

impl RedrawHandle {
    pub fn new(ping: Ping) -> Self {
        Self(ping)
    }

    /// Schedule a redraw. Safe to call more than once per frame — pinging
    /// an already-pending `Ping` is a no-op (see its own doc comment).
    pub fn mark_dirty(&self) {
        self.0.ping();
    }
}

/// Implemented by anything that holds a [`RedrawHandle`] and calls
/// `mark_dirty()` on its own state changes, rather than exposing "please
/// remember to redraw after calling this" as the caller's responsibility.
pub trait Redrawable {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle);
}

/// What a [`HitTestable`] implementor found under a point. Deliberately
/// minimal for now — scoped to what actually has a real caller today
/// (docks.rs's handle hit-test). The composable Hut hierarchy RFC's later
/// steps will likely grow this once there's a generic dispatch loop
/// consulting every `HitTestable` in z-order (see the RFC's Q3/cross-
/// cutting sections) — not designed further ahead of that real need.
pub enum Hit {
    /// A docked Floating Window's handle, ready to start a drag on.
    DockHandle(smithay::reexports::wayland_server::protocol::wl_surface::WlSurface),
}

/// Anything that can claim a click/hit-test at a point already translated
/// into its own local coordinate space. Independent of [`Redrawable`] —
/// e.g. the switcher popup has no click behavior at all (Alt-Tab-preview-
/// only, keyboard-driven), so it implements `Redrawable` without this.
pub trait HitTestable {
    fn hit_test(&self, point: Point<i32, Physical>) -> Option<Hit>;
}
