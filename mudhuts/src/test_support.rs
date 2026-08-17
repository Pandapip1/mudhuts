//! `#[cfg(test)]`-only harness for constructing real
//! [`smithay::desktop::Window`]s in unit tests, without a live compositor
//! process. `Window::new_wayland_window` needs a genuine `ToplevelSurface`,
//! which only smithay's own `XdgShellState`/`CompositorState` machinery can
//! produce (their handler traits are the sole path to one — there's no
//! public constructor) — so this drives a real, minimal client/server
//! xdg_shell handshake over an in-process socket pair, the server side
//! implementing just enough of `CompositorHandler`/`XdgShellHandler` to let
//! a toplevel through. The client side's handshake mirrors
//! `mudhuts-test-client`'s own `AppState`/`spawn_toplevel` (see that
//! crate's doc comment) request-for-request against a throwaway
//! in-process server instead of a real `$WAYLAND_DISPLAY` — not literally
//! shared code (`mudhuts-test-client` is a bin-only crate, nothing here
//! depends on it), so if that handshake ever needs a new step, both
//! copies need updating together or this one silently drifts out of sync.
//!
//! This lets tests elsewhere in this crate exercise real Main-Window/
//! Floating-Window/Alert logic (`console_hut.rs`, `main_window.rs`,
//! `handlers/shell.rs::retag`) that operates on `Window`/`WlSurface`
//! identity — previously untestable at all, since nothing in this
//! codebase (or in Smithay itself) had a lighter-weight way to produce one.
//!
//! **Call [`spawn_test_windows`] exactly once per test, sized for
//! everything that test needs** — never split across two separate calls
//! within the same test, even if the two groups of windows are logically
//! unrelated (e.g. "belongs to Hut A" vs. "belongs to Hut B"). Each call
//! stands up its own brand-new, independent in-process Wayland server
//! from scratch, so every client-side object (including every `WlSurface`)
//! gets a fresh, low protocol object id starting from the same small
//! numbers a *different* call's client would also start from — nothing
//! about `WlSurface`'s own identity/equality has any reason to also
//! account for *which Display instance* minted it. Two surfaces from two
//! separate calls can this way come out numerically indistinguishable
//! from `WlSurface`'s own point of view, even though they're genuinely
//! different objects. Found the hard way, not reasoned out in advance:
//! `handlers/shell.rs`'s own `retag_in_stack` tests originally called
//! this twice per test (once for the windows being pushed as initial
//! state, once more via a separate handle-minting helper) and
//! intermittently mismatched surfaces that were never supposed to
//! compare equal — see that module's own test-helper doc comment.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::foreign_toplevel_list::{ForeignToplevelListHandler, ForeignToplevelListState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor::WlCompositor, wl_registry, wl_surface::WlSurface as ClientSurface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};

/// The server side of the in-process handshake — implements only the two
/// handler traits a `ToplevelSurface` requires, nothing else this crate's
/// real `State` also implements (input, rendering, ownership routing...).
/// `toplevels` collects every one that arrives, in creation order, so
/// [`spawn_test_windows`] can hand back exactly the count the caller asked
/// for regardless of how many round trips the client needed.
struct ServerState {
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    foreign_toplevel_list_state: ForeignToplevelListState,
    toplevels: Vec<ToplevelSurface>,
}

impl CompositorHandler for ServerState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<crate::state::ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, _surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {}
}

impl XdgShellHandler for ServerState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
        self.toplevels.push(surface);
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

    fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }
}

