//! `ext-session-lock-v1` — lets a trusted screen-locker client (e.g. a
//! `swaylock`-alike) blank the whole output and take over every input
//! event until it explicitly unlocks, without needing to be an ordinary
//! toplevel a user could Alt-Tab away from or a window manager could
//! otherwise let slip out of view. Real security surface for daily-driver
//! use on real hardware (`mudhuts --tty`) — before this, mudhuts had no
//! way to actually lock the screen at all.
//!
//! Shaped like an exclusive/modal surface in the same spirit as
//! `handlers/layer_shell.rs`'s `exclusive` layer, but stronger: a lock
//! sits *above* even an exclusive layer-shell surface (see `input.rs`'s
//! `process_input_event`, which checks `state.locked` before anything
//! else at all, including that layer's own check) and blanks the *entire*
//! render pipeline (see `render.rs`'s `build_frame_elements`), not just
//! keyboard focus.
//!
//! ## Lifecycle
//! 1. A client calls `ext_session_lock_manager_v1.lock` → Smithay calls
//!    [`lock`](SessionLockHandler::lock) with a [`SessionLocker`] "confirmation"
//!    handle. Multiple clients can race to do this "simultaneously"; only
//!    the first is accepted here — every later one has its `SessionLocker`
//!    dropped immediately, which Smithay's own `Drop` impl turns into the
//!    client-visible `finished()` rejection automatically.
//! 2. Accepting stashes the confirmation in `State::pending_lock` rather
//!    than confirming it right away: the protocol requires that `locked()`
//!    not reach the client until the compositor has actually presented a
//!    genuinely blank/locked frame, which can only be known once a
//!    backend's render call site (`udev_backend.rs`'s `render_surface`,
//!    `winit_backend.rs`'s `WinitEvent::Redraw` handler) has actually
//!    submitted one — see those call sites for where `pending_lock` is
//!    finally taken and confirmed.
//! 3. The client calls `get_lock_surface` for whatever output(s) it knows
//!    about (just the one, here) → [`new_surface`](SessionLockHandler::new_surface)
//!    fires, and the resulting [`LockSurface`] is stored so `render.rs`
//!    can composite it once it has content.
//! 4. `unlock_and_destroy` from the client that actually holds the lock →
//!    Smithay validates that it's the right instance (a stale/second
//!    client's attempt is rejected at the protocol level before this even
//!    gets called) and calls [`unlock`](SessionLockHandler::unlock).
//! 5. If the locking client dies *without* unlocking, [`unlock`] is
//!    intentionally never called — per the protocol's own semantics, the
//!    session simply stays locked forever (until, if this ever supports
//!    it, some other trusted mechanism intervenes). Not a bug to route
//!    around; the alternative (auto-unlocking on client death) would
//!    silently defeat the entire point of a screen lock.

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::utils::Size;
use smithay::wayland::session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker};

use crate::State;

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        if self.locked {
            // Already locked by some other (or the same) client — reject
            // this racing request by just letting `confirmation` drop at
            // the end of this arm; Smithay's `SessionLocker::Drop` impl
            // sends the client `finished()` for us. No explicit "deny"
            // call exists or is needed.
            tracing::info!("rejecting a session-lock request while already locked");
            return;
        }
        self.accepted_lock = Some(confirmation.ext_session_lock().clone());
        self.locked = true;
        // A stale lock surface from some previous lock session shouldn't
        // still be showing (there shouldn't be one, since `unlock` always
        // clears this too, but staying defensive rather than assuming).
        self.lock_surface = None;
        self.pending_lock = Some(confirmation);
        self.request_redraw();
    }

    fn unlock(&mut self) {
        self.locked = false;
        self.lock_surface = None;
        self.accepted_lock = None;
        // Shouldn't still be `Some` by the time a legitimate `unlock`
        // fires (it's only ever held between `lock` and the next
        // successfully-presented frame), but clearing it defensively
        // rather than assuming keeps a dropped-but-unconfirmed locker
        // from ever calling `.lock()` after the session already moved on.
        self.pending_lock = None;
        self.request_redraw();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(accepted) = &self.accepted_lock else {
            tracing::debug!("new lock surface but no lock is currently accepted, dropping it");
            return;
        };
        if surface.ext_session_lock() != accepted {
            // A racing second (already-rejected) client's lock surface —
            // its `get_lock_surface` request could still have been queued
            // before it received the `finished()` this compositor already
            // sent for its lock instance in `lock()` above. Ignoring it
            // rather than trusting it as if it were the real lock keeps a
            // rejected client from ever being able to show anything.
            tracing::debug!("new lock surface from a non-accepted lock instance, ignoring");
            return;
        }
        let Some(out) = Output::from_resource(&output) else {
            tracing::warn!("new lock surface but its output no longer resolves, dropping it");
            return;
        };
        if let Some(mode) = out.current_mode() {
            // Logical, not the mode's raw physical pixel size — this is a
            // real client-facing configure, same reasoning as
            // `handlers/xdg_shell.rs`'s `new_toplevel`/`State::usable_area_logical`.
            let logical: smithay::utils::Size<i32, smithay::utils::Logical> = mode
                .size
                .to_f64()
                .to_logical(out.current_scale().fractional_scale())
                .to_i32_round();
            let size = (logical.w.max(0) as u32, logical.h.max(0) as u32);
            surface.with_pending_state(|state| {
                state.size = Some(Size::from(size));
            });
        }
        // Harmless if Smithay's own post-`new_surface` call to this same
        // method (see `lock.rs`'s `Request::GetLockSurface` handling)
        // finds nothing left pending — this one is what actually carries
        // the size just set above out to the client, since that automatic
        // call happens right after this function returns.
        surface.send_configure();
        self.lock_surface = Some(surface);
        self.request_redraw();
    }
}
