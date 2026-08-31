use std::cell::RefCell;
use std::ffi::OsString;
use std::rc::Rc;
use std::sync::Arc;

use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::WinitGraphicsBackend;
use smithay::desktop::{PopupManager, Window, WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::ping::Ping;
use smithay::reexports::calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};
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
use crate::space_element::HutSpaceElement;
use crate::graph_stack::GraphStack;

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

    /// The real, physical `Output` of whichever output currently has
    /// focus. Every ConsoleHut owns its own `Space<HutSpaceElement>`
    /// (`ConsoleHut::space`, bound to a *synthetic* output sized to its
    /// own content — composable Hut hierarchy RFC migration step 5
    /// sub-step 2) for actual window composition; this field exists for
    /// everything that only ever needs to reach the real, focused output
    /// itself (layer-shell placement, screen capture, `focused_usable_area`'s
    /// size, ...), never a window. Kept in sync with the focused
    /// `GraphStack` slot by `sync_focused_output` — real multi-monitor
    /// support means this *is* reassigned at runtime, both on a genuine
    /// focus change to a different output and on reconnect (`GraphStack::set_output`
    /// swaps a slot's own `Output` handle in place, e.g. a single-output
    /// machine's one connector being unplugged and replugged). `None`
    /// only before the first output exists.
    pub output: Option<Output>,
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
    /// (`handlers/compositor.rs`), which pushes the scale of whichever
    /// output the surface's own owning Hut resolves to (real multi-
    /// monitor: not necessarily the focused one) the moment it asks for
    /// one. No output ever changes scale mid-session (see
    /// `output_scale_for()`'s doc comment), so unlike anvil's
    /// `post_repaint`, there's no ongoing per-frame re-push loop needed
    /// here: nothing could ever change between one push and the next.
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
    /// Every physical keyboard device currently plugged in, under the real
    /// udev/DRM backend only (always empty under winit — a nested window
    /// has no hardware LEDs to update) — populated/pruned from
    /// `InputEvent::DeviceAdded`/`DeviceRemoved` in `udev_backend.rs`'s
    /// libinput event source, and iterated by `led_state_changed`
    /// (`handlers/mod.rs`) to keep each one's Caps/Num/Scroll Lock LED in
    /// sync with the seat's own xkb state. Mirrors `anvil`'s own
    /// `udev.rs` `keyboards` field/pattern exactly.
    pub keyboards: Vec<smithay::reexports::input::Device>,

    pub stack: GraphStack,
    pub keymap: Keymap,
    pub theme: crate::theme::Theme,
    pub display_config: crate::display_config::DisplayConfig,
    pub chrome_config: crate::chrome_config::ChromeConfig,
    pub perf_config: crate::perf_config::PerfConfig,
    /// Whether the combined Hut-level + Main-Window tab strip is
    /// currently revealed — only meaningful while
    /// `chrome_config.auto_hide_tab_strip` is on, and only ever true for
    /// whichever output currently has the pointer (`self.stack.
    /// focused_output_index()` — real multi-monitor's focus-follows-mouse
    /// policy means that's always the output the pointer is actually
    /// over, see `handle_pointer_motion`'s own doc comment). Updated from
    /// `input.rs`'s `update_tab_strip_reveal`, read from `render.rs`'s
    /// `build_frame_elements` (gates whether the strip draws at all) and
    /// `input.rs`'s `try_click_chrome` (gates whether it's clickable) —
    /// kept as one plain `bool` rather than a `Signal` since it's checked
    /// on every single pointer motion event regardless of whether it
    /// actually flips, matching `pointer_location`'s own "plain field,
    /// manual `request_redraw()`" treatment for the same reason.
    pub tab_strip_revealed: bool,
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

    /// The id of whichever Hut currently owns an in-progress live drag —
    /// either `dock_drag` above, or a real `grabs.rs::MoveSurfaceGrab`
    /// (which Smithay owns once `pointer.set_grab()` is called, so
    /// `State` has no other way to see it's active). Set at drag-start
    /// (`docks::start_drag`, `handlers/xdg_shell.rs::move_request`),
    /// cleared at drag-end (`docks::finish_drag`,
    /// `grabs::MoveSurfaceGrab::unset`). Checked by
    /// `sync_visible_main_window`/`sync_hut_space` to skip resyncing
    /// *this specific* Hut's `space` while its drag is live: a real
    /// PointerGrab doesn't block keyboard input, so a keybinding that
    /// changes what's visible (`Action::ToggleTerminal`, `TabNext`, ...)
    /// can fire on the very Hut a drag is actively writing a live,
    /// not-yet-persisted position into via `ConsoleHut::space_raw_mut` —
    /// `sync_main_window_space` unconditionally unmaps everything first,
    /// which would discard that live write mid-drag, exactly the
    /// corruption the `space()`/`space_raw_mut`/`space_mut` split
    /// elsewhere in this codebase already exists to prevent.
    ///
    /// One field shared by two independent drag mechanisms, relying on
    /// them never being live at once (a dock handle press is intercepted
    /// by `input.rs` before it can reach `move_request`; a client only
    /// issues `xdg_toplevel.move` for a window it owns, never a dock
    /// handle) — nothing in the type enforces that itself. Set *after*,
    /// not before, whatever installs the drag (`move_request`'s own
    /// comment on why: `pointer.set_grab` replacing an already-live grab
    /// synchronously calls the outgoing one's `unset()` first, which
    /// would otherwise immediately clobber a value just set for the new
    /// one).
    pub dragging_hut_id: Option<u64>,

    /// The pointer's current position, tracked here explicitly for the
    /// udev/libinput backend's relative-motion events (real mice/
    /// touchpads report deltas, not an absolute position the way a
    /// nested winit window's host compositor does) — see
    /// `input.rs`'s `InputEvent::PointerMotion` handling. Unused under
    /// the winit backend, which computes an absolute position fresh
    /// from each event instead.
    ///
    /// Genuinely [`Logical`] (scale-divided), matching every other value
    /// that flows through `pointer.motion()`/`surface_under()`/a Console
    /// Hut's own `space` — *not* physical output pixels, unlike
    /// `output_size`/`focused_usable_area()` below. `input.rs`'s
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
    /// Every output's own lock surface the locking client has mapped so
    /// far, keyed by real `Output` identity rather than output index (an
    /// index can shift out from under a stored value on hotplug — see
    /// `GraphStack::remove_output`'s own index-shift). A conforming
    /// locker calls `get_lock_surface` once per output it knows about
    /// (see this module's doc's Lifecycle section), so real multi-
    /// monitor needs one entry per output, not a single shared slot — a
    /// plain `Option` here meant every output but the last-locked one
    /// showed a blank rectangle with no interactive unlock prompt at
    /// all. No entry for a given output means "locked, but nothing to
    /// show there yet" — `render.rs`'s `build_frame_elements` still
    /// blanks that output's screen either way.
    ///
    /// Purged by real `Output` identity in `udev_backend.rs`'s
    /// `connector_disconnected` when a monitor is unplugged — nothing
    /// about `GraphStack::remove_output` itself reaches into `State`, so
    /// that's the one place responsible for keeping this from
    /// accumulating stale entries for since-disconnected outputs.
    pub lock_surfaces: Vec<(Output, LockSurface)>,
    /// Held between accepting a lock request and actually confirming it —
    /// see `handlers/session_lock.rs`'s `lock` doc comment for why that
    /// confirmation can't happen synchronously, and `udev_backend.rs`'s
    /// `render_surface`/`winit_backend.rs`'s redraw handler for where it
    /// finally gets taken and confirmed.
    pub pending_lock: Option<SessionLocker>,
    /// Every real `Output` that has successfully queued a locked frame
    /// since `pending_lock` was last set — cleared in
    /// `handlers/session_lock.rs`'s `lock`/`unlock`. `udev_backend.rs`'s
    /// `render_surface` (called once per crtc, independently, possibly
    /// several event-loop ticks apart on a multi-monitor session) only
    /// takes and confirms `pending_lock` once every currently-connected
    /// output has an entry here — without this, the very first crtc to
    /// queue a frame after `lock()` confirmed the client's `locked()`
    /// immediately, even though every other monitor was still showing
    /// pre-lock desktop content until its own crtc got around to
    /// rendering: a real content-disclosure gap for a security feature on
    /// any 2+-monitor setup.
    pub pending_lock_confirmed_outputs: Vec<Output>,
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

