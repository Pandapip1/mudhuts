use std::ffi::OsString;
use std::sync::Arc;

use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::{Seat, SeatState};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

use crate::hut::Hut;
use crate::keybindings::Keymap;

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

    pub hut: Hut,
    pub keymap: Keymap,

    /// Whether the Hut's terminal (vs. its client window(s)) is the
    /// active view, toggled by `Action::ToggleTerminal` (Ctrl+`).
    ///
    /// Interim placeholder for the real Main Window tab system (Phase 4):
    /// for now a Hut's client windows are shown centered over a blacked-out
    /// background, entirely replacing the terminal view rather than
    /// compositing on top of it, so a toggle is needed to get back to the
    /// terminal at all. Ignored (forced true) when there are no client
    /// windows to toggle to.
    pub showing_terminal: bool,

    /// A visible mouse pointer. Smithay tracks pointer position/focus for
    /// input purposes regardless, but nothing renders it unless we do.
    pub cursor_buffer: SolidColorBuffer,
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
        hut: Hut,
        socket: (ListeningSocketSource, OsString),
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let start_time = std::time::Instant::now();
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let popups = PopupManager::default();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);

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
            hut,
            keymap: Keymap::load(),
            showing_terminal: true,
            cursor_buffer: SolidColorBuffer::new((10, 10), [1.0, 1.0, 1.0, 1.0]),
        })
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
                Ok(PostAction::Continue)
            },
        )?;

        Ok(())
    }

    /// Whether the terminal (vs. a client window) should currently be the
    /// visible/focused view — [`Self::showing_terminal`], but forced true
    /// when there's nothing to toggle to.
    pub fn showing_terminal_effective(&self) -> bool {
        self.showing_terminal || self.space.elements().next().is_none()
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        Point<f64, Logical>,
    )> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(
                        pos - location.to_f64(),
                        smithay::desktop::WindowSurfaceType::ALL,
                    )
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
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
