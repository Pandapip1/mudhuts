//! Hand-written `Dispatch2`/`GlobalDispatch2` implementations for
//! `mudhuts_power_button_manager_v1`/`mudhuts_power_button_v1` (see
//! `mudhuts-protocols/protocol/mudhuts-power-button.xml`) — same pattern
//! `handlers/shell.rs` already established for `mudhuts_shell_v1`: no
//! Smithay-provided `delegate_*!` macro for a custom protocol, so this
//! plugs straight into the generic blanket impl
//! `smithay::delegate_dispatch2!(State)` already invoked once in
//! `handlers/mod.rs`.
//!
//! Trust model: unlike `mudhuts_shell_authority_v1`, there's no
//! authentication here — any client can bind, mirroring how genuinely
//! privileged wlr-* protocols (`wlr-layer-shell`, `wlr-output-management`,
//! ...) are unrestricted at the protocol level too. **Unlike those,
//! though, merely binding this one has a real side effect**: per
//! `State::notify_power_button`'s own doc comment, mudhuts' own logout
//! fallback is suppressed the instant *any* `mudhuts_power_button_v1`
//! exists, whether or not that client actually does anything with the
//! events it receives — an inert client that binds and never acts is
//! functionally identical, from mudhuts' side, to a real power-menu UI
//! that's still deciding. Combined with `[power] inhibit-power-key`
//! defaulting on (`power_config.rs`), a client that binds this and does
//! nothing else leaves the physical power button doing *nothing at all*
//! for as long as it stays bound (caught in review — an earlier version
//! of this doc comment claimed the worst case was merely observing
//! events, which is wrong). Accepted rather than fixed with real
//! authentication: this is a *custom, mudhuts-specific* protocol name no
//! generic app would discover or bind by accident, so the realistic risk
//! is a client deliberately targeting mudhuts, not an ordinary one
//! stumbling into this; `[power] inhibit-power-key = false` remains a
//! real, independent backstop (logind's own immediate-poweroff default)
//! against exactly this scenario regardless of what any Wayland client
//! does.

use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, New};
use smithay::wayland::{Dispatch2, GlobalData, GlobalDispatch2};

use mudhuts_protocols::server::mudhuts_power_button_manager_v1::{self, MudhutsPowerButtonManagerV1};
use mudhuts_protocols::server::mudhuts_power_button_v1::MudhutsPowerButtonV1;

use crate::State;

impl GlobalDispatch2<MudhutsPowerButtonManagerV1, State> for GlobalData {
    fn bind(
        &self,
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<MudhutsPowerButtonManagerV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl Dispatch2<MudhutsPowerButtonManagerV1, State> for GlobalData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        _resource: &MudhutsPowerButtonManagerV1,
        request: mudhuts_power_button_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            mudhuts_power_button_manager_v1::Request::GetPowerButton { id } => {
                let object = data_init.init(id, GlobalData);
                state.power_button_objects.push(object);
            }
            mudhuts_power_button_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch2<MudhutsPowerButtonV1, State> for GlobalData {
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        _resource: &MudhutsPowerButtonV1,
        _request: mudhuts_protocols::server::mudhuts_power_button_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        // The only request is `destroy`, handled entirely by `destroyed`
        // below (a destructor request needs no explicit handling of its
        // own — `wayland-server` destroys the resource for us once this
        // returns).
    }

    fn destroyed(&self, state: &mut State, _client: ClientId, resource: &MudhutsPowerButtonV1) {
        state.power_button_objects.retain(|o| o != resource);
        if state.power_button_objects.is_empty() {
            // Otherwise this can go stale across exactly this kind of
            // client churn: client A binds, a press is reported to it
            // (the flag goes `true`), A disconnects mid-hold (this
            // branch, without the reset), then client B binds before
            // the button is released — `notify_power_button` would see
            // a non-empty `power_button_objects` (B) and the still-
            // `true` flag from A's own press, and send B a `Released`
            // it never got a matching `Pressed` for, the exact
            // inconsistency this flag exists to prevent (caught in
            // review). Resetting here, the moment the set of bound
            // clients actually empties, closes that regardless of
            // whether `notify_power_button` happens to run again before
            // some new client binds.
            state.power_button_pressed_reported = false;
        }
    }
}