/// Shared body of [`State::focused_real_output_geometry`]/[`State::real_output_geometry_for`]
/// — the transform-and-scale-divide math itself doesn't care which
/// `Output` it's given.
fn real_output_geometry_for_output(output: &Output) -> Option<Rectangle<i32, Logical>> {
    let mode = output.current_mode()?;
    let size = output
        .current_transform()
        .transform_size(mode.size)
        .to_f64()
        .to_logical(output.current_scale().fractional_scale())
        .to_i32_ceil();
    Some(Rectangle::new((0, 0).into(), size))
}

/// Shared body of [`State::focused_usable_area`]/[`State::usable_area_for`]
/// — the layer-map-zone-lookup-and-physical-conversion math itself doesn't
/// care which `Output` it's given or which caller's own missing-output
/// fallback applies (see [`real_output_geometry_for_output`]'s identical
/// reasoning). Pulled out specifically so it's unit-testable against a
/// synthetic `Output` without needing a whole `State` — this exact
/// physical/logical conversion is what silently broke (fed a *physical*
/// value where `Space::map_element` required a genuinely *logical* one)
/// in the fractional-scale black-bar-under-waybar bug; see
/// `console_hut.rs`'s `sync_main_window_space` doc comment for the full
/// mechanism.
/// Bridges a genuinely-typed `Rectangle` into the bare `(i32, i32, i32, i32)`
/// tuples `GraphStack`'s own internal geometry API (`leaf_absolute_rect`,
/// `active_pane_offset`, `pane_rects`, ...) still uses — that API is
/// self-consistently *always* physical-pixel-space internally (its own
/// `TileNode`/`ConsoleNode` callers never cross it with a genuinely
/// `Logical` value anywhere), so it wasn't worth threading the same
/// `Physical`/`Logical` phantom-type split this file's own `usable_area*`
/// family now has all the way down into `graph_stack.rs`/`graph_nodes.rs`
/// too — the actual, previously-shipped bug this split exists to prevent
/// was specifically about *this* file's callers reaching for the wrong one
/// of two same-shaped functions, not about `GraphStack`'s single, already-
/// consistent internal convention. Named explicitly (not just `.into()`)
/// so a reader sees exactly where a genuinely-typed value deliberately
/// downgrades to an untyped one, rather than that happening invisibly.
///
/// Deliberately monomorphic to `Physical`, not generic over `Kind` — every
/// real call site must only ever feed this a physical-pixel rect (per the
/// reasoning above); a version generic over `Kind` would happily accept a
/// `Logical` rect too, silently reintroducing the exact physical/logical
/// mismatch bug class this file's own `usable_area*` split exists to
/// prevent, just one level further down the call chain (caught in
/// review). The test module below has its own tiny generic twin for
/// comparing both `Physical` and `Logical` fixtures against a tuple.
fn rect_to_tuple(rect: Rectangle<i32, Physical>) -> (i32, i32, i32, i32) {
    (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h)
}

