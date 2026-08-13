use std::cell::RefCell;
use std::ffi::OsString;
use std::rc::Rc;
use std::sync::Arc;

use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::WinitGraphicsBackend;
use smithay::desktop::{PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatState};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::ping::Ping;
use smithay::reexports::calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::foreign_toplevel_list::ForeignToplevelListState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::image_capture_source::{ImageCaptureSourceState, OutputCaptureSourceState};
use smithay::wayland::image_copy_capture::{ImageCopyCaptureState, Session};
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::ext_data_control::DataControlState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::session_lock::{LockSurface, SessionLockManagerState, SessionLocker};
use smithay::wayland::shell::wlr_layer::{Layer as WlrLayer, WlrLayerShellState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::viewporter::ViewporterState;

use crate::keybindings::Keymap;
use crate::stack::MruStackHut;
use crate::hut::{Hut, pane_rects};

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

    /// Client windows. Phase 1 does not yet organize these into the ConsoleHut's
    /// Main Windows (that's Phase 4) — they're just composited on top.
    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// `xdg-wm-dialog-v1` — lets a toolkit mark a toplevel as a "dialog"
    /// (optionally "modal") relative to a parent set via
    /// `xdg_toplevel.set_parent`, so `handlers/xdg_shell.rs`'s
    /// `handle_commit` can skip the usual fullscreen hint for it. No
    /// state of its own beyond the global handle; the actual per-toplevel
    /// hint lives on `XdgToplevelSurfaceData` (Smithay's own storage).
    pub xdg_dialog_state: XdgDialogState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    /// `zwp_primary_selection_v1` — the X11-style "select text = copy to
    /// primary" selection, independent of the regular clipboard (see
    /// `input.rs`'s `PointerButton` handler, which commits to this on
    /// every completed drag-selection, vs. an explicit copy keybinding for
    /// `data_device_state` above). Shares its per-seat storage with
    /// `data_device_state` under the hood (both just set fields on the
    /// same `SeatData`) — this is a second protocol surface, not a second
    /// independent selection system.
    pub primary_selection_state: PrimarySelectionState,
    /// `wp_viewporter` — lets a client crop/scale its own buffer to an
    /// arbitrary destination size independent of the buffer's native
    /// pixel dimensions. No handler trait at all (unlike every other
    /// `*State` field here) — `handlers/compositor.rs`'s `commit()`
    /// already calls `on_commit_buffer_handler`, which (per that
    /// protocol's own module doc) calls `ensure_viewport_valid` for every
    /// surface itself, so this needs no wiring beyond existing here to
    /// keep the global registered.
    pub viewporter_state: ViewporterState,
    /// `wp_fractional_scale_v1` — tells a client the output's *exact*
    /// scale factor (e.g. `1.5`), not just the coarse integer `wl_output`
    /// clients otherwise have to round up to. Paired with
    /// `viewporter_state` above: together they're what let a HiDPI-aware
    /// client render its own content at the output's real scale instead
    /// of over- or under-sampling. Driven by
    /// `FractionalScaleHandler::new_fractional_scale`
    /// (`handlers/compositor.rs`), which pushes `State::output_scale()`
    /// to a surface the moment it asks for one — mudhuts is single-
    /// output and never changes scale mid-session (see `output_scale()`'s
    /// doc comment), so unlike anvil's `post_repaint`, there's no
    /// ongoing per-frame re-push loop needed here: nothing could ever
    /// change between one push and the next.
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    /// `ext_data_control_v1` — lets a privileged client (a clipboard
    /// manager/history tool) see and set *both* selections above on this
    /// seat's behalf, rather than just this compositor. Reuses
    /// `set_data_device_selection`/`set_primary_selection` for the
    /// "compositor sets a selection" direction; there's no separate setter
    /// for data-control specifically (see `handlers/mod.rs`'s module doc).
    pub data_control_state: DataControlState,
    pub popups: PopupManager,

    pub seat: Seat<Self>,

    pub stack: MruStackHut,
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

    /// Set while dragging a docked Floating Window's handle out to float —
    /// see `docks.rs`.
    pub dock_drag: Option<crate::docks::DockDrag>,

    /// The pointer's current position, tracked here explicitly for the
    /// udev/libinput backend's relative-motion events (real mice/
    /// touchpads report deltas, not an absolute position the way a
    /// nested winit window's host compositor does) — see
    /// `input.rs`'s `InputEvent::PointerMotion` handling. Unused under
    /// the winit backend, which computes an absolute position fresh
    /// from each event instead.
    ///
    /// Genuinely [`Logical`] (scale-divided), matching every other value
    /// that flows through `self.space`/`pointer.motion()`/
    /// `surface_under()` — *not* physical output pixels, unlike
    /// `output_size`/`usable_area()` below. `input.rs`'s
    /// `handle_pointer_motion` converts to physical once, locally, for
    /// the handful of call sites (chrome/dock hit-testing, terminal
    /// cell math) that need mudhuts' own native pixel space instead —
    /// see that function's doc comment for why the two can't just be
    /// unified onto one space.
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
    /// Mirrors `dmabuf_renderer` above, but for the winit backend: nothing
    /// under `winit_backend.rs` otherwise stores a renderer/backend handle
    /// reachable from a Wayland `Dispatch` callback (the renderer there
    /// normally only exists transiently inside `backend.bind()`'s scope,
    /// inside the redraw closure) — needed because screenshot capture
    /// (`handlers/capture.rs`) is client-pull-driven, firing from a
    /// `Dispatch` callback rather than tied to either backend's own redraw
    /// tick. Only ever one of this and `dmabuf_renderer` is set, depending
    /// on which backend actually started.
    pub winit_backend: Option<Rc<RefCell<WinitGraphicsBackend<GlesRenderer>>>>,

    /// `ext-image-capture-source-v1` core state (shared by every source
    /// type; see `handlers/capture.rs`'s module doc).
    pub image_capture_source_state: ImageCaptureSourceState,
    /// `ext-output-image-capture-source-v1` — lets a client turn a
    /// `wl_output` into an opaque capture source. No toplevel/region
    /// capture source in v1 (see `handlers/capture.rs`'s module doc).
    pub output_capture_source_state: OutputCaptureSourceState,
    /// `ext-image-copy-capture-v1` — the actual screenshot protocol; see
    /// `handlers/capture.rs`.
    pub image_copy_capture_state: ImageCopyCaptureState,
    /// Owned capture [`Session`]s, kept alive here for as long as their
    /// client keeps them open — `Session`'s `Drop` impl immediately sends
    /// `stopped()` and fails every pending frame, so dropping one before
    /// the client is done would look exactly like a spurious capture
    /// failure. Entries for destroyed sessions are removed by
    /// `ImageCopyCaptureHandler::session_destroyed`; `ImageCopyCaptureState`'s
    /// own separate internal tracking Vecs need a periodic `.cleanup()`
    /// call instead (both backends' redraw ticks do this already, next to
    /// `state.popups.cleanup()`).
    pub image_copy_sessions: Vec<Session>,

    /// DRM leasing (`wp_drm_lease_v1`) global state — see
    /// `udev_backend.rs`'s module doc and `DrmLeaseHandler` impl. Lives
    /// directly on `State` (unlike `udev_inner` below) because
    /// `DrmLeaseHandler::drm_lease_state` has to hand back a `&mut
    /// DrmLeaseState` tied to `&mut self`, which nothing reached through
    /// `Rc<RefCell<_>>` can do. `None` under `winit_backend.rs` (no
    /// global ever created there) or if `init_udev`'s
    /// `DrmLeaseState::new` failed at startup (logged, non-fatal — DRM
    /// leasing is optional, the desktop output must come up regardless).
    pub drm_leasing_global: Option<smithay::wayland::drm_lease::DrmLeaseState>,
    /// Shared with `udev_backend.rs`'s own `Inner` (a clone of the same
    /// `Rc`) — needed so `DrmLeaseHandler`'s other trait methods (also
    /// `&mut self` on `State`) can reach the backend's DRM device handle,
    /// non-desktop connector list, and active-lease list, none of which
    /// `State` otherwise has access to (same reasoning as
    /// `dmabuf_renderer` above). `None` under `winit_backend.rs`.
    /// Also doubles as the backend handle `wlr-gamma-control-unstable-v1`'s
    /// `set_gamma`/`get_gamma` ioctl calls go through (no separate field —
    /// same `Inner`, same reasoning: `State` has no DRM device handle of
    /// its own).
    pub(crate) udev_inner: Option<Rc<RefCell<crate::udev_backend::Inner>>>,

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
    /// `ext-session-lock-v1` — lets a trusted screen-locker client blank
    /// the screen and take over all input until it explicitly unlocks
    /// (see `handlers/session_lock.rs`'s module doc for the full
    /// lifecycle). Mirrors `layer_shell_state`'s pattern: the manager
    /// global lives here, everything else the protocol needs is the
    /// handful of fields immediately below.
    pub session_lock_state: SessionLockManagerState,
    /// Whether a client currently holds the session lock — checked ahead
    /// of *everything* else in `input.rs`'s `process_input_event` (a
    /// locked session wins even over an `exclusive` layer-shell surface)
    /// and in `render.rs`'s `build_frame_elements` (nothing else is drawn
    /// while this is set). Left `true` if the locking client dies without
    /// unlocking — see `handlers/session_lock.rs`'s `unlock` doc comment
    /// on why that's the protocol-correct default, not a bug.
    pub locked: bool,
    /// The identity of whichever `ext_session_lock_v1` this compositor has
    /// actually accepted (as opposed to a racing second client's, which
    /// gets its confirmation dropped — see `handlers/session_lock.rs`'s
    /// `lock`) — kept independently of `pending_lock` (which is consumed
    /// once confirmed) so a late `new_surface` call can still be checked
    /// against it for the lock's entire lifetime, not just until the
    /// first frame confirms it.
    pub accepted_lock: Option<ExtSessionLockV1>,
    /// This output's lock surface, if the locking client has mapped one —
    /// single-output throughout (like everything else here), so a plain
    /// `Option` suffices rather than a per-output map. `None` means
    /// "locked, but nothing to show yet" — `render.rs`'s
    /// `build_frame_elements` still blanks the screen either way.
    pub lock_surface: Option<LockSurface>,
    /// Held between accepting a lock request and actually confirming it —
    /// see `handlers/session_lock.rs`'s `lock` doc comment for why that
    /// confirmation can't happen synchronously, and `udev_backend.rs`'s
    /// `render_surface`/`winit_backend.rs`'s redraw handler for where it
    /// finally gets taken and confirmed.
    pub pending_lock: Option<SessionLocker>,
    /// Stable identity for the plain blank backdrop `render.rs`'s
    /// `build_frame_elements` draws while locked and no lock surface is
    /// mapped yet (or one just went stale) — kept here rather than
    /// minted fresh every frame so Smithay's damage tracking sees the
    /// same element across frames instead of "a brand new one" each time
    /// (mirrors why `village.rs`'s `highlight_ids` are stored, not
    /// generated per-frame).
    pub lock_backdrop_id: Id,
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
    /// separately so `main` can export `WAYLAND_DISPLAY` for the ConsoleHut's
    /// shell (and anything it launches) *before* that shell is spawned,
    /// rather than after, which would leave it pointed at whatever
    /// compositor mudhuts itself is nested in.
    pub fn new(
        event_loop: &mut EventLoop<Self>,
        display: Display<Self>,
        stack: MruStackHut,
        socket: (ListeningSocketSource, OsString),
        redraw_ping: Ping,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let start_time = std::time::Instant::now();
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_dialog_state = XdgDialogState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let popups = PopupManager::default();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        // Order matters: this constructor borrows `primary_selection_state`
        // to decide whether to advertise primary-selection support to
        // data-control clients too, so it must come after.
        let data_control_state =
            DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_| true);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        dh.create_global::<Self, mudhuts_protocols::server::mudhuts_shell_v1::MudhutsShellV1, _>(
            2,
            smithay::wayland::GlobalData,
        );
        let foreign_toplevel_list_state = ForeignToplevelListState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        // `|_| true`: every client is allowed to see the global, matching
        // this compositor's general permissiveness elsewhere (there's no
        // broader client-trust/allowlist model here to hook a narrower
        // filter into — see `handlers/shell.rs`'s module doc for the one
        // place mudhuts *does* have one, which is unrelated to this).
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, |_| true);
        let image_capture_source_state = ImageCaptureSourceState::new();
        let output_capture_source_state = OutputCaptureSourceState::new::<Self>(&dh);
        let image_copy_capture_state = ImageCopyCaptureState::new::<Self>(&dh);
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
            xdg_dialog_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            data_control_state,
            viewporter_state,
            fractional_scale_manager_state,
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
            drm_leasing_global: None,
            udev_inner: None,
            winit_backend: None,
            image_capture_source_state,
            output_capture_source_state,
            image_copy_capture_state,
            image_copy_sessions: Vec::new(),
            foreign_toplevel_list_state,
            layer_shell_state,
            keyboard_shortcuts_inhibit_state,
            session_lock_state,
            locked: false,
            accepted_lock: None,
            lock_surface: None,
            pending_lock: None,
            lock_backdrop_id: Id::new(),
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

    /// A cloneable handle onto the same ping [`Self::request_redraw`] uses,
    /// for anything implementing [`crate::redraw::Redrawable`] that isn't
    /// `State` itself — see that module's doc comment.
    pub fn redraw_handle(&self) -> crate::redraw::RedrawHandle {
        crate::redraw::RedrawHandle::new(self.redraw_ping.clone())
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
                // Closing the very last ConsoleHut (e.g. Ctrl+D-ing out of it)
                // is the closest thing mudhuts has to a "log out"/"close
                // the window" gesture — there's no other way to exit at
                // all otherwise (no window-close button under winit, no
                // way back to a login greeter under the real TTY
                // backend without this). Skips the normal remove/
                // respawn path entirely (nothing will touch the stack
                // again before the process exits, so leaving the now-
                // dead ConsoleHut entry in place is harmless) rather than
                // trying to keep the "always ≥1 ConsoleHut" invariant intact
                // through a stop-in-progress shutdown.
                //
                // `self.stack.len()` counts *top-level Stack entries*, not
                // Huts — since Phase 6, one entry can be a Tab/Tile-Hut
                // wrapping several Huts. Checking `len() == 1` alone meant
                // exiting a Tile-Hut's *active* pane (its ConsoleHut id
                // happens to equal `self.stack.focused().id`, since that
                // resolves through the active pane) looked exactly like
                // closing the only ConsoleHut in the whole compositor — even
                // with a live sibling pane right next to it — silently
                // exiting the entire compositor (and, on the real DRM
                // backend, dropping straight back to the login greeter)
                // instead of just falling back to that sibling. Counting
                // every ConsoleHut in the whole tree is the correct "is this
                // really the last one" check.
                if self.stack.all_huts().count() == 1 && self.stack.focused().id == id {
                    tracing::info!("last ConsoleHut closed, exiting");
                    self.loop_signal.stop();
                    return;
                }
                if let Err(err) = self.stack.remove_exited(id) {
                    tracing::error!("failed to respawn after shell exit: {err}");
                }
                self.request_redraw();
            }
            mudhuts_term::TermEvent::Wakeup => {
                // Only the focused ConsoleHut's content is currently visible;
                // a background ConsoleHut changing doesn't need a redraw yet
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

    /// This output's current scale factor — `1.0` if there's no output
    /// yet (matches every other single-output fallback in this file).
    ///
    /// Set once, at output creation (`winit_backend.rs`/`udev_backend.rs`),
    /// from real detection (the host window's own DPI scale under winit;
    /// a physical-size/mode-resolution heuristic, overridable via
    /// `MUDHUTS_OUTPUT_SCALE`, under udev) and never changed again —
    /// mudhuts has no runtime "change display scale" mechanism (no
    /// settings UI, no hotplug-driven rescale), so there's nothing that
    /// would ever need this to be anything other than a fixed value read
    /// fresh from the `Output` each time it's needed, rather than cached
    /// anywhere.
    pub fn output_scale(&self) -> f64 {
        self.space
            .outputs()
            .next()
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0)
    }

    /// The rectangle (physical pixels, output-relative) that ConsoleHut/Main-
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
    ///
    /// `layer_map_for_output`'s own `non_exclusive_zone()` is genuinely
    /// [`Logical`] (Smithay arranges layer-shell surfaces against the
    /// output's scale-divided size, same as everything else it manages —
    /// see `handlers/layer_shell.rs`'s module doc) — converted to
    /// physical here so this keeps meaning what its own doc/name always
    /// have, for the ~10 call sites (`render.rs`, `docks.rs`, `input.rs`,
    /// both backends' resize/redraw handlers) that size/hit-test mudhuts'
    /// own pixel-native rendering against it. [`Self::usable_area_logical`]
    /// is the *other* half: the same zone, unconverted, for the couple of
    /// call sites that configure real Wayland clients instead.
    pub fn usable_area(&self) -> (i32, i32, i32, i32) {
        let Some(output) = self.space.outputs().next() else {
            return (0, 0, self.output_size.0, self.output_size.1);
        };
        let zone = layer_map_for_output(output).non_exclusive_zone();
        let physical: smithay::utils::Rectangle<i32, smithay::utils::Physical> =
            zone.to_physical_precise_round(self.output_scale());
        (physical.loc.x, physical.loc.y, physical.size.w, physical.size.h)
    }

    /// [`Self::usable_area`]'s raw, unconverted counterpart — genuinely
    /// [`Logical`] (scale-divided), for the two call sites
    /// (`handlers/xdg_shell.rs`'s `new_toplevel`,
    /// `winit_backend.rs`'s `WinitEvent::Resized`) that build an
    /// `xdg_toplevel` configure size: that's real client-facing protocol
    /// state, which Wayland always expresses in logical coordinates
    /// regardless of how many physical pixels mudhuts itself renders a
    /// Main Window's fullscreen content into. Falls back to
    /// `self.output_size` (physical) with no output mapped yet — matches
    /// `usable_area()`'s identical fallback, and only ever actually
    /// matters before the first output exists, when physical and logical
    /// aren't meaningfully different yet anyway.
    pub fn usable_area_logical(&self) -> (i32, i32, i32, i32) {
        let Some(output) = self.space.outputs().next() else {
            return (0, 0, self.output_size.0, self.output_size.1);
        };
        let zone = layer_map_for_output(output).non_exclusive_zone();
        (zone.loc.x, zone.loc.y, zone.size.w, zone.size.h)
    }

    /// The whole output's current size, genuinely [`Logical`] (scale-
    /// divided) — as opposed to `self.output_size`, which is always
    /// physical. Used wherever a physical-pixel-native value (a dragged
    /// Floating Window's position/size, read back from `self.space` and
    /// therefore already Logical) needs to be compared against "the
    /// output's size" in the *same* space — see `grabs.rs`'s `unset` and
    /// `docks.rs`'s `finish_drag`, both computing whether a drop point is
    /// near an edge. Falls back to `self.output_size` with no output
    /// mapped yet, same reasoning as `usable_area_logical`.
    pub fn output_size_logical(&self) -> (i32, i32) {
        let Some(output) = self.space.outputs().next() else {
            return self.output_size;
        };
        match self.space.output_geometry(output) {
            Some(geo) => (geo.size.w, geo.size.h),
            None => self.output_size,
        }
    }

    /// Whether the focused ConsoleHut's terminal (vs. its active Main Window)
    /// should currently be the visible view — `ConsoleHut::showing_terminal`,
    /// but forced true when that ConsoleHut has no Main Windows to toggle to.
    pub fn showing_terminal_effective(&self) -> bool {
        // A genuinely tiled Tile-Hut (2+ panes) always shows every
        // pane's terminal, regardless of any individual ConsoleHut's own
        // `showing_terminal` flag — see `render.rs`'s Tile-Hut
        // compositing and `village.rs`'s module doc on why Main Windows
        // aren't shown in a tile pane in v1. Without this, a ConsoleHut that had
        // toggled to a Main Window before being tiled would report
        // `false` here while still visually showing its terminal,
        // desyncing mouse-interaction routing (selection/mouse-reports)
        // from what's actually on screen.
        if matches!(self.stack.focused_top_level(), Hut::Tile(tile) if tile.children.len() >= 2)
        {
            return true;
        }
        let hut = self.stack.focused();
        hut.showing_terminal || hut.main_window_count() == 0
    }

    /// Screen-space offset of whichever pane currently has effective
    /// focus — always at least [`Self::usable_area`]'s own origin (`(0.0,
    /// 0.0)` unless a layer-shell surface reserves part of the output),
    /// plus the Tile-Hut pane offset on top of that if the focused
    /// top-level Hut is one (see `render.rs`'s Tile-Hut
    /// compositing, which places each pane at exactly this same offset)
    /// — so mouse interaction (selection, click, scroll) lines up with
    /// the pane that's actually on screen there, rather than being
    /// computed against the raw output as if the focused ConsoleHut's terminal
    /// still filled it edge to edge.
    pub fn active_pane_offset(&self) -> (f64, f64) {
        let (area_x, area_y, area_w, area_h) = self.usable_area();
        let Hut::Tile(tile) = self.stack.focused_top_level() else {
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

    /// Make `self.space` match what the focused ConsoleHut should currently be
    /// showing: unmap whatever's mapped (harmless if nothing was), then map
    /// the focused ConsoleHut's active Main Window (if it isn't showing its
    /// terminal) plus every currently-floating Floating Window and every Alert
    /// belonging to that Main Window — docked Floating Windows stay unmapped
    /// (`docks.rs` draws a handle instead), Alerts are mapped last so they
    /// end up on top. Call after anything that could change which ConsoleHut/tab
    /// is focused or which view a ConsoleHut is showing (Alt-Tab commit,
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
        for sub in &entry.floating_windows {
            if let crate::main_window::Dock::Floating(pos) = sub.dock {
                self.space.map_element(sub.window.clone(), pos, false);
            }
        }
        for alert in &entry.alerts {
            self.space
                .map_element(alert.window.clone(), alert.position, false);
        }
    }

    /// Find a window (Main Window, Floating Window, or Alert) by its surface
    /// across *every* ConsoleHut, not just whatever's currently visible in
    /// `self.space` — a background ConsoleHut's windows still need commit/
    /// configure handling while hidden, and so do docked Floating Windows that
    /// aren't mapped at all.
    pub fn find_window_by_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.stack.all_huts().find_map(|h| {
            h.main_windows().iter().find_map(|entry| {
                if entry.matches(surface) {
                    return Some(entry.window.clone());
                }
                if let Some(sub) = entry.floating_windows.iter().find(|s| s.matches(surface)) {
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
    /// `self.space` (the focused ConsoleHut's visible Main Window/Floating Windows/
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
