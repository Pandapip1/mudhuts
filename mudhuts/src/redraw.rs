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

/// The generic version of the pattern `TabbedHut`/`TileHut`'s `set_active`
/// hand-wrote for exactly one field each: wrap any render-relevant field
/// in this instead of writing a bespoke setter, and it's impossible for a
/// future mutation site to forget to request a redraw — the wrapper
/// itself calls [`RedrawHandle::mark_dirty`] on every `&mut` access (via
/// `DerefMut`), unconditionally, the same "safe by default" choice
/// `set_active` already made (no comparing old vs. new — see
/// `RedrawHandle::mark_dirty`'s own doc comment on why over-marking
/// within one frame is free).
///
/// `redraw` starts `None`, the same shape `TabbedHut::redraw`/
/// `TileHut::redraw` already used — reads always work; a write before
/// [`Redrawable::attach_redraw_handle`] runs is silently untracked, which
/// only actually happens during construction, before anything could have
/// rendered from the value anyway.
pub struct Signal<T> {
    value: T,
    redraw: Option<RedrawHandle>,
}

impl<T> Signal<T> {
    pub fn new(value: T) -> Self {
        Self { value, redraw: None }
    }
}

impl<T> Redrawable for Signal<T> {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.redraw = Some(handle);
    }
}

impl<T> std::ops::Deref for Signal<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> std::ops::DerefMut for Signal<T> {
    fn deref_mut(&mut self) -> &mut T {
        if let Some(redraw) = &self.redraw {
            redraw.mark_dirty();
        }
        &mut self.value
    }
}

/// So read-site comparisons (`tab.active == 0`) keep working without an
/// explicit deref. Indexing (`tab.children[tab.active]`) still needs one
/// (`tab.children[*tab.active]`) — that's a `Vec`-specific concern, not
/// something `Signal` itself should carry.
impl<T: PartialEq> PartialEq<T> for Signal<T> {
    fn eq(&self, other: &T) -> bool {
        &self.value == other
    }
}

/// Forwards to `T`'s own `Debug` (e.g. for `assert_eq!` failure messages
/// in tests) — the `redraw` handle itself carries nothing worth printing.
impl<T: std::fmt::Debug> std::fmt::Debug for Signal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.value, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moved here from `hut.rs` (migration step 4's cutover — see
    /// `docs/rfcs/typed-graph-hut.md`) once `TileHut` itself was deleted:
    /// this tests `Signal`'s own core contract directly rather than via
    /// a now-gone wrapper type, since it was never really about
    /// `TileHut` in the first place.
    #[test]
    fn writing_through_a_signal_marks_the_attached_redraw_handle_dirty() {
        let mut value = Signal::new(0);
        let (ping, source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        value.attach_redraw_handle(RedrawHandle::new(ping));

        *value = 1;
        assert_eq!(value, 1);

        let mut event_loop: smithay::reexports::calloop::EventLoop<'static, ()> =
            smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        let fired_clone = fired.clone();
        event_loop
            .handle()
            .insert_source(source, move |_, _, _| fired_clone.set(true))
            .unwrap();
        event_loop
            .dispatch(std::time::Duration::from_millis(0), &mut ())
            .unwrap();
        assert!(fired.get(), "writing through the Signal should have pinged the attached RedrawHandle");
    }
}