fn usable_area_physical_for_output(output: &Output) -> Rectangle<i32, Physical> {
    let scale = output.current_scale().fractional_scale();
    let zone = layer_map_for_output(output).non_exclusive_zone();
    zone.to_physical_precise_round(scale)
}

/// Shared body of [`State::focused_usable_area_logical`]/
/// [`State::usable_area_logical_for`] — [`usable_area_physical_for_output`]'s
/// unconverted counterpart, for the same reason that one exists.
fn usable_area_logical_for_output(output: &Output) -> Rectangle<i32, Logical> {
    layer_map_for_output(output).non_exclusive_zone()
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
        stack: GraphStack,
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

        let (listening_socket, socket_name) = socket;
        Self::init_wayland_listener(display, event_loop, listening_socket)?;
        let loop_signal = event_loop.get_signal();

        // Read once, shared by all four `*Config::load()`s below —
        // see `crate::config::read_config_file`'s own doc comment for
        // why that function itself doesn't cache this.
        let config_file = crate::config::read_config_file();

        Ok(Self {
            start_time,
            socket_name,
            display_handle: dh,
            output: None,
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
            keyboards: Vec::new(),
            stack,
            keymap: Keymap::load(&config_file),
            theme: crate::theme::Theme::load(&config_file),
            display_config: crate::display_config::DisplayConfig::load(&config_file),
            chrome_config: crate::chrome_config::ChromeConfig::load(&config_file),
            perf_config: crate::perf_config::PerfConfig::load(&config_file),
            tab_strip_revealed: false,
            output_size: (0, 0),
            text_selecting: false,
            text_selection_dragged: false,
            mouse_report_button_held: None,
            dock_drag: None,
            dragging_hut_id: None,
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
            lock_surfaces: Vec::new(),
            pending_lock: None,
            pending_lock_confirmed_outputs: Vec::new(),
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
                match self.stack.remove_exited(id) {
                    // Focus can shift to a different Hut/pane here (a
                    // bare top-level removal shifts `current`, a nested
                    // removal can collapse a Tab/Tile node onto a
                    // sibling) — without this, real Wayland keyboard
                    // focus stays pointed at whatever surface it was on
                    // before the exit, silently routing every later
                    // keystroke to the wrong Hut/pane. See
                    // `GraphStack::remove_exited`'s own doc comment.
                    Ok(Some(output_index)) => {
                        let hut_id = self.stack.focused_for(output_index).id;
                        self.sync_hut_space(hut_id);
                    }
                    Ok(None) => {}
                    Err(err) => tracing::error!("failed to respawn after shell exit: {err}"),
                }
                // This id can never exist again (`ConsoleHut::id` is a
                // fresh counter per spawn) — see `render::purge_hut_content`'s
                // doc comment on the leak this avoids.
                crate::render::purge_hut_content(id);
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
    pub fn focused_output_scale(&self) -> f64 {
        self.output
            .as_ref()
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0)
    }

    /// Keeps `self.output`/`self.output_size` (deliberately still singular
    /// fields — see `GraphStack::focused_output`'s doc comment) matching
    /// whichever `OutputSlot` currently has input focus. Every one of
    /// `input.rs`'s chrome/docks/terminal-hit-testing call sites, and the
    /// handful of protocol-default-output lookups in `handlers/`, reads
    /// through these two fields rather than `self.stack.outputs()`
    /// directly — real multi-monitor's own "whatever the user is
    /// currently interacting with" concern, mirrored by every other plain
    /// (non-`_for`) accessor in this file. Call after anything that can
    /// change which output is focused: `GraphStack::set_focused_output`
    /// (focus-follows-mouse, `input.rs`), and connector connect/disconnect
    /// (`udev_backend.rs`), which can both change *which* output is
    /// focused and change that output's own `Output` handle out from
    /// under an already-focused index.
    pub fn sync_focused_output(&mut self) {
        let slot = self.stack.outputs().get(self.stack.focused_output_index());
        let new_output = slot.map(|s| s.output.clone());
        // `Output`'s own `PartialEq` (`Arc::ptr_eq` under the hood —
        // cheap, and correct even in the hypothetical case of two
        // outputs sharing the same name) — compared against `self.output`
        // (still the *old* value here, not yet overwritten below) rather
        // than tracking a separate "previous focused index" field just
        // for this.
        let output_changed = self.output != new_output;
        self.output = new_output;
        self.output_size = slot
            .and_then(|s| s.output.current_mode())
            .map(|mode| (mode.size.w, mode.size.h))
            .unwrap_or((0, 0));
        // `tab_strip_revealed` only ever means "revealed for
        // `focused_output_index()`" (see its own doc comment) — reset
        // whenever *that* genuinely changes (pointer motion crossing
        // outputs in `input.rs`, or a connector hotplug shifting it in
        // `udev_backend.rs`'s `connector_connected`/`connector_disconnected`),
        // so stale reveal state from whichever output used to be focused
        // never leaks onto the new one — one choke point instead of each
        // call site having to remember it separately. Conditioned on
        // `output_changed`, not unconditional: `connector_connected` in
        // particular calls this on *every* new connector regardless of
        // whether focus actually moved (plugging in a 2nd monitor while
        // focus stays on the 1st) — resetting unconditionally there would
        // have flickered away an already-revealed strip on the still-
        // focused output for no reason.
        if output_changed {
            self.tab_strip_revealed = false;
            // `input.rs`'s `handle_pointer_motion` (the other writer of
            // this field) always calls `request_redraw()` unconditionally
            // right around where it calls this function, so a flip there
            // never needs its own ping — but `udev_backend.rs`'s
            // `connector_connected`/`connector_disconnected` (the other
            // two callers) don't, and the udev backend's render loop is
            // purely demand-driven (see its own module doc): without
            // this, a strip that was genuinely revealed on the
            // now-unfocused output could stay on screen past the hotplug
            // event until something unrelated happened to trigger the
            // next frame (caught in review).
            self.request_redraw();
        }
    }

    /// If a lock is pending and every currently-connected output has
    /// already queued a locked frame (`self.pending_lock_confirmed_outputs`
    /// — see its own doc comment), take and confirm it. Called from
    /// `udev_backend.rs`'s `render_surface` right after adding *this*
    /// output to that list on every render pass, but also needs calling
    /// from `connector_disconnected`: unplugging the one remaining
    /// not-yet-confirmed output can make every *remaining* output already
    /// confirmed, and nothing else would ever re-check that — without
    /// this, the locking client's confirmation could stall forever
    /// waiting on an output that no longer exists, even once the screen
    /// is genuinely, fully blanked.
    pub fn confirm_pending_lock_if_ready(&mut self) {
        if !self.locked || self.pending_lock.is_none() {
            return;
        }
        let all_confirmed = self
            .stack
            .outputs()
            .iter()
            .all(|slot| self.pending_lock_confirmed_outputs.contains(&slot.output));
        if all_confirmed && let Some(confirmation) = self.pending_lock.take() {
            confirmation.lock();
        }
    }

    /// [`Self::focused_output_scale`], for a specific output — see
    /// [`Self::usable_area_for`]'s doc comment on why real multi-monitor
    /// needs this alongside the focused-output version.
    pub fn output_scale_for(&self, output_index: usize) -> f64 {
        self.stack
            .outputs()
            .get(output_index)
            .map(|slot| slot.output.current_scale().fractional_scale())
            .unwrap_or(1.0)
    }

    /// [`Self::focused_real_output_geometry`], for a specific output.
    pub fn output_size_for(&self, output_index: usize) -> (i32, i32) {
        self.stack
            .outputs()
            .get(output_index)
            .and_then(|slot| slot.output.current_mode())
            .map(|mode| (mode.size.w, mode.size.h))
            .unwrap_or((0, 0))
    }

    /// [`Self::output`]'s (the *focused* output's) geometry — always at
    /// `(0, 0)` (each output maps at its own local origin, independent
    /// of `OutputSlot::position`'s shared-compositor-space offset — see
    /// that field's own doc comment), so this is really just "the
    /// current mode's size, transformed and scale-divided" — the exact
    /// math `Space::output_geometry` itself uses (`.../desktop/space/mod.rs`'s
    /// `output_geometry`, confirmed against the pinned Smithay checkout),
    /// reproduced here now that `output` is decoupled from any
    /// particular `Space`.
    pub fn focused_real_output_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let output = self.output.as_ref()?;
        real_output_geometry_for_output(output)
    }

    /// [`Self::focused_real_output_geometry`], for a specific output rather than
    /// implicitly the focused one — same real-multi-monitor need as
    /// [`Self::usable_area_for`].
    pub fn real_output_geometry_for(&self, output_index: usize) -> Option<Rectangle<i32, Logical>> {
        let output = &self.stack.outputs().get(output_index)?.output;
        real_output_geometry_for_output(output)
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
    /// own pixel-native rendering against it. [`Self::focused_usable_area_logical`]
    /// is the *other* half: the same zone, unconverted, for the couple of
    /// call sites that configure real Wayland clients instead.
    pub fn focused_usable_area(&self) -> Rectangle<i32, Physical> {
        let Some(output) = self.output.as_ref() else {
            return Rectangle::new(Point::from((0, 0)), Size::from(self.output_size));
        };
        usable_area_physical_for_output(output)
    }

    /// [`Self::focused_usable_area`], for a *specific* output rather than
    /// implicitly the focused one — real multi-monitor's own need: a
    /// backgrounded (unfocused) monitor's own render pass still has to
    /// size/position its content against *its own* usable area, not the
    /// currently-focused monitor's. `(0, 0, 0, 0)` for an unknown index
    /// or an output with no real mode set yet (the placeholder synthetic
    /// output every `OutputSlot` starts with — see `GraphStack::new`'s
    /// doc comment) — every real caller only ever asks this for an
    /// output that's already been through `GraphStack::set_output`/
    /// `add_output` with a real connector, so this only actually matters
    /// in the same brief startup window `focused_usable_area`'s own `None`
    /// fallback already covers.
    pub fn usable_area_for(&self, output_index: usize) -> Rectangle<i32, Physical> {
        let Some(slot) = self.stack.outputs().get(output_index) else {
            return Rectangle::default();
        };
        if slot.output.current_mode().is_none() {
            return Rectangle::default();
        }
        usable_area_physical_for_output(&slot.output)
    }

    /// [`Self::focused_usable_area`]'s raw, unconverted counterpart — genuinely
    /// [`Logical`] (scale-divided), for the two call sites
    /// (`handlers/xdg_shell.rs`'s `new_toplevel`,
    /// `winit_backend.rs`'s `WinitEvent::Resized`) that build an
    /// `xdg_toplevel` configure size: that's real client-facing protocol
    /// state, which Wayland always expresses in logical coordinates
    /// regardless of how many physical pixels mudhuts itself renders a
    /// Main Window's fullscreen content into. Falls back to
    /// `self.output_size` (physical) with no output mapped yet — matches
    /// `focused_usable_area()`'s identical fallback, and only ever actually
    /// matters before the first output exists, when physical and logical
    /// aren't meaningfully different yet anyway.
    pub fn focused_usable_area_logical(&self) -> Rectangle<i32, Logical> {
        let Some(output) = self.output.as_ref() else {
            return Rectangle::new(Point::from((0, 0)), Size::from(self.output_size));
        };
        usable_area_logical_for_output(output)
    }

    /// [`Self::focused_usable_area_logical`], for a *specific* output rather than
    /// implicitly the focused one — mirrors [`Self::usable_area_for`]'s
    /// own reason for existing: `handlers/layer_shell.rs`'s
    /// `reconfigure_main_windows` must build an `xdg_toplevel` configure
    /// size against the output a change actually happened on, which is
    /// not always the currently-focused one.
    pub fn usable_area_logical_for(&self, output_index: usize) -> Rectangle<i32, Logical> {
        // `(0, 0, 0, 0)`, not `self.output_size` (the *focused* output's
        // own physical size) — unlike `focused_usable_area_logical`'s identical-
        // looking fallback (which really is about "no output exists
        // anywhere yet," so the focused one and "the" output are the
        // same concept), a bad `output_index` here means a *specific,
        // possibly different* output doesn't exist — mislabeling some
        // unrelated output's size as this one's would be actively
        // misleading, not just imprecise. Matches
        // [`Self::output_size_for`]/[`Self::usable_area_for`]'s own
        // neutral-zero fallback.
        let Some(slot) = self.stack.outputs().get(output_index) else {
            return Rectangle::default();
        };
        usable_area_logical_for_output(&slot.output)
    }

    /// The whole output's current size, genuinely [`Logical`] (scale-
    /// divided) — as opposed to `self.output_size`, which is always
    /// physical. Used wherever a physical-pixel-native value (a dragged
    /// Floating Window's position/size, read back from a Console Hut's own
    /// `space` and therefore already Logical) needs to be compared against "the
    /// output's size" in the *same* space — see `grabs.rs`'s `unset` and
    /// `docks.rs`'s `finish_drag`, both computing whether a drop point is
    /// near an edge. Falls back to `self.output_size` with no output
    /// mapped yet, same reasoning as `focused_usable_area_logical`.
    pub fn focused_output_size_logical(&self) -> Size<i32, Logical> {
        match self.focused_real_output_geometry() {
            Some(geo) => geo.size,
            None => Size::from(self.output_size),
        }
    }

    /// [`Self::focused_output_size_logical`], for a specific output — needed
    /// wherever a genuinely-Logical size has to be compared against
    /// something scoped to a *particular* output rather than whichever
    /// one currently has input focus (`grabs.rs`'s `MoveSurfaceGrab::unset`,
    /// which persists a drag against the window's real owning output,
    /// not necessarily the focused one by the time the drag ends).
    pub fn output_size_logical_for(&self, output_index: usize) -> Size<i32, Logical> {
        // `(0, 0)`, not `self.output_size` — see
        // `usable_area_logical_for`'s identical fallback fix and its own
        // doc comment for why a *specific*-output accessor can't reuse
        // `focused_output_size_logical`'s "no output exists at all yet" fallback
        // for "this particular output_index doesn't exist."
        match self.real_output_geometry_for(output_index) {
            Some(geo) => geo.size,
            None => Size::default(),
        }
    }

    /// Whether the focused ConsoleHut's terminal (vs. its active Main Window)
    /// should currently be the visible view — `ConsoleHut::showing_terminal`,
    /// but forced true when that ConsoleHut has no Main Windows to toggle to.
    pub fn focused_showing_terminal_effective(&self) -> bool {
        self.stack.shows_terminal_effective(self.stack.focused_top_level())
    }

    pub fn showing_terminal_effective_for(&self, output_index: usize) -> bool {
        self.stack.shows_terminal_effective(self.stack.focused_top_level_for(output_index))
    }

    /// Screen-space offset of whichever pane currently has effective
    /// focus — always at least [`Self::focused_usable_area`]'s own origin (`(0.0,
    /// 0.0)` unless a layer-shell surface reserves part of the output),
    /// plus the Tile-Hut pane offset on top of that if the focused
    /// top-level Hut is one (see `render.rs`'s Tile-Hut
    /// compositing, which places each pane at exactly this same offset)
    /// — so mouse interaction (selection, click, scroll) lines up with
    /// the pane that's actually on screen there, rather than being
    /// computed against the raw output as if the focused ConsoleHut's terminal
    /// still filled it edge to edge.
    pub fn active_pane_offset(&self) -> (f64, f64) {
        let area = self.focused_usable_area();
        self.stack
            .active_pane_offset(self.stack.focused_top_level(), rect_to_tuple(area))
    }

    /// `root`'s absolute physical-pixel rect right now, if it's a Main
    /// Window, Floating Window, or Alert currently on screen — composable
    /// Hut hierarchy RFC's Open Question 3 resolution, generalizing
    /// [`Self::active_pane_offset`] (which only ever answers for whichever
    /// pane is focused) to an arbitrary target surface, for
    /// `handlers/xdg_shell.rs`'s `unconstrain_popup` to anchor a popup to
    /// its actual root window instead of assuming every Main Window fills
    /// the whole output. A Floating Window/Alert root resolves via
    /// [`crate::console_hut::ConsoleHut::floating_or_alert_absolute_rect`]'s
    /// own tracked position, not this rect's own `area` — see
    /// `GraphStack::leaf_absolute_rect`'s own doc comment. `None` for a
    /// Main Window that's currently backgrounded or behind an inactive
    /// Hut-tab, or a Floating Window/Alert whose own owning Main Window
    /// entry isn't the active one — only the *focused* top-level Stack
    /// entry's *active* content is ever actually on screen, matching
    /// every other render/hit-test call site's scope.
    pub fn focused_leaf_absolute_rect(&self, root: &WlSurface) -> Option<(i32, i32, i32, i32)> {
        self.stack.leaf_absolute_rect(
            self.stack.focused_top_level(),
            root,
            rect_to_tuple(self.focused_usable_area()),
        )
    }

    /// [`Self::focused_leaf_absolute_rect`], for a specific output rather than
    /// whichever one currently has input focus — `unconstrain_popup`'s
    /// popup root doesn't have to be on the focused output, and the
    /// focused-only version's `self.stack.focused_top_level()` would walk
    /// the wrong output's own subtree entirely for one that isn't,
    /// always missing (returning `None`, falling back to the coarser
    /// whole-output rect) even for the very common "root is a plain
    /// ConsoleHut's own Main Window" case this otherwise resolves
    /// precisely.
    pub fn leaf_absolute_rect_for(&self, output_index: usize, root: &WlSurface) -> Option<(i32, i32, i32, i32)> {
        self.stack.leaf_absolute_rect(
            self.stack.focused_top_level_for(output_index),
            root,
            rect_to_tuple(self.usable_area_for(output_index)),
        )
    }

    /// Make the focused ConsoleHut's own `space` match what it should
    /// currently be showing: unmap whatever's mapped (harmless if nothing
    /// was), then map its active Main Window (if it isn't showing its
    /// terminal) plus every currently-floating Floating Window and every Alert
    /// belonging to that Main Window — docked Floating Windows stay unmapped
    /// (`docks.rs` draws a handle instead), Alerts are mapped last so they
    /// end up on top. Call after anything that could change which ConsoleHut/tab
    /// is focused or which view a ConsoleHut is showing (Alt-Tab commit,
    /// `ToggleTerminal`, `TabNext`/`TabPrev`, a new toplevel auto-switching
    /// in, a toplevel closing).
    ///
    /// Composable Hut hierarchy RFC migration step 5 sub-step 2: this used
    /// to rebuild the single global `state.space`; now rebuilds whichever
    /// ConsoleHut is focused's own `space` instead. A backgrounded Hut's
    /// `space` is otherwise left as it was here — `render.rs`'s Alt-Tab-
    /// popup thumbnail refresh (`refresh_hut_content_thumbnail`) is the
    /// *other* caller of the underlying `ConsoleHut::sync_main_window_space`,
    /// syncing a background entry's own `space` only while its thumbnail
    /// is actually about to be shown.
    ///
    /// Also resyncs keyboard focus (`input.rs`'s `sync_keyboard_focus_to_view`)
    /// every time, not left as a separate call the caller has to remember
    /// to pair this with — across several rounds of review, real call
    /// sites kept turning up that called this but not that, leaving
    /// keyboard input going to a now-hidden surface. Folded in instead of
    /// documented as a convention: `sync_keyboard_focus_to_view` only
    /// ever touches the *focused* Hut's own keyboard focus regardless of
    /// which Hut this call is for, so it's a safe no-op whenever the
    /// visible view it would resync to hasn't actually changed.
    pub fn sync_visible_main_window(&mut self) {
        // Skip the resync itself (but still fall through to the keyboard-
        // focus resync below) if the focused Hut is the one a live drag
        // is currently writing into — see `State::dragging_hut_id`'s own
        // doc comment. `sync_main_window_space` unconditionally unmaps
        // every element first, which would discard that drag's live,
        // not-yet-persisted `space_raw_mut` write; a keybinding that
        // reaches this function (`Action::ToggleTerminal`, `TabNext`/
        // `TabPrev`, ...) doesn't stop working just because the pointer
        // also has a grab active.
        if self.dragging_hut_id != Some(self.stack.focused().id) {
            // `focused_usable_area_logical`, not `focused_usable_area` —
            // `ConsoleHut::sync_main_window_space` requires a genuinely
            // `Point<i32, Logical>` origin (it maps straight into a real
            // `Space<HutSpaceElement>` via `Space::map_element`, which
            // itself requires one — Smithay's pinned source:
            // `P: Into<Point<i32, Logical>>`). Passing the *physical*-
            // pixel origin used to type-check fine anyway, back when both
            // functions returned a bare `(i32, i32)` tuple (nothing
            // caught the mismatch), and doubled up with the real output's
            // own scale the next time this position got converted back to
            // physical for rendering (`render.rs`'s
            // `content_pieces_to_elements`, which treats
            // `ContentPiece::Window`'s position as genuinely Logical and
            // multiplies by scale) — invisible at scale 1.0, but at any
            // other scale silently shifted a Main Window down/right by
            // roughly one *extra* copy of whatever's reserving space at
            // the output's origin. Now a real, previously-shipped bug a
            // *type* error, not just a doc comment away from recurring:
            // `focused_usable_area()` (physical) returns
            // `Rectangle<i32, Physical>`, so passing *its* `.loc` here
            // wouldn't compile at all.
            //
            // Computed before taking `hut`'s mutable borrow below —
            // `focused_usable_area_logical` needs `&self` as a whole,
            // which the borrow checker won't allow alongside an active
            // `&mut self.stack` borrow.
            let area = self.focused_usable_area_logical();
            self.stack.focused_mut().sync_main_window_space(area.loc);
        }
        self.sync_keyboard_focus_to_view();
    }

    /// [`Self::sync_visible_main_window`], for a specific Hut rather than
    /// implicitly the focused one — a no-op if `hut_id` no longer exists
    /// (its shell exited) or its output can't be resolved. Needed
    /// anywhere a Hut other than the focused one just had its window
    /// model mutated (`handlers/shell.rs`'s `retag`,
    /// `handlers/xdg_shell.rs`'s `toplevel_destroyed`, both of which
    /// search *every* output's own Huts, not just the focused output's):
    /// `sync_visible_main_window` alone would rebuild the wrong Hut's
    /// `space`, leaving the one that actually changed un-remapped. Also
    /// resyncs keyboard focus, same as `sync_visible_main_window` — see
    /// its own doc comment.
    pub fn sync_hut_space(&mut self, hut_id: u64) {
        let Some(output_index) = self.stack.output_index_for_hut(hut_id) else {
            return;
        };
        // See `sync_visible_main_window`'s own doc comment on why a
        // currently-dragging Hut skips the resync itself but still falls
        // through to the keyboard-focus resync below.
        if self.dragging_hut_id != Some(hut_id) {
            // `usable_area_logical_for`, not `usable_area_for` — see
            // `sync_visible_main_window`'s own doc comment for why
            // `sync_main_window_space` needs a genuinely
            // `Point<i32, Logical>` origin here.
            let area = self.usable_area_logical_for(output_index);
            if let Some(hut) = self.stack.find_mut(hut_id) {
                hut.sync_main_window_space(area.loc);
            }
        }
        self.sync_keyboard_focus_to_view();
    }

    /// Find a window (Main Window, Floating Window, or Alert) by its surface
    /// across *every* ConsoleHut, not just whatever's currently visible in
    /// the focused one's own `space` — a background ConsoleHut's windows
    /// still need commit/configure handling while hidden, and so do docked
    /// Floating Windows that aren't mapped at all.
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

    /// Which output the `ConsoleHut` owning `surface`'s window lives on —
    /// for `unconstrain_popup`, whose parent window doesn't have to be
    /// the focused Hut's (a popup can be opened from a backgrounded
    /// monitor's own window), so its output-sized/-scaled fallback needs
    /// that specific output, not `self.output`. `None` if no Hut owns a
    /// window matching `surface` (mirrors [`Self::find_window_by_surface`]'s
    /// own "not found" case).
    pub fn output_index_for_window_surface(&self, surface: &WlSurface) -> Option<usize> {
        let hut_id = self.stack.all_huts().find_map(|h| {
            let owns = h.main_windows().iter().any(|entry| {
                entry.matches(surface)
                    || entry.floating_windows.iter().any(|s| s.matches(surface))
                    || entry.alerts.iter().any(|a| a.matches(surface))
            });
            owns.then_some(h.id)
        })?;
        self.stack.output_index_for_hut(hut_id)
    }

    /// Topmost surface under `pos`, for pointer motion/click routing —
    /// checked in the same front-to-back z-order `render.rs`'s
    /// `build_frame_elements` actually draws: a `wlr-layer-shell` Top or
    /// Overlay surface first (drawn above normal content — see
    /// `composite_normal_content`'s doc comment), then whatever's mapped in
    /// the focused ConsoleHut's own `space` (its visible Main Window/
    /// Floating Windows/Alerts), then a Bottom or Background layer surface
    /// last. Doesn't need to special-case the terminal-visible branch:
    /// while the terminal itself is showing, it occupies the whole usable
    /// area and nothing in that `space` is mapped there for it to compete
    /// with, so a layer surface only ever gets picked up here when there's
    /// genuinely nothing else on top of it at that point.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        Point<f64, Logical>,
    )> {
        let output = self.output.as_ref();

        if let Some(output) = output {
            let layers = layer_map_for_output(output);
            if let Some(hit) = layer_surface_under(&layers, pos, true)
                .and_then(|layer| under_layer(&layers, layer, pos))
            {
                return Some(hit);
            }
        }

        // `space()`, deliberately NOT the self-syncing `space_mut` —
        // `docks.rs`'s `advance_drag`/`grabs.rs`'s `MoveSurfaceGrab::motion`
        // write a live, in-progress drag position directly into this
        // same focused Hut's `space` via `space_raw_mut` earlier in the
        // very same `handle_pointer_motion` call (`input.rs`) that calls
        // this. A forced sync here — caught by review before landing —
        // would rebuild from the still-stale pre-drag model and
        // immediately discard that live write before a frame ever
        // renders it, breaking dragging entirely. Reading raw also
        // happens to be the *more correct* behavior anyway: hit-testing
        // during a drag should see the window where it currently,
        // visibly is, not a resynced model position.
        if let Some(hit) = self
            .stack
            .focused()
            .space()
            .element_under(pos)
            .and_then(|(element, location)| match element {
                HutSpaceElement::Window(window) => window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64())),
                // No underlying surface to click into — nothing maps a
                // `Composited` element into this Space yet (that starts
                // once a Hut-tree node other than a bare ConsoleHut can be
                // this Hut's own child — see the RFC's later sub-steps).
                HutSpaceElement::Composited(_) => None,
            })
        {
            return Some(hit);
        }

        let output = output?;
        let layers = layer_map_for_output(output);
        layer_surface_under(&layers, pos, false).and_then(|layer| under_layer(&layers, layer, pos))
    }
}