impl ForeignToplevelListHandler for ServerState {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

smithay::delegate_dispatch2!(ServerState);

/// Minimal client-side app state — same shape as `mudhuts-test-client`'s
/// `AppState`, trimmed to just what's needed to get past the initial
/// xdg_surface configure handshake (ack + commit) for however many
/// toplevels are requested.
struct ClientAppState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientAppState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for ClientAppState {
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

impl Dispatch<XdgSurface, ClientSurface> for ClientAppState {
    fn event(
        _: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        surface: &ClientSurface,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            surface.commit();
        }
    }
}

wayland_client::delegate_noop!(ClientAppState: ignore WlCompositor);
wayland_client::delegate_noop!(ClientAppState: ignore ClientSurface);
wayland_client::delegate_noop!(ClientAppState: ignore XdgToplevel);

/// Spawn `count` bare `xdg_toplevel`s against a throwaway in-process
/// server and return the resulting real `Window`s plus a real
/// `ForeignToplevelHandle` for each (what `MainWindowEntry::new` needs
/// alongside a `Window` — see `handlers/xdg_shell.rs`'s own `new_toplevel`,
/// the only other place one gets minted), in creation order. Panics on any
/// protocol/setup failure or a 5-second timeout — a test harness failing
/// closed (never silently returning fewer windows than asked for) is much
/// easier to debug than a caller quietly indexing past the end of a
/// too-short `Vec`.
pub(crate) fn spawn_test_windows(
    count: usize,
) -> Vec<(Window, smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle)> {
    let mut display: Display<ServerState> = Display::new().expect("failed to create test Display");
    let dh: DisplayHandle = display.handle();
    let compositor_state = CompositorState::new::<ServerState>(&dh);
    let xdg_shell_state = XdgShellState::new::<ServerState>(&dh);
    let foreign_toplevel_list_state = ForeignToplevelListState::new::<ServerState>(&dh);
    let mut server_state =
        ServerState { compositor_state, xdg_shell_state, foreign_toplevel_list_state, toplevels: Vec::new() };

    let (client_sock, server_sock) = UnixStream::pair().expect("failed to create test socket pair");
    dh.clone()
        .insert_client(server_sock, Arc::new(crate::state::ClientState::default()))
        .expect("failed to insert test client");

    // The client side runs on its own thread: `registry_queue_init`/
    // `roundtrip` block on a reply, which only ever arrives once the
    // server side (driven by this function's own thread, below) has
    // actually processed the corresponding request — the two can't run
    // on the same thread without a real async runtime interleaving them.
    let client_thread = std::thread::spawn(move || {
        let conn = Connection::from_socket(client_sock).expect("failed to wrap test client socket");
        let (globals, mut event_queue) =
            registry_queue_init::<ClientAppState>(&conn).expect("failed to init test client globals");
        let qh = event_queue.handle();
        let mut state = ClientAppState;

        let compositor: WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("test server didn't advertise wl_compositor");
        let wm_base: XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("test server didn't advertise xdg_wm_base");

        for _ in 0..count {
            let surface = compositor.create_surface(&qh, ());
            let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, surface.clone());
            let _toplevel = xdg_surface.get_toplevel(&qh, ());
            surface.commit();
            // Each iteration's own roundtrip is what actually drives this
            // toplevel's ack_configure/commit (sent from the `XdgSurface`
            // dispatch impl above, itself triggered by the `send_configure`
            // `ServerState::new_toplevel` issues) — nothing further is
            // needed per toplevel, and no trailing roundtrip belongs after
            // the loop: this thread must not wait on the server for
            // anything once its own work is done, or it deadlocks against
            // the server-pump loop below, which stops driving the server
            // the instant it sees `count` toplevels (see that loop's own
            // comment — this exact ordering deadlocked the harness once
            // already, hanging both threads in `poll`/`join` forever).
            event_queue.roundtrip(&mut state).expect("test client roundtrip failed");
        }
        // `roundtrip()`'s last internal dispatch can process the
        // `Configure` event and its own `sync` done-callback in the same
        // read (both were likely flushed by the server together) and
        // return without flushing again — which would silently strand the
        // final toplevel's `ack_configure`/`commit` (queued by the
        // `XdgSurface` dispatch impl above, itself triggered by that same
        // `Configure`) unsent when this thread exits and drops `conn`.
        // Every iteration but the last gets away with this by accident
        // (the *next* iteration's `roundtrip()` flushes the leftover
        // bytes as a side effect) — flush explicitly here so the last one
        // doesn't depend on that accident too.
        conn.flush().expect("failed to flush test client's final requests");
    });

