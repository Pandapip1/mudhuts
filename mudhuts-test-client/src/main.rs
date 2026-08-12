//! Verification tool for `mudhuts_shell_v1` (see the plan's Phase 5
//! notes) — no real GUI app speaks this protocol yet, so this is how
//! role assignment actually gets exercised: connects, creates two bare
//! `xdg_toplevel`s, binds `mudhuts_shell_v1`, and tags the second as a
//! Sub-Window or Alert of the first (selected via `--sub`/`--alert`).
//! Run from mudhuts' own built-in shell so its PID-ancestry Hut
//! assignment (see `mudhuts/src/ownership.rs`) puts both toplevels in the
//! same Hut — check mudhuts' logs to confirm the role landed.

use std::env;
use std::process::ExitCode;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor::WlCompositor, wl_registry, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};

use mudhuts_protocols::client::mudhuts_shell_v1::MudhutsShellV1;
use mudhuts_protocols::client::mudhuts_window_role_v1::MudhutsWindowRoleV1;

struct AppState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Every global this one-shot test needs is bound right after the
        // initial roundtrip in `main` — dynamic add/remove afterward
        // isn't interesting here.
    }
}

impl Dispatch<XdgWmBase, ()> for AppState {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

/// User data is the toplevel's own `wl_surface` — so the initial
/// `configure` handshake (ack, then a follow-up commit) can be completed
/// without tracking it anywhere else. No buffer is ever attached; this
/// test doesn't render anything, it only exercises the role-assignment
/// protocol.
impl Dispatch<XdgSurface, WlSurface> for AppState {
    fn event(
        _: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        surface: &WlSurface,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            surface.commit();
        }
    }
}

wayland_client::delegate_noop!(AppState: ignore WlCompositor);
wayland_client::delegate_noop!(AppState: ignore WlSurface);
wayland_client::delegate_noop!(AppState: ignore XdgToplevel);
wayland_client::delegate_noop!(AppState: ignore MudhutsShellV1);
wayland_client::delegate_noop!(AppState: ignore MudhutsWindowRoleV1);

/// Create a bare toplevel and drive it through the initial
/// commit/configure/ack/commit handshake (via a roundtrip, handled by
/// the `Dispatch<XdgSurface, _>` impl above) before handing it back.
fn spawn_toplevel(
    compositor: &WlCompositor,
    wm_base: &XdgWmBase,
    qh: &QueueHandle<AppState>,
    event_queue: &mut EventQueue<AppState>,
    state: &mut AppState,
) -> XdgToplevel {
    let surface = compositor.create_surface(qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, qh, surface.clone());
    let toplevel = xdg_surface.get_toplevel(qh, ());
    surface.commit();
    if let Err(err) = event_queue.roundtrip(state) {
        eprintln!("roundtrip while configuring a toplevel failed: {err}");
    }
    toplevel
}

fn main() -> ExitCode {
    let arg = env::args().nth(1).unwrap_or_else(|| "--sub".to_string());
    let as_alert = match arg.as_str() {
        "--sub" => false,
        "--alert" => true,
        other => {
            eprintln!("usage: mudhuts-test-client [--sub | --alert] (got {other:?}, defaulting to --sub)");
            false
        }
    };

    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("failed to connect to the Wayland display: {err}");
            return ExitCode::FAILURE;
        }
    };

    let (globals, mut event_queue) = match registry_queue_init::<AppState>(&conn) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("failed to initialize globals: {err}");
            return ExitCode::FAILURE;
        }
    };
    let qh = event_queue.handle();
    let mut state = AppState;

    let compositor: WlCompositor = match globals.bind(&qh, 1..=6, ()) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("wl_compositor global not available: {err}");
            return ExitCode::FAILURE;
        }
    };
    let wm_base: XdgWmBase = match globals.bind(&qh, 1..=6, ()) {
        Ok(w) => w,
        Err(err) => {
            eprintln!("xdg_wm_base global not available: {err}");
            return ExitCode::FAILURE;
        }
    };
    let shell: MudhutsShellV1 = match globals.bind(&qh, 1..=1, ()) {
        Ok(s) => s,
        Err(err) => {
            eprintln!(
                "mudhuts_shell_v1 global not available ({err}) — is this running under mudhuts?"
            );
            return ExitCode::FAILURE;
        }
    };

    let main_toplevel = spawn_toplevel(&compositor, &wm_base, &qh, &mut event_queue, &mut state);
    let target_toplevel = spawn_toplevel(&compositor, &wm_base, &qh, &mut event_queue, &mut state);

    let role = shell.get_window_role(&target_toplevel, &qh, ());
    if as_alert {
        role.set_alert(&main_toplevel);
        println!("tagged the second toplevel as an Alert of the first");
    } else {
        role.set_sub(&main_toplevel);
        println!("tagged the second toplevel as a Sub-Window of the first");
    }

    if let Err(err) = event_queue.roundtrip(&mut state) {
        eprintln!("final roundtrip failed: {err}");
        return ExitCode::FAILURE;
    }
    println!("done — check mudhuts' logs to confirm the role assignment landed");
    ExitCode::SUCCESS
}