/// Find whichever `wlr-layer-shell` surface (if any) is under `pos`,
/// restricted to either the "above normal content" half (Top + Overlay,
/// `above = true`) or the "below" half (Background + Bottom,
/// `above = false`) — the same split `render.rs`'s layer-shell compositing
/// already uses. Composable Hut hierarchy RFC migration step 5 sub-step 4
/// (Q2)'s hit-test consolidation: this exact upper/lower split used to be
/// re-derived independently by `State::surface_under` and
/// `input.rs::try_click_layer_surface`, one for spatial hit-testing, one
/// for click routing — now shared by both, alongside [`under_layer`] for
/// the "resolve the actual surface within that layer" half each of them
/// also used to duplicate.
pub(crate) fn layer_surface_under(
    layers: &smithay::desktop::LayerMap,
    pos: Point<f64, Logical>,
    above: bool,
) -> Option<&smithay::desktop::LayerSurface> {
    if above {
        layers
            .layer_under(WlrLayer::Overlay, pos)
            .or_else(|| layers.layer_under(WlrLayer::Top, pos))
    } else {
        layers
            .layer_under(WlrLayer::Bottom, pos)
            .or_else(|| layers.layer_under(WlrLayer::Background, pos))
    }
}

/// Shared tail of a layer-surface hit test: resolve `layer`'s own surface
/// (or a subsurface/popup of it) under `pos`, given `pos` is still in
/// output-relative coordinates (not yet offset by the layer's own
/// on-screen location).
pub(crate) fn under_layer(
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

/// Data associated with a wayland client that connects to mudhuts.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

#[cfg(test)]
mod tests {
    use crate::space_element::synthetic_output;

    use super::{real_output_geometry_for_output, usable_area_logical_for_output, usable_area_physical_for_output};
    use smithay::utils::Rectangle;

    // Regression coverage for the class of bug behind the "black bar under
    // waybar" fix (commit `4291e55`): a *physical*-pixel value silently fed
    // into an API that required a genuinely *logical* one, invisible at
    // scale 1.0 and only surfacing as a doubled offset/size at real
    // (non-1.0) scale. These exercise the two pure conversion functions the
    // whole `usable_area*`/`real_output_geometry*` family now shares
    // (`usable_area_physical_for_output`/`usable_area_logical_for_output`/
    // `real_output_geometry_for_output`) directly, without needing a whole
    // `State` — so a future accidental swap of which one a call site reaches
    // for gets caught here instead of requiring a live HiDPI rebuild+retest.
    //
    // Test-only twin of the production `rect_to_tuple` (deliberately
    // monomorphic to `Physical` — see its own doc comment): these
    // assertions need to compare both `Physical` and `Logical` fixtures
    // against a plain tuple, which the production function must not accept.
    fn tuple<Kind>(rect: Rectangle<i32, Kind>) -> (i32, i32, i32, i32) {
        (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h)
    }

    #[test]
    fn physical_and_logical_usable_area_agree_at_scale_one() {
        let output = synthetic_output("test", (1920, 1080), 1.0);
        assert_eq!(tuple(usable_area_physical_for_output(&output)), (0, 0, 1920, 1080));
        assert_eq!(tuple(usable_area_logical_for_output(&output)), (0, 0, 1920, 1080));
    }

    #[test]
    fn physical_usable_area_is_the_real_pixel_mode_size_at_fractional_scale() {
        // A real HiDPI setup (this codebase's own `ilama` host): the
        // output's *mode* is already expressed in real physical pixels —
        // `usable_area_physical_for_output` must report that size back
        // unchanged (modulo an as-yet-unreserved exclusive zone, i.e. none
        // here), not divide it down by `scale` a second time.
        let output = synthetic_output("test", (3840, 2160), 2.0);
        assert_eq!(tuple(usable_area_physical_for_output(&output)), (0, 0, 3840, 2160));
    }

    #[test]
    fn logical_usable_area_is_the_physical_size_divided_by_scale() {
        // The exact invariant the waybar bug violated: logical must be
        // physical / scale, not physical fed through unconverted (which is
        // what `sync_main_window_space` used to silently receive, doubling
        // every downstream `to_physical` conversion at scale 2.0).
        let output = synthetic_output("test", (3840, 2160), 2.0);
        assert_eq!(tuple(usable_area_logical_for_output(&output)), (0, 0, 1920, 1080));
    }

    #[test]
    fn real_output_geometry_is_also_scale_divided_not_raw_physical() {
        let output = synthetic_output("test", (3840, 2160), 2.0);
        let geo = real_output_geometry_for_output(&output).expect("mode was set");
        assert_eq!(tuple(geo), (0, 0, 1920, 1080));
    }

    #[test]
    fn odd_fractional_scale_stays_consistent_between_physical_and_logical() {
        // A non-integer scale (1.5, e.g. a 2880px-wide HiDPI panel at
        // 150%) is the case most likely to silently break again via a
        // rounding-direction mismatch between the physical and logical
        // variants — assert they're still each other's inverse under
        // rounding, not just for the clean *2/ 2 case above.
        let output = synthetic_output("test", (2880, 1620), 1.5);
        assert_eq!(tuple(usable_area_physical_for_output(&output)), (0, 0, 2880, 1620));
        assert_eq!(tuple(usable_area_logical_for_output(&output)), (0, 0, 1920, 1080));
    }
}