    // Keeps driving the server for as long as the client thread is still
    // running, not just until `count` is reached — the client thread's
    // own request queue is the only thing that ever stops on its own; if
    // this loop stopped pumping the moment `count` toplevels arrived
    // while the client still had unflushed work in flight, the client
    // would block waiting for a reply nothing was left to send, and
    // `client_thread.join()` below would then block on it forever. Both
    // conditions (not just the toplevel count) really do need to hold
    // before it's safe to stop.
    let deadline = Instant::now() + Duration::from_secs(5);
    while server_state.toplevels.len() < count || !client_thread.is_finished() {
        display.dispatch_clients(&mut server_state).expect("test server dispatch failed");
        display.flush_clients().expect("test server flush failed");
        // Fails fast on a panicked client instead of spinning out the
        // full deadline below: a thread that already exited without
        // delivering every toplevel didn't get wedged (that leaves it
        // *not* finished — the normal timeout path handles that case) —
        // it errored out, and `join()` here both surfaces its real panic
        // message instead of a generic timeout and skips the remaining
        // wait entirely.
        if server_state.toplevels.len() < count && client_thread.is_finished() {
            match client_thread.join() {
                Err(payload) => std::panic::resume_unwind(payload),
                Ok(()) => panic!(
                    "test client thread exited early with only {} of {count} toplevel(s) and no panic",
                    server_state.toplevels.len()
                ),
            }
        }
        if Instant::now() > deadline {
            // Deliberately doesn't join `client_thread` first: if it's
            // genuinely wedged (the only way this deadline fires — see
            // this loop's own doc comment on why the happy path never
            // reaches it), joining would just trade this clear timeout
            // panic for an indefinite hang here instead. The thread leaks
            // for the rest of this test binary process, which is an
            // accepted cost of failing loudly rather than silently
            // hanging the whole suite — this path should never trigger
            // outside of a genuine regression in this harness itself.
            panic!(
                "timed out waiting for {count} test toplevel(s) and client teardown; {} toplevel(s) arrived",
                server_state.toplevels.len()
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    client_thread.join().expect("test client thread panicked");

    server_state
        .toplevels
        .into_iter()
        .map(|toplevel| {
            let handle = server_state
                .foreign_toplevel_list_state
                .new_toplevel::<ServerState>("test window", "test.app");
            (Window::new_wayland_window(toplevel), handle)
        })
        .collect()
}

/// A real `LoopHandle` from a real, never-run `EventLoop` — `graph_stack.rs`
/// and `ownership.rs` each already hand-rolled this exact same three-line
/// sequence independently before this consolidation (and a third,
/// freshly-written copy very nearly joined them, in `handlers/shell.rs`'s
/// own new test module — caught in review before that ever landed).
pub(crate) fn test_loop_handle() -> smithay::reexports::calloop::LoopHandle<'static, crate::State> {
    let event_loop: smithay::reexports::calloop::EventLoop<'static, crate::State> =
        smithay::reexports::calloop::EventLoop::try_new().unwrap();
    Box::leak(Box::new(event_loop)).handle()
}

/// A fresh, single-`ConsoleHut` `GraphStack` — same "three call sites,
/// one hand-rolled copy each" duplication [`test_loop_handle`] itself
/// used to have, consolidated here for the same reason.
pub(crate) fn test_stack() -> crate::graph_stack::GraphStack {
    let (hut, events) = crate::console_hut::ConsoleHut::spawn(std::iter::empty(), 1.0).unwrap();
    let (ping, _source) = smithay::reexports::calloop::ping::make_ping().unwrap();
    crate::graph_stack::GraphStack::new(hut, events, test_loop_handle(), Vec::new(), crate::redraw::RedrawHandle::new(ping))
        .unwrap()
}

/// `window`'s own toplevel surface — a one-line helper, but
/// `console_hut.rs`'s own test module already had an independent copy of
/// it before this consolidation (and `handlers/shell.rs`'s new one would
/// have made a second, caught in the same review pass as
/// [`test_loop_handle`]'s own near-duplicate).
pub(crate) fn surface_of(window: &Window) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    window.toplevel().unwrap().wl_surface().clone()
}

#[cfg(test)]
mod tests {
    use smithay::reexports::wayland_server::Resource;
    use smithay::wayland::seat::WaylandFocus;

    use super::*;

    #[test]
    fn spawns_distinct_windows_with_distinct_surfaces() {
        let windows = spawn_test_windows(2);
        assert_eq!(windows.len(), 2);
        assert_ne!(
            windows[0].0.wl_surface().map(|s| s.id()),
            windows[1].0.wl_surface().map(|s| s.id())
        );
    }
}
