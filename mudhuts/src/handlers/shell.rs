//! Hand-written `Dispatch2`/`GlobalDispatch2` implementations for
//! `mudhuts_shell_v1` (see `mudhuts-protocols/protocol/mudhuts-shell.xml`
//! and the plan's Phase 5 notes) — there's no Smithay-provided
//! `delegate_*!` macro for a custom protocol, so this plugs straight into
//! the generic blanket impl `smithay::delegate_dispatch2!(State)` already
//! invoked once in `handlers/mod.rs`.
//!
//! Phase 5b's `mudhuts_shell_authority_v1` lives here too: a *privileged*
//! sibling of `mudhuts_window_role_v1` for a trusted helper program to tag
//! *other* clients' toplevels (ones that don't speak `mudhuts_shell_v1`
//! natively) by rule — app_id/title matching, entirely the helper's own
//! business, not this protocol's. Trust model: any client can bind
//! `get_authority`, but every request on the resulting object except
//! `authenticate` is refused until it presents the one-time secret
//! `State::authority_token` mudhuts generated at startup and handed only
//! to the one helper process it itself spawned (`main.rs`'s
//! `--authority-helper`, via the `MUDHUTS_AUTHORITY_TOKEN` env var of
//! that one child) — an unrelated client binding the interface just gets
//! every request refused, since it can't know the token. Toplevels are
//! named by `ext_foreign_toplevel_list_v1` identifier strings rather than
//! direct object references, since the helper doesn't own them the way a
//! natively-speaking client owns its own toplevel.

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, New, Resource};
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::{Dispatch2, GlobalData, GlobalDispatch2};

use mudhuts_protocols::server::mudhuts_shell_authority_v1::{self, Error as AuthorityError, MudhutsShellAuthorityV1};
use mudhuts_protocols::server::mudhuts_shell_v1::{self, MudhutsShellV1};
use mudhuts_protocols::server::mudhuts_window_role_v1::{self, MudhutsWindowRoleV1};

use crate::State;
use crate::main_window::{Alert, FloatingWindow};

impl GlobalDispatch2<MudhutsShellV1, State> for GlobalData {
    fn bind(
        &self,
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<MudhutsShellV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl Dispatch2<MudhutsShellV1, State> for GlobalData {
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        _resource: &MudhutsShellV1,
        request: mudhuts_shell_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            mudhuts_shell_v1::Request::GetWindowRole { id, toplevel } => {
                data_init.init(id, WindowRoleUserData { toplevel });
            }
            mudhuts_shell_v1::Request::GetAuthority { id } => {
                data_init.init(id, AuthorityUserData::default());
            }
            _ => {}
        }
    }
}

/// Whether a `mudhuts_shell_authority_v1` object has successfully
/// authenticated yet — see this module's doc for the trust model.
#[derive(Default)]
struct AuthorityUserData {
    authenticated: AtomicBool,
}

