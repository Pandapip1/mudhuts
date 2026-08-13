use std::cell::RefCell;
use std::ffi::OsString;
use std::rc::Rc;
use std::sync::Arc;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatState};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::ping::Ping;
use smithay::reexports::calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::foreign_toplevel_list::ForeignToplevelListState;
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::wlr_layer::{Layer as WlrLayer, WlrLayerShellState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

use crate::keybindings::Keymap;
use crate::stack::HutStack;
use crate::village::{Village, pane_rects};

/// Open mudhuts' listening Wayland socket without yet wiring it into an
/// event loop. Split out from [`State::new`] so callers can learn the
/// socket name (to export `WAYLAND_DISPLAY`) before spawning anything that
/// should connect to *this* compositor rather than whatever it's nested in.
pub fn create_socket() -> Result<(ListeningSocketSource, OsString), Box<dyn std::error::Error>> {
    let listening_socket = ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket.socket_name().to_os_string();
    Ok((listening_socket, socket_name))
}

pub struct State {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    /// Client windows. Phase 1 does not yet organize these into the Hut's
    /// Main Windows (that's Phase 4) — they're just composited on top.
    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,

    pub seat: Seat<Self>,

    pub stack: HutStack,
    pub keymap: Keymap,
    /// The output's current pixel size, tracked here (not just read inside
    /// `winit_backend.rs`'s redraw handler) so newly-focused Huts can be
    /// resized immediately on switch rather than showing a stale grid
    /// until the next real resize.
    pub output_size: (i32, i32),

    /// Set while dragging out a text selection in the terminal (left
    /// button held, no mouse reporting active).
    pub text_selecting: bool,
    /// Whether the drag in `text_selecting` has actually moved to a
    /// different cell yet — a plain click with no drag should clear any
    /// selection rather than leave a persistent single-cell one.
    pub text_selection_dragged: bool,
    /// The raw button code currently held for SGR mouse-reporting drag
    /// purposes (mutually exclusive with `text_selecting` — reporting mode
    /// and our own selection are never both active at once).
    pub mouse_report_button_held: Option<u32>,

    /// Set while dragging a docked Sub-Window's handle out to float —
    /// see `docks.rs`.
    pub dock_drag: Option<crate::docks::DockDrag>,

    /// The pointer's current position, tracked here explicitly for the
    /// udev/libinput backend's relative-motion events (real mice/
    /// touchpads report deltas, not an absolute position the way a
    /// nested winit window's host compositor does) — see
    /// `input.rs`'s `InputEvent::PointerMotion` handling. Unused under
    /// the winit backend, which computes an absolute position fresh
    /// from each event instead.
    pub pointer_location: Point<f64, Logical>,

    /// The pointer's current appearance as last requested by a client
    /// (`wl_pointer.set_cursor`) — updated by `SeatHandler::cursor_image`
    /// (`handlers/mod.rs`). Only the udev/DRM backend actually draws a
    /// cursor from this (see `cursor.rs`'s module doc); the winit backend
    /// relies on its host compositor's own cursor and never reads it.
    pub cursor_status: CursorImageStatus,
    /// `cursor-shape-v1` (`wp_cursor_shape_manager_v1`) — lets a client ask
    /// the compositor to draw a *named* cursor shape (`"pointer"`, `"text"`,
    /// `"grab"`, ...) instead of uploading its own cursor surface pixels.
    /// Its requests route straight into `SeatHandler::cursor_image`
    /// (`handlers/mod.rs`), the same entry point `wl_pointer.set_cursor`
    /// already uses, so `cursor_status` above needs no changes at all —
    /// only stored here (like every other `*State`) to keep its global
    /// registered for the compositor's lifetime.
    pub cursor_shape_manager_state: CursorShapeManagerState,

    /// dmabuf client-buffer import (`zwp_linux_dmabuf_v1`) — lets clients
    /// hand over a GPU buffer directly instead of a plain SHM buffer that
    /// has to be copied/re-uploaded on every commit. Always present (an
    /// empty `DmabufState` costs nothing), but the actual global is only
    /// created by `udev_backend.rs::init_udev` once a GBM-backed renderer
    /// exists — `winit_backend.rs` never populates `dmabuf_global`, so no
    /// global ever gets advertised there, matching this compositor's
    /// existing SHM-only behavior under that backend.
    pub dmabuf_state: DmabufState,
    /// Just needs to stay alive — dropping it would remove the global.
    pub dmabuf_global: Option<DmabufGlobal>,
    /// Shared with `udev_backend.rs`'s own `Inner::renderer` — needed so
    /// `DmabufHandler::dmabuf_imported` (`handlers/mod.rs`) can actually
    /// attempt the import; `State` doesn't otherwise have a renderer of
    /// its own; that's normally backend-private state.
    pub dmabuf_renderer: Option<Rc<RefCell<GlesRenderer>>>,

    /// `ext_foreign_toplevel_list_v1` — advertises every Main Window to
    /// any client that binds it (panels, taskbars, ...), and gives each
    /// one a stable identifier string. Phase 5b's `mudhuts_shell_authority_v1`
    /// piggybacks on that same identifier to let a trusted helper program
    /// tag *other* clients' toplevels without needing a direct object
    /// reference to them (see `handlers/shell.rs`).
    pub foreign_toplevel_list_state: ForeignToplevelListState,
    /// `wlr-layer-shell` (`zwlr_layer_shell_v1`) — lets clients like status
    /// bars/launchers/notification daemons anchor a surface to a screen
    /// edge/region outside the normal fullscreen-toplevel model, stacked
    /// in one of 4 layers (background/bottom/top/overlay) relative to
    /// normal content. See `handlers/layer_shell.rs`'s module doc.
    pub layer_shell_state: WlrLayerShellState,
    /// `keyboard-shortcuts-inhibit-unstable-v1` — lets a client (a VM
    /// viewer, remote-desktop app, ...) ask mudhuts not to intercept its
    /// own global keybindings for its surface, so it can forward raw key
    /// events (e.g. the guest's own Ctrl+Alt+Fn) through instead. See
    /// `input.rs`'s `process_input_event` for where this is actually
    /// consulted.
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    /// A one-time secret, generated fresh each run, that a helper program
    /// mudhuts itself spawns (see `main.rs`'s `--authority-helper`) must
    /// present via `mudhuts_shell_authority_v1.authenticate` before any of
    /// its other requests are honored — see `handlers/shell.rs`'s module
    /// doc for the trust model this establishes.
    pub authority_token: String,

    /// Wakes up the winit backend's redraw handler (see `winit_backend.rs`,
    /// the only place that owns the actual window handle needed to call
    /// its `request_redraw()`) from anywhere else that changes something
    /// visible — Wayland protocol handlers, the PTY event channel — via
    /// [`Self::request_redraw`]. The render loop only redraws in response
    /// to one of these pings (or input/resize, handled directly in
    /// `winit_backend.rs`) rather than continuously, so an idle compositor
    /// does no per-frame work at all.
    redraw_ping: Ping,
}

impl State {
    /// `socket` must already be listening (see [`create_socket`]) — created
    /// separately so `main` can export `WAYLAND_DISPLAY` for the Hut's
    /// shell (and anything it launches) *before* that shell is spawned,
    /// rather than after, which would leave it pointed at whatever
    /// compositor mudhuts itself is nested in.
    pub fn new(
        event_loop: &mut EventLoop<Self>,
        display: Display<Self>,
        stack: HutStack,
        socket: (ListeningSocketSource, OsString),
        redraw_ping: Ping,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let start_time = std::time::Instant::now();
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let popups = PopupManager::default();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&dh);
        dh.create_global::<Self, mudhuts_protocols::server::mudhuts_shell_v1::MudhutsShellV1, _>(
            2,
            smithay::wayland::GlobalData,
        );
        let foreign_toplevel_list_state = ForeignToplevelListState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        let authority_token = {
            use rand::distr::{Alphanumeric, SampleString};
            Alphanumeric.sample_string(&mut rand::rng(), 32)
        };

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "mudhuts");
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        let space = Space::default();
        let (listening_socket, socket_name) = socket;
        Self::init_wayland_listener(display, event_loop, listening_socket)?;
        let loop_signal = event_loop.get_signal();

        Ok(Self {
            start_time,
            socket_name,
            display_handle: dh,
            space,
            loop_signal,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            popups,
            seat,
            stack,
            keymap: Keymap::load(),
            output_size: (0, 0),
            text_selecting: false,
            text_selection_dragged: false,
            mouse_report_button_held: None,
            dock_drag: None,
            pointer_location: Point::from((0.0, 0.0)),
            cursor_status: CursorImageStatus::default_named(),
            cursor_shape_manager_state,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            dmabuf_renderer: None,
            foreign_toplevel_list_state,
            layer_shell_state,
            keyboard_shortcuts_inhibit_state,
            authority_token,
            redraw_ping,
        })
    }

    /// Ask the render loop to redraw soon. Safe and cheap to call from
    /// anywhere — Wayland protocol handlers, the PTY event channel — see
    /// [`Self::redraw_ping`].
    pub fn request_redraw(&self) {
        self.redraw_ping.ping();
    }

    /// Route a `TermEvent` from one of The Stack's Huts (identified by
    /// `id`, stable across the Stack's own reordering/discarding) to the
    /// right place.
    pub fn handle_term_event(&mut self, id: u64, event: mudhuts_term::TermEvent) {
        match event {
            mudhuts_term::TermEvent::Title(title) => {
                tracing::debug!("hut {id} title: {title}");
            }
            mudhuts_term::TermEvent::Exited => {
                tracing::info!("hut {id} shell exited");
                // Closing the very last Hut (e.g. Ctrl+D-ing out of it)
                // is the closest thing mudhuts has to a "log out"/"close
                // the window" gesture — there's no other way to exit at
                // all otherwise (no window-close button under winit, no
                // way back to a login greeter under the real TTY
                // backend without this). Skips the normal remove/
                // respawn path entirely (nothing will touch the stack
                // again before the process exits, so leaving the now-
                // dead Hut entry in place is harmless) rather than
                // trying to keep the "always ≥1 Hut" invariant intact
                // through a stop-in-progress shutdown.
                //
                // `self.stack.len()` counts *top-level Stack entries*, not
                // Huts — since Phase 6, one entry can be a Tab/Tile-Village
                // wrapping several Huts. Checking `len() == 1` alone meant
                // exiting a Tile-Village's *active* pane (its Hut id
                // happens to equal `self.stack.focused().id`, since that
                // resolves through the active pane) looked exactly like
                // closing the only Hut in the whole compositor — even
                // with a live sibling pane right next to it — silently
                // exiting the entire compositor (and, on the real DRM
                // backend, dropping straight back to the login greeter)
                // instead of just falling back to that sibling. Counting
                // every Hut in the whole tree is the correct "is this
                // really the last one" check.
                if self.stack.all_huts().count() == 1 && self.stack.focused().id == id {
                    tracing::info!("last Hut closed, exiting");
                    self.loop_signal.stop();
                    return;
                }
                if let Err(err) = self.stack.remove_exited(id) {
                    tracing::error!("failed to respawn after shell exit: {err}");
                }
                self.request_redraw();
            }
            mudhuts_term::TermEvent::Wakeup => {
                // Only the focused Hut's content is currently visible;
                // a background Hut changing doesn't need a redraw yet
                // (there's no activity indicator to update either, in
                // this phase).
                if self.stack.focused().id == id {
                    self.request_redraw();
                }
            }
        }
    }

    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<Self>,
        listening_socket: ListeningSocketSource,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let loop_handle = event_loop.handle();

        loop_handle.insert_source(listening_socket, move |client_stream, _, state| {
            if let Err(err) = state
                .display_handle
                .insert_client(client_stream, Arc::new(ClientState::default()))
            {
                tracing::warn!("failed to insert new client: {err}");
            }
        })?;

        loop_handle.insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // Safety: `display` is not dropped while registered with calloop.
                if let Err(err) = unsafe { display.get_mut().dispatch_clients(state) } {
                    tracing::warn!("error dispatching wayland clients: {err}");
                }
                // Dispatching a client's request can itself queue a
                // response (an initial configure, an ack'd commit's
                // buffer-release, ...) — same reasoning as
                // `input.rs`'s `process_input_event`: nothing else
                // flushes client sockets between redraw passes, so
                // without this a client wouldn't see its own request's
                // response until some unrelated redraw happened to flush
                // it.
                let _ = state.display_handle.flush_clients();
                Ok(PostAction::Continue)
            },
        )?;

        Ok(())
    }

    /// The rectangle (physical pixels, output-relative) that Hut/Main-
    /// Window *content* should actually be sized/positioned against —
    /// the full output, minus whatever every mapped layer-shell surface's
    /// exclusive zone currently reserves (see `handlers/layer_shell.rs`'s
    /// module doc). Falls back to the full output size at `(0, 0)` if
    /// there's no output yet.
    ///
    /// Deliberately narrower than "everything mudhuts draws": its own
    /// chrome (`chrome.rs`/`village_chrome.rs`'s tab strips, `docks.rs`'s
    /// edge handles, the Alt-Tab popup) still anchors to the raw output
    /// rect unconditionally — a real, accepted v1 gap (a top-anchored
    /// panel and mudhuts' own tab strip can visually collide) rather than
    /// threading this through every chrome element too.
    pub fn usable_area(&self) -> (i32, i32, i32, i32) {
        let Some(output) = self.space.outputs().next() else {
            return (0, 0, self.output_size.0, self.output_size.1);
        };
        let zone = layer_map_for_output(output).non_exclusive_zone();
        (zone.loc.x, zone.loc.y, zone.size.w, zone.size.h)
    }

    /// Whether the focused Hut's terminal (vs. its active Main Window)
    /// should currently be the visible view — `Hut::showing_terminal`,
    /// but forced true when that Hut has no Main Windows to toggle to.
    pub fn showing_terminal_effective(&self) -> bool {
        // A genuinely tiled Tile-Village (2+ panes) always shows every
        // pane's terminal, regardless of any individual Hut's own
        // `showing_terminal` flag — see `render.rs`'s Tile-Village
        // compositing and `village.rs`'s module doc on why Main Windows
        // aren't shown in a tile pane in v1. Without this, a Hut that had
        // toggled to a Main Window before being tiled would report
        // `false` here while still visually showing its terminal,
        // desyncing mouse-interaction routing (selection/mouse-reports)
        // from what's actually on screen.
        if matches!(self.stack.focused_village(), Village::Tile(tile) if tile.children.len() >= 2)
        {
            return true;
        }
        let hut = self.stack.focused();
        hut.showing_terminal || hut.main_window_count() == 0
    }

    /// Screen-space offset of whichever pane currently has effective
    /// focus — always at least [`Self::usable_area`]'s own origin (`(0.0,
    /// 0.0)` unless a layer-shell surface reserves part of the output),
    /// plus the Tile-Village pane offset on top of that if the focused
    /// top-level Village is one (see `render.rs`'s Tile-Village
    /// compositing, which places each pane at exactly this same offset)
    /// — so mouse interaction (selection, click, scroll) lines up with
    /// the pane that's actually on screen there, rather than being
    /// computed against the raw output as if the focused Hut's terminal
    /// still filled it edge to edge.
    pub fn active_pane_offset(&self) -> (f64, f64) {
        let (area_x, area_y, area_w, area_h) = self.usable_area();
        let Village::Tile(tile) = self.stack.focused_village() else {
            return (area_x as f64, area_y as f64);
        };
        if tile.children.len() < 2 {
            return (area_x as f64, area_y as f64);
        }
        let rects = pane_rects(
            tile.axis,
            tile.children.iter().map(|(_, frac)| *frac),
            (area_w, area_h),
        );
        let (x, y, _, _) = rects[tile.active];
        ((area_x + x) as f64, (area_y + y) as f64)
    }

    /// Make `self.space` match what the focused Hut should currently be
    /// showing: unmap whatever's mapped (harmless if nothing was), then map
    /// the focused Hut's active Main Window (if it isn't showing its
    /// terminal) plus every currently-floating Sub-Window and every Alert
    /// belonging to that Main Window — docked Sub-Windows stay unmapped
    /// (`docks.rs` draws a handle instead), Alerts are mapped last so they
    /// end up on top. Call after anything that could change which Hut/tab
    /// is focused or which view a Hut is showing (Alt-Tab commit,
    /// `ToggleTerminal`, `TabNext`/`TabPrev`, a new toplevel auto-switching
    /// in, a toplevel closing).
    pub fn sync_visible_main_window(&mut self) {
        let mapped: Vec<_> = self.space.elements().cloned().collect();
        for window in mapped {
            self.space.unmap_elem(&window);
        }
        let hut = self.stack.focused();
        if hut.showing_terminal {
            return;
        }
        let Some(entry) = hut.active_main_window_entry() else {
            return;
        };
        // Positioned at the usable area's own origin, not literally
        // (0, 0) — matters once a layer-shell surface reserves part of
        // the output (e.g. a left-anchored panel) — see
        // `Self::usable_area`'s doc comment.
        let (area_x, area_y, _, _) = self.usable_area();
        self.space
            .map_element(entry.window.clone(), (area_x, area_y), false);
        for sub in &entry.sub_windows {
            if let crate::main_window::Dock::Floating(pos) = sub.dock {
                self.space.map_element(sub.window.clone(), pos, false);
            }
        }
        for alert in &entry.alerts {
            self.space
                .map_element(alert.window.clone(), alert.position, false);
        }
    }

    /// Find a window (Main Window, Sub-Window, or Alert) by its surface
    /// across *every* Hut, not just whatever's currently visible in
    /// `self.space` — a background Hut's windows still need commit/
    /// configure handling while hidden, and so do docked Sub-Windows that
    /// aren't mapped at all.
    pub fn find_window_by_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.stack.all_huts().find_map(|h| {
            h.main_windows().iter().find_map(|entry| {
                if entry.matches(surface) {
                    return Some(entry.window.clone());
                }
                if let Some(sub) = entry.sub_windows.iter().find(|s| s.matches(surface)) {
                    return Some(sub.window.clone());
                }
                entry
                    .alerts
                    .iter()
                    .find(|a| a.matches(surface))
                    .map(|a| a.window.clone())
            })
        })
    }

    /// Topmost surface under `pos`, for pointer motion/click routing —
    /// checked in the same front-to-back z-order `render.rs`'s
    /// `build_frame_elements` actually draws: a `wlr-layer-shell` Top or
    /// Overlay surface first (drawn above normal content — see
    /// `layer_elements`'s doc comment), then whatever's mapped in
    /// `self.space` (the focused Hut's visible Main Window/Sub-Windows/
    /// Alerts), then a Bottom or Background layer surface last. Doesn't
    /// need to special-case the terminal-visible branch: while the
    /// terminal itself is showing, it occupies the whole usable area and
    /// nothing in `self.space` is mapped there for it to compete with,
    /// so a layer surface only ever gets picked up here when there's
    /// genuinely nothing else on top of it at that point.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        Point<f64, Logical>,
    )> {
        let output = self.space.outputs().next();

        if let Some(output) = output {
            let layers = layer_map_for_output(output);
            if let Some(hit) = layers
                .layer_under(WlrLayer::Overlay, pos)
                .or_else(|| layers.layer_under(WlrLayer::Top, pos))
                .and_then(|layer| Self::under_layer(&layers, layer, pos))
            {
                return Some(hit);
            }
        }

        if let Some(hit) = self.space.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        }) {
            return Some(hit);
        }

        let output = output?;
        let layers = layer_map_for_output(output);
        layers
            .layer_under(WlrLayer::Bottom, pos)
            .or_else(|| layers.layer_under(WlrLayer::Background, pos))
            .and_then(|layer| Self::under_layer(&layers, layer, pos))
    }

    /// Shared tail of a layer-surface hit test: resolve `layer`'s own
    /// surface (or a subsurface/popup of it) under `pos`, given `pos` is
    /// still in output-relative coordinates (not yet offset by the
    /// layer's own on-screen location).
    fn under_layer(
        layers: &smithay::desktop::LayerMap,
        layer: &smithay::desktop::LayerSurface,
        pos: Point<f64, Logical>,
    ) -> Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        Point<f64, Logical>,
    )> {
        let layer_loc = layers.layer_geometry(layer)?.loc;
        layer
            .surface_under(pos - layer_loc.to_f64(), WindowSurfaceType::ALL)
            .map(|(s, p)| (s, (p + layer_loc).to_f64()))
    }
}

/// Data associated with a wayland client that connects to mudhuts.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
