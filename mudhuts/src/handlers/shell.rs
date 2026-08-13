//! Hand-written `Dispatch2`/`GlobalDispatch2` implementations for
//! `mudhuts_shell_v1` (see `mudhuts-protocols/protocol/mudhuts-shell.xml`
//! and the plan's Phase 5 notes) — there's no Smithay-provided
//! `delegate_*!` macro for a custom protocol, so this plugs straight into
//! the generic blanket impl `smithay::delegate_dispatch2!(State)` already
//! invoked once in `handlers/mod.rs`.

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, New};
use smithay::wayland::{Dispatch2, GlobalData, GlobalDispatch2};

use mudhuts_protocols::server::mudhuts_shell_v1::{self, MudhutsShellV1};
use mudhuts_protocols::server::mudhuts_window_role_v1::{self, MudhutsWindowRoleV1};

use crate::State;
use crate::main_window::{Alert, SubWindow};

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
        if let mudhuts_shell_v1::Request::GetWindowRole { id, toplevel } = request {
            data_init.init(id, WindowRoleUserData { toplevel });
        }
    }
}

/// Which `xdg_toplevel` a `mudhuts_window_role_v1` object was created for
/// — the toplevel that's being (re-)tagged as a bare Main Window, a
/// Sub-Window, or an Alert.
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
            mudhuts_window_role_v1::Request::SetSub { main } => {
                let Some(main_surface) = resolve_surface(state, &main) else {
                    tracing::warn!("set_sub referencing an already-dead main toplevel");
                    return;
                };
                retag(state, &tagged_surface, Some((Role::Sub, main_surface)));
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
    Sub,
    Alert,
}

/// Move `tagged_surface`'s window to its new role, wherever it currently
/// lives (a bare Main Window, or already a Sub-Window/Alert of some other
/// Main Window) — always within the same Hut it was originally assigned
/// to (Sub-Windows/Alerts belong to a Main Window in *their own* Hut; a
/// `main` toplevel from a different Hut just means the target isn't
/// found, handled below by leaving it as a bare Main Window instead of
/// silently moving it across Huts).
fn retag(state: &mut State, tagged_surface: &WlSurface, target: Option<(Role, WlSurface)>) {
    // Mutation happens entirely within this loop (over `state.stack`'s own
    // borrow); `sync_visible_main_window`/`request_redraw` need `&mut
    // State` themselves, so they're called after the loop's borrow has
    // ended rather than from inside it.
    let mut handled = false;
    for hut in state.stack.all_huts_mut() {
        let Some(window) = hut
            .take_bare_main_window(tagged_surface)
            .or_else(|| hut.take_nested_window(tagged_surface))
        else {
            continue;
        };

        match &target {
            None => {
                tracing::debug!("mudhuts_window_role_v1: retagged as a bare Main Window");
                hut.push_main_window(window, true);
            }
            Some((role, main_surface)) => match hut.find_main_window_mut(main_surface) {
                Some(entry) => {
                    match role {
                        Role::Sub => entry.sub_windows.push(SubWindow::new(window)),
                        Role::Alert => entry.alerts.push(Alert::new(window)),
                    }
                    tracing::debug!(
                        "mudhuts_window_role_v1: retagged as {}",
                        match role {
                            Role::Sub => "a Sub-Window",
                            Role::Alert => "an Alert",
                        }
                    );
                }
                None => {
                    tracing::warn!(
                        "mudhuts_window_role_v1: target main window not found in the same Hut, leaving as a bare Main Window"
                    );
                    hut.push_main_window(window, true);
                }
            },
        }
        handled = true;
        break;
    }

    if handled {
        state.sync_visible_main_window();
        state.request_redraw();
    } else {
        tracing::warn!("mudhuts_window_role_v1: tagged toplevel not found in any Hut");
    }
}