impl Dispatch2<MudhutsShellAuthorityV1, State> for AuthorityUserData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        resource: &MudhutsShellAuthorityV1,
        request: mudhuts_shell_authority_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        if let mudhuts_shell_authority_v1::Request::Authenticate { token } = request {
            if token == state.authority_token {
                self.authenticated.store(true, Ordering::Relaxed);
            } else {
                resource.post_error(AuthorityError::BadToken, "wrong token".to_string());
            }
            return;
        }

        if !self.authenticated.load(Ordering::Relaxed) {
            resource.post_error(
                AuthorityError::NotAuthenticated,
                "must authenticate before any other request".to_string(),
            );
            return;
        }

        match request {
            mudhuts_shell_authority_v1::Request::Authenticate { .. } => unreachable!("handled above"),
            mudhuts_shell_authority_v1::Request::SetMain { identifier } => {
                let Some(surface) = resolve_by_identifier(state, &identifier) else {
                    resource.post_error(AuthorityError::UnknownToplevel, format!("no Main Window with identifier {identifier:?}"));
                    return;
                };
                retag(state, &surface, None);
            }
            mudhuts_shell_authority_v1::Request::SetFloating { identifier, main_identifier } => {
                let (Some(surface), Some(main_surface)) = (
                    resolve_by_identifier(state, &identifier),
                    resolve_by_identifier(state, &main_identifier),
                ) else {
                    resource.post_error(AuthorityError::UnknownToplevel, "identifier or main_identifier not found".to_string());
                    return;
                };
                retag(state, &surface, Some((Role::Floating, main_surface)));
            }
            mudhuts_shell_authority_v1::Request::SetAlert { identifier, main_identifier } => {
                let (Some(surface), Some(main_surface)) = (
                    resolve_by_identifier(state, &identifier),
                    resolve_by_identifier(state, &main_identifier),
                ) else {
                    resource.post_error(AuthorityError::UnknownToplevel, "identifier or main_identifier not found".to_string());
                    return;
                };
                retag(state, &surface, Some((Role::Alert, main_surface)));
            }
            mudhuts_shell_authority_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

/// Find a current Main Window's surface by its
/// `ext_foreign_toplevel_list_v1` identifier string — see this module's
/// doc on why the authority path names toplevels this way rather than by
/// direct object reference.
fn resolve_by_identifier(state: &State, identifier: &str) -> Option<WlSurface> {
    state.stack.all_huts().find_map(|hut| {
        hut.main_windows().iter().find_map(|entry| {
            if entry.foreign_handle.identifier() == identifier {
                entry.window.toplevel().map(|t| t.wl_surface().clone())
            } else {
                None
            }
        })
    })
}

/// Which `xdg_toplevel` a `mudhuts_window_role_v1` object was created for
/// — the toplevel that's being (re-)tagged as a bare Main Window, a
/// Floating Window, or an Alert.
pub struct WindowRoleUserData {
    toplevel: xdg_toplevel::XdgToplevel,
}

impl Dispatch2<MudhutsWindowRoleV1, State> for WindowRoleUserData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        _resource: &MudhutsWindowRoleV1,
        request: mudhuts_window_role_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        let Some(tagged) = state.xdg_shell_state.get_toplevel(&self.toplevel) else {
            tracing::warn!("mudhuts_window_role_v1 request for an already-dead toplevel");
            return;
        };
        let tagged_surface = tagged.wl_surface().clone();

        match request {
            mudhuts_window_role_v1::Request::SetMain => {
                retag(state, &tagged_surface, None);
            }
            mudhuts_window_role_v1::Request::SetFloating { main } => {
                let Some(main_surface) = resolve_surface(state, &main) else {
                    tracing::warn!("set_floating referencing an already-dead main toplevel");
                    return;
                };
                retag(state, &tagged_surface, Some((Role::Floating, main_surface)));
            }
            mudhuts_window_role_v1::Request::SetAlert { main } => {
                let Some(main_surface) = resolve_surface(state, &main) else {
                    tracing::warn!("set_alert referencing an already-dead main toplevel");
                    return;
                };
                retag(state, &tagged_surface, Some((Role::Alert, main_surface)));
            }
            mudhuts_window_role_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

fn resolve_surface(state: &State, toplevel: &xdg_toplevel::XdgToplevel) -> Option<WlSurface> {
    state
        .xdg_shell_state
        .get_toplevel(toplevel)
        .map(|t| t.wl_surface().clone())
}

enum Role {
    Floating,
    Alert,
}

/// Move `tagged_surface`'s window to its new role, wherever it currently
/// lives (a bare Main Window, or already a Floating Window/Alert of some other
/// Main Window) — always within the same ConsoleHut it was originally assigned
/// to (Floating Windows/Alerts belong to a Main Window in *their own* ConsoleHut; a
/// `main` toplevel from a different ConsoleHut just means the target isn't
/// found, handled below by leaving it as a bare Main Window instead of
/// silently moving it across Huts).
fn retag(state: &mut State, tagged_surface: &WlSurface, target: Option<(Role, WlSurface)>) {
    // Mutation happens entirely within this loop (over `state.stack`'s own
    // borrow); `sync_visible_main_window`/`request_redraw` need `&mut
    // State` themselves, so they're called after the loop's borrow has
    // ended rather than from inside it.
    let mut handled_hut_id = None;
    // Whether this retag actually produced a Floating Window/Alert (as
    // opposed to a bare Main Window, or the "target not found" fallback
    // that also treats it as a bare Main Window) — see this function's
    // own resync below for why this matters: neither
    // `sync_hut_space`/`sync_keyboard_focus_to_view` ever focuses
    // anything but a Hut's *Main Window*, so without an explicit
    // `keyboard.set_focus` call here, a newly-tagged Alert/Floating
    // Window becomes visible on screen but silently never receives
    // keyboard input — caught in review of an unrelated change to
    // `sync_keyboard_focus_to_view` (see its own doc comment), but a
    // real, pre-existing gap in this function specifically, not
    // introduced by that change.
    let mut tagged_floating_or_alert = false;
    for hut in state.stack.all_huts_mut() {
        // Captured *before* removal — `take_bare_main_window` shifts/
        // clamps `active_main_window` the moment it removes anything
        // (`shift_active_index_on_removal`), which for a 2+-tab Hut can
        // leave it pointing at a *different* surviving tab, not "none" —
        // so checking "was this the active tab" only makes sense against
        // the *pre*-removal state. `false` for a nested Floating
        // Window/Alert being promoted (its owning Main Window entry is a
        // different `Window` than `tagged_surface` itself, so this never
        // matches there) — correct, since promoting one of those is
        // always a genuinely new tab, not a re-insertion of one that was
        // already active.
        let was_active = hut.active_main_window_entry().is_some_and(|e| e.matches(tagged_surface));
        // Which removal path succeeded matters for `make_active` below,
        // so this can no longer just be `.or_else(...)`'d away into a
        // single `Option` the way it started — a *bare* Main Window
        // being re-tagged (e.g. a redundant `SetMain`, or a `SetFloating`
        // whose target didn't resolve, see the fallback arm below) is a
        // fundamentally different case from a *nested* Floating
        // Window/Alert being promoted to a bare Main Window: only the
        // former should ever be judged against `was_active`/"Hut already
        // has other tabs" at all — the latter (`SetMain` on something
        // that was Floating/Alert) is always a deliberate "bring this to
        // the front" action, and always making it active is the original,
        // correct behavior there (caught in review: an earlier version of
        // this fix applied the bare-Main-Window formula to *both* cases,
        // silently backgrounding a freshly-un-floated/un-alerted window
        // instead of showing it).
        let was_bare = hut.has_bare_main_window(tagged_surface);
        let Some(window) = hut
            .take_bare_main_window(tagged_surface)
            .or_else(|| hut.take_nested_window(tagged_surface))
        else {
            continue;
        };
        // Captured now — `all_huts_mut()` walks every output, not just
        // the focused one, so the Hut this retag actually landed on
        // isn't necessarily the focused one (see this function's own
        // resync below).
        handled_hut_id = Some(hut.id);
        // Whether the retagged window should become/stay this Hut's
        // active tab once re-inserted as a bare Main Window (`None`
        // below, or the "target not found" fallback). For `was_bare`:
        // mirrors `handlers/xdg_shell.rs`'s `new_toplevel` (see
        // `push_main_window`'s own doc comment on the bug this avoids)
        // for "the Hut already has other tabs open", plus `was_active`
        // for "this window *was itself* the active tab being removed and
        // reinserted" — `new_toplevel` never has to handle that (it only
        // ever deals with a genuinely brand-new `Window`, never one
        // round-tripping through remove-then-reinsert within the same Hut
        // it came from); without it, a redundant re-tag of an
        // already-active bare Main Window would silently flip the user's
        // view to whatever sibling tab the active-index clamp happened to
        // land on. For `!was_bare` (a nested Floating Window/Alert being
        // promoted): unconditionally `true` — promoting one to Main is
        // always a deliberate "show this now" action, regardless of
        // whether the Hut has other tabs open.
        let make_active = if was_bare { was_active || hut.main_window_count() == 0 } else { true };

        match &target {
            None => {
                tracing::debug!("mudhuts_window_role_v1: retagged as a bare Main Window");
                let foreign_handle = state
                    .foreign_toplevel_list_state
                    .new_toplevel::<State>(&crate::chrome::window_title(&window), &crate::chrome::window_app_id(&window));
                hut.push_main_window(window, make_active, foreign_handle);
            }
            Some((role, main_surface)) => match hut.find_main_window_mut(main_surface) {
                Some(entry) => {
                    match role {
                        Role::Floating => entry.floating_windows.push(FloatingWindow::new(window)),
                        Role::Alert => entry.alerts.push(Alert::new(window)),
                    }
                    tagged_floating_or_alert = true;
                    tracing::debug!(
                        "mudhuts_window_role_v1: retagged as {}",
                        match role {
                            Role::Floating => "a Floating Window",
                            Role::Alert => "an Alert",
                        }
                    );
                }
                None => {
                    tracing::warn!(
                        "mudhuts_window_role_v1: target main window not found in the same ConsoleHut, leaving as a bare Main Window"
                    );
                    let foreign_handle = state.foreign_toplevel_list_state.new_toplevel::<State>(
                        &crate::chrome::window_title(&window),
                        &crate::chrome::window_app_id(&window),
                    );
                    hut.push_main_window(window, make_active, foreign_handle);
                }
            },
        }
        break;
    }

    if let Some(hut_id) = handled_hut_id {
        // This Hut's own space, not `state.sync_visible_main_window()`
        // (which only ever rebuilds the *focused* Hut's `space` — same
        // fix/reasoning as `grabs.rs`'s `unset`/`docks.rs`'s
        // `finish_drag`): the retagged window's Hut isn't necessarily
        // the focused one, since the loop above searches every output.
        state.sync_hut_space(hut_id);
        // Explicit keyboard focus for a freshly-tagged Alert (in
        // practice — see below for why a fresh Floating Window never
        // actually reaches this) — `sync_hut_space` (called just above)
        // only ever resolves keyboard focus to a Hut's *Main Window* (or
        // `None`), never a Floating Window/Alert, so without this the new
        // dialog would render on screen and simply never receive a
        // keystroke.
        //
        // A fresh Floating Window (`Role::Floating`, above) never
        // actually passes the `window_in_space` check below, only an
        // Alert does — not a bug, just this gate correctly doing its job:
        // `FloatingWindow::new` starts every one `Dock::Docked` (see its
        // own doc comment), and `sync_main_window_space` deliberately
        // never maps a *docked* Floating Window into `space` at all — a
        // docked one isn't a real composited surface yet (`docks.rs`
        // draws a small handle instead), so there's genuinely nothing to
        // focus until the user drags it out to float. `tagged_floating_or_alert`'s
        // name still covers both roles correctly (it only gates whether
        // this whole block is even worth checking), just don't expect the
        // `set_focus` call itself to ever actually fire for the
        // `Role::Floating` arm specifically.
        //
        // `hut_id == state.stack.focused().id` alone isn't enough — a
        // Main Window can have several tabs, and `mudhuts_shell_authority_v1`'s
        // target resolution isn't restricted to the *active* one, so the
        // tagged window's owning Hut can be focused while the specific
        // Main Window it just got attached to is a *backgrounded* tab (or
        // the Hut is currently showing its terminal instead of any Main
        // Window at all) — caught in review. `sync_main_window_space`
        // (just run, via `sync_hut_space` above) only ever maps the
        // *active* entry's own Floating Windows/Alerts into `space`, so
        // checking whether `tagged_surface` actually ended up there is
        // the real "is this genuinely visible right now" answer, not
        // just "is it in the right Hut" — reusing the same
        // `window_in_space` helper `sync_keyboard_focus_to_view` itself
        // uses for the identical question.
        //
        // Known narrow gap, not fixed: `sync_hut_space` (above) skips
        // `sync_main_window_space` while `state.dragging_hut_id ==
        // Some(hut_id)` (a live dock-drag in progress on this exact Hut —
        // see `sync_hut_space`'s own doc comment), so a retag landing on
        // that same Hut mid-drag would find the new window not yet in
        // `space` and skip focusing it here, with nothing re-checking
        // once the drag ends. Requires a real drag and a retag racing on
        // the *same* Hut at the *same* moment; accepted rather than
        // adding cross-module coordination for it, matching this
        // codebase's existing tolerance for similarly narrow staleness
        // windows elsewhere (see `sync_keyboard_focus_to_view`'s own doc
        // comment on `space()`'s identical residual risk).
        if tagged_floating_or_alert
            && hut_id == state.stack.focused().id
            && crate::space_element::window_in_space(state.stack.focused().space(), tagged_surface).is_some()
            && let Some(keyboard) = state.seat.get_keyboard()
        {
            keyboard.set_focus(state, Some(tagged_surface.clone()), SERIAL_COUNTER.next_serial());
        }
        state.request_redraw();
    } else {
        tracing::warn!("mudhuts_window_role_v1: tagged toplevel not found in any ConsoleHut");
    }
}
