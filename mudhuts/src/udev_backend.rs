//! Real seat/DRM backend (Phase 7): runs mudhuts directly on a TTY via
//! `libseat` (session/seat management), `udev` (GPU/connector discovery),
//! DRM/KMS (real modesetting + page-flipping), GBM (buffer allocation),
//! and `libinput` (real hardware input) — no host compositor underneath.
//! Mirrors `winit_backend.rs`'s role as a single self-contained module
//! owning its own calloop sources, closures over `&mut State`.
//!
//! Scoped to a single GPU, single seat, single output for v1 (matches
//! mudhuts' existing single-output assumptions throughout — see the
//! plan's Phase 7 notes for the full list of what's deliberately
//! deferred: multi-GPU `GpuManager`/`MultiRenderer`, explicit GPU sync
//! via drm-syncobj, 10-bit color, dmabuf client-buffer import). Cursor
//! rendering (`cursor.rs`) and DRM leasing (`drm-lease-v1`, below) are no
//! longer on that list — implemented once real GUI clients confirmed the
//! rest of the backend actually worked / once VR use was actually on the
//! table.
//!
//! ## DRM leasing (`wp_drm_lease_v1`)
//!
//! Lets a client (Monado, SteamVR, ...) take exclusive KMS control of a
//! connector away from this compositor — the standard way a VR runtime
//! gets a direct, low-latency path to a headset's display rather than
//! going through normal desktop compositing. A connector is classified
//! as leasable, instead of becoming a normal desktop `Output`, in
//! `connector_connected` using the kernel's own "non-desktop" DRM
//! property (the same signal wlroots/KWin/cosmic-comp use — driver-set,
//! usually derived from EDID) *plus* a defense-in-depth denylist on
//! `connector::Interface` (eDP/LVDS/DSI/DPI — the "always internal
//! panel" interface types): `non-desktop` alone depends on driver/EDID
//! correctness, and the built-in Apple Silicon panel must never become
//! leasable even if a driver ever mis-reports it. Real VR headsets set
//! `non-desktop` deliberately to get leased by exactly this class of
//! compositor, so the extra denylist doesn't break real leasing — it
//! only ever excludes the one connector that must never be touched.
//!
//! Setting up the `wp_drm_lease_device_v1` global (`DrmLeaseState::new`)
//! is non-fatal on failure (logged, not propagated) — see `init_udev`:
//! DRM leasing is optional, the desktop output must always come up
//! regardless of whether this device supports/permits it. Once up, the
//! actual per-request accept/reject/build logic lives in this module's
//! `DrmLeaseHandler` impl below, following anvil's reference behavior
//! (`anvil/src/udev.rs`) adapted to mudhuts' single-GPU/single-node
//! shape (no `HashMap<DrmNode, BackendData>` indirection needed — there
//! is only ever the one `node`/`DrmNode` this whole module manages).
//! `DrmLeaseState` itself lives directly on `State` (`drm_leasing_global`),
//! not inside this module's private `Inner` — `DrmLeaseHandler::
//! drm_lease_state` has to hand back a `&mut DrmLeaseState` tied to
//! `&mut self`, which isn't possible for anything reached through
//! `Rc<RefCell<_>>` (a `RefMut`'s borrow can't outlive the temporary that
//! produced it); everything else this protocol needs (the DRM device
//! handle, the non-desktop connector list, the active-lease list) has no
//! such constraint and stays in `Inner` as normal, reached via
//! `State::udev_inner` the same way `dmabuf_renderer` reaches the
//! renderer.
//!
//! Rendering is demand-driven, same principle as `winit_backend.rs`'s use
//! of `redraw_ping`/`request_redraw()`: nothing here polls on a timer to
//! *drive rendering*. `redraw_ping_source` fires `render_surface` for
//! every known crtc whenever shared code calls `State::request_redraw()`
//! (PTY output, a keypress, a client commit, etc.) — but a render attempt
//! only actually submits a new atomic commit if `render_frame` finds real
//! damage *and* no previous commit for that crtc is still in flight
//! (`SurfaceData::frame_pending`); a ping that arrives mid-flight is a
//! no-op, since the already-queued commit's eventual `DrmEvent::VBlank`
//! will call `frame_finish` -> `render_surface` again once it lands,
//! picking up anything that changed in the meantime. This avoids two
//! failure modes of a naive "just resubmit immediately" approach: tearing
//! (a new buffer handed to the display mid-scanout, before the previous
//! one finished presenting) and submitting a second atomic commit before
//! the first one's page-flip event has been processed, which the kernel/
//! `DrmCompositor` doesn't support. It also means this backend is VRR-
//! transparent for free: a new frame goes out as soon as one is both
//! ready and safe to submit, gated only by real vblank completion, not by
//! a fixed wall-clock interval — if a given connector is genuinely
//! VRR-capable, the kernel decides how soon that next vblank actually is,
//! and nothing about this render-triggering logic needs to change either
//! way.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::{GbmFramebufferExporter, NodeFilter};
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::input::pointer::{CursorIcon, CursorImageAttributes, CursorImageStatus};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Scale as OutputScale};
use smithay::reexports::calloop::ping::PingSource;
use smithay::reexports::calloop::{EventLoop, LoopHandle};
use smithay::reexports::drm::control::{Device as _, ModeTypeFlags, connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_manager_v1::{
    self, ZwlrGammaControlManagerV1,
};
use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_v1::{
    self, ZwlrGammaControlV1,
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, New};
use smithay::utils::{DeviceFd, IsAlive, Point, Scale, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
use smithay::wayland::drm_lease::{
    DrmLease, DrmLeaseBuilder, DrmLeaseHandler, DrmLeaseRequest, DrmLeaseState, LeaseRejected,
};
use smithay::wayland::{Dispatch2, GlobalData, GlobalDispatch2};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::State;
use crate::cursor::{Cursor, PointerElement};
use crate::render::{self, OutputRenderElements};
use crate::space_element::HutSpaceRenderElement;

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type OutputManager = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type CrtcOutput = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;
type Elements = OutputRenderElements<GlesRenderer, HutSpaceRenderElement>;

struct SurfaceData {
    output: Output,
    /// This output's own `wl_output` global — kept so
    /// `connector_disconnected` can call `DisplayHandle::remove_global`
    /// on hotplug-disconnect. `Output::create_global` returning a real,
    /// meaningful `GlobalId` (rather than nothing worth keeping) is
    /// itself the signal that this global has to be explicitly torn
    /// down somewhere; nothing about `Output`/`Global` does that
    /// automatically on drop.
    global: smithay::reexports::wayland_server::backend::GlobalId,
    /// Which `GraphStack::outputs()` slot this crtc drives — real
    /// multi-monitor: `render_surface` needs this to resolve/build this
    /// crtc's own content, not necessarily whichever output currently has
    /// input focus. Kept in sync with `GraphStack::remove_output`'s own
    /// index-shift by `connector_disconnected` (see its own comment).
    output_index: usize,
    drm_output: CrtcOutput,
    /// Whether a submitted atomic commit for this crtc is still waiting
    /// on its `DrmEvent::VBlank` completion. Gates `render_surface`: a
    /// second commit can't safely go out before the first one's
    /// page-flip event has been processed (see the module doc), so a
    /// `redraw_ping` that arrives mid-flight is dropped here rather than
    /// attempted — `frame_finish` re-renders once the pending commit
    /// actually completes, picking up anything that changed meanwhile.
    frame_pending: bool,
    /// Whether some client currently holds an active
    /// `zwlr_gamma_control_v1` for this crtc's output — the protocol
    /// grants at most one client exclusive gamma access per output, so a
    /// second `get_gamma_control` while this is set gets an immediate
    /// `.failed()` rather than displacing the first (see
    /// `get_gamma_control`).
    gamma_control_bound: bool,
    /// This crtc's persistent scratch buffer for
    /// `render::build_frame_elements`'s output — `clear()`ed (inside that
    /// function itself) and rebuilt every `render_surface` call rather
    /// than allocated fresh, so a steady-state frame (same rough element
    /// count as last frame) settles into reusing one already-grown
    /// allocation instead of round-tripping the allocator every frame,
    /// same reasoning as `GpuTermRenderer::instances_scratch`.
    elements: Vec<Elements>,
}

/// `pub(crate)` (not module-private) solely so `state.rs` can name this
/// type for `State::udev_inner`'s `Rc<RefCell<Inner>>` — see this
/// module's doc and `dmabuf_renderer`'s identical precedent. Every field
/// stays private; nothing outside this module ever reaches inside.
/// `wlr-gamma-control-unstable-v1`'s `Dispatch2` impls below reach
/// `drm_output_manager`/`surfaces` through the same handle (via
/// `state: &mut State`) rather than stashing their own clone in Wayland
/// resource user-data: `wayland-server` requires resource user-data to be
/// `Send + Sync` (see `ObjectData`'s `DowncastSync` supertrait), which
/// `Rc<RefCell<_>>` deliberately never is — this compositor has no
/// multi-threaded dispatch to justify making it so.
pub(crate) struct Inner {
    /// `Rc<RefCell<_>>`, not owned outright — `State::dmabuf_renderer`
    /// holds a clone of the same renderer so `DmabufHandler::dmabuf_imported`
    /// (`handlers/mod.rs`) can attempt a client buffer import, since
    /// `State` otherwise has no renderer of its own (that's normally
    /// backend-private state — see this module's doc).
    renderer: Rc<RefCell<GlesRenderer>>,
    drm_output_manager: OutputManager,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// The loaded Xcursor theme images (see `cursor.rs`), one `Cursor` per
    /// distinct shape a client has actually requested so far (via
    /// `wl_pointer.set_cursor`'s `CursorImageStatus::default_named()` or,
    /// now, `cursor-shape-v1`'s `set_shape`) — lazily populated in
    /// `build_cursor_elements` rather than eagerly loading every
    /// `CursorIcon` variant up front, since most sessions only ever touch
    /// a handful (`Default`, `Text`, `Pointer`, ...). Plus the last-seen
    /// `CursorImageStatus`'s composited render state, which persists
    /// across frames rather than being rebuilt each render pass.
    pointer_images: HashMap<CursorIcon, Cursor>,
    pointer_element: PointerElement,
    /// Caches one uploaded `MemoryRenderBuffer` per distinct xcursor
    /// frame `Image` (an animated cursor theme has only a handful of
    /// these) — without this, every single render pass would re-upload
    /// the same pixel data to a fresh GPU texture even when the cursor's
    /// visible frame hasn't changed since the last one.
    pointer_image_cache: Vec<((CursorIcon, usize, i32), MemoryRenderBuffer)>,
    /// Connectors currently classified as leasable (see the module doc's
    /// DRM-leasing section) together with the CRTC `rescan_connectors`
    /// found for them. Never gets a desktop `Output`/`SurfaceData` entry
    /// at all — tracked here purely so `DrmLeaseHandler::lease_request`
    /// (below) knows which CRTC a client-requested connector maps to.
    non_desktop_connectors: Vec<(connector::Handle, crtc::Handle)>,
    /// Leases currently handed out to clients. Dropping an entry revokes
    /// it (see `DrmLease`'s own `Drop` impl) — populated by
    /// `DrmLeaseHandler::new_active_lease`, drained one at a time by
    /// `lease_destroyed`, and drained wholesale on session pause (VT
    /// switch away), matching anvil's own hard-revoke-on-pause behavior
    /// rather than trying to preserve a lease through a switch that's
    /// about to take DRM master away regardless.
    active_leases: Vec<DrmLease>,
}

pub fn init_udev(
    event_loop: &mut EventLoop<'static, State>,
    state: &mut State,
    redraw_ping_source: PingSource,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut session, notifier) =
        LibSeatSession::new().map_err(|err| format!("failed to initialize a session: {err}"))?;
    let seat_name = session.seat();

    // Candidates to try, in order. `smithay::backend::udev::primary_gpu`'s
    // own heuristic deliberately isn't used here: its 2nd-priority rule
    // ("prefer whichever GPU has an associated render node") is aimed at
    // typical PC dGPU/iGPU splits, but backfires on hardware where the
    // display controller and the GPU core are altogether separate DRM
    // nodes (e.g. Apple Silicon: the display controller has real
    // connectors but *no* render node, while the GPU core has a render
    // node but no modesetting/connector capability at all — exactly
    // backwards from what that heuristic assumes). Since this backend
    // doesn't split "render GPU" from "display GPU" in the first place
    // (single plain `GlesRenderer`, see the module doc), what actually
    // matters is which node can do real modesetting — so each candidate
    // is tried for real (open + `DrmDevice::new`, which itself fails if
    // resource-handle enumeration isn't supported) rather than guessed.
    let candidates: Vec<PathBuf> = match std::env::var_os("MUDHUTS_DRM_DEVICE") {
        Some(path) => vec![PathBuf::from(path)],
        None => udev::all_gpus(&seat_name).map_err(|err| format!("failed to list GPUs: {err}"))?,
    };
    if candidates.is_empty() {
        return Err("no GPU found for this seat".into());
    }

    let mut last_err = None;
    let mut opened = None;
    for path in &candidates {
        let result = session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(|err| format!("failed to open {path:?}: {err}"))
            .and_then(|fd| {
                let fd = DrmDeviceFd::new(DeviceFd::from(fd));
                // `disable_connectors: false` — `true` would force an
                // atomic commit disabling every connector immediately on
                // open, before any connector scan. That collides badly
                // with a live session handoff: the outgoing session
                // (e.g. cosmic-greeter tearing down) may still be mid-
                // teardown of its own claim on the same connector, and
                // the display controller's power-state machine here is
                // asynchronous (coprocessor round-trips taking seconds),
                // so two overlapping "disable" commands to the same
                // connector is a real way to wedge it. `false` matches
                // what actually works on this hardware (see cosmic-comp's
                // own `Device::new`, which uses `false` for exactly this
                // reason — "prevent flickering of already turned on
                // connectors").
                DrmDevice::new(fd.clone(), false)
                    .map(|(drm_device, drm_notifier)| (fd, drm_device, drm_notifier))
                    .map_err(|err| format!("{path:?} can't do modesetting: {err}"))
            });
        match result {
            Ok(triple) => {
                opened = Some((path.clone(), triple));
                break;
            }
            Err(err) => {
                tracing::debug!("{err}");
                last_err = Some(err);
            }
        }
    }
    let Some((primary_gpu_path, (fd, drm_device, drm_notifier))) = opened else {
        return Err(last_err.unwrap_or_else(|| "no usable DRM device found".to_string()).into());
    };
    let node = DrmNode::from_path(&primary_gpu_path)
        .map_err(|err| format!("{primary_gpu_path:?} is not a DRM node: {err}"))?;
    tracing::info!("using {primary_gpu_path:?} as the DRM device");

    let gbm = GbmDevice::new(fd).map_err(|err| format!("failed to initialize GBM: {err}"))?;

    // Same EGL/GBM recipe the winit backend already uses internally —
    // plain `GlesRenderer`, no multi-GPU `GpuManager` (see the module
    // doc): every render-element-building call in `gpu_term.rs`/
    // `chrome.rs`/`docks.rs`/`switcher.rs` already expects exactly this
    // concrete type, so this is what lets `render::build_frame_elements`
    // be shared unchanged between both backends.
    let egl_display =
        unsafe { EGLDisplay::new(gbm.clone()) }.map_err(|err| format!("failed to initialize EGL: {err}"))?;
    let egl_context = EGLContext::new(&egl_display)
        .map_err(|err| format!("failed to create an EGL context: {err}"))?;
    let renderer = unsafe { GlesRenderer::new(egl_context) }
        .map_err(|err| format!("failed to initialize the GL renderer: {err}"))?;

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), NodeFilter::All);
    // Matches anvil's/cosmic-comp's exact format list — channel order
    // isn't an arbitrary preference here: `DrmOutputManager` picks the
    // first entry both this list and the renderer's own reported
    // `render_formats` support, so requesting a format the display
    // controller's pixel-format converter doesn't actually accept (my
    // earlier `Argb8888`/`Xrgb8888` guess used the wrong channel order —
    // ABGR, not ARGB, is what both known-working references request)
    // could pick a working-on-paper-but-not-really format.
    let color_formats = [
        Fourcc::Abgr2101010,
        Fourcc::Argb2101010,
        Fourcc::Abgr8888,
        Fourcc::Argb8888,
    ];
    let render_formats = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied();
    let drm_output_manager =
        DrmOutputManager::new(drm_device, allocator, exporter, Some(gbm), color_formats, render_formats);

    // Client-buffer dmabuf import (`zwp_linux_dmabuf_v1`): lets clients
    // hand over a GPU buffer directly rather than a plain SHM buffer that
    // has to be copied/re-uploaded to the GPU on every commit — real
    // GPU-rendering toolkits (Qt, iced/libcosmic) submit far more
    // frequent, often fullscreen-sized commits than this compositor's
    // own terminal rendering does, making that copy a genuine, avoidable
    // cost at this output's resolution. `renderer` needs to be shared
    // (`Rc<RefCell<_>>`) from here on: `State::dmabuf_renderer` keeps its
    // own clone so `DmabufHandler::dmabuf_imported` can reach it (`State`
    // has no renderer of its own otherwise).
    let renderer = Rc::new(RefCell::new(renderer));
    state.dmabuf_renderer = Some(renderer.clone());
    // The graph's own `RenderEnv` needs the *same* `Rc<RefCell<_>>` —
    // see `graph_nodes::RenderEnv::renderer`'s own doc comment for why a
    // separately-owned second `GlesRenderer` would be a real correctness
    // bug, not just wasteful.
    state.stack.set_renderer(renderer.clone());
    let dmabuf_formats: Vec<_> = renderer.borrow().dmabuf_formats().iter().copied().collect();
    match DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats).build() {
        Ok(default_feedback) => {
            let global = state
                .dmabuf_state
                .create_global_with_default_feedback::<State>(&state.display_handle, &default_feedback);
            state.dmabuf_global = Some(global);
        }
        Err(err) => {
            tracing::warn!(
                "failed to build dmabuf feedback, client dmabuf import unavailable (falling back to SHM): {err}"
            );
        }
    }

    let inner = Rc::new(RefCell::new(Inner {
        renderer,
        drm_output_manager,
        drm_scanner: DrmScanner::new(),
        surfaces: HashMap::new(),
        pointer_images: HashMap::new(),
        pointer_element: PointerElement::default(),
        pointer_image_cache: Vec::new(),
        non_desktop_connectors: Vec::new(),
        active_leases: Vec::new(),
    }));
    // Shared with `DrmLeaseHandler`'s trait methods (`&mut self` on
    // `State`, dispatched by Smithay's own protocol code with no access
    // to anything captured in this function's closures) — same reasoning
    // as `state.dmabuf_renderer` above. `None` under `winit_backend.rs`,
    // which never calls this function at all.
    state.udev_inner = Some(inner.clone());

    // DRM leasing (`wp_drm_lease_v1`) — see the module doc. Non-fatal on
    // failure: `DrmLeaseState::new` does a one-shot open+drop-master
    // probe against this node's own device path before creating the
    // global, which can fail on an unusual permission setup — matches
    // anvil's own `.inspect_err(...).ok()` exactly. The desktop output
    // must come up regardless of whether this succeeds.
    state.drm_leasing_global = DrmLeaseState::new::<State>(&state.display_handle, &node)
        .inspect_err(|err| {
            tracing::warn!(
                "failed to initialize the drm-lease global, DRM leasing (VR headsets, etc.) will be unavailable this session: {err}"
            );
        })
        .ok();

    // `wlr-gamma-control-unstable-v1`: only ever registered here, mirroring
    // `dmabuf_global` right above — reaches `Inner` through the same
    // `state.udev_inner` clone already set up for `DrmLeaseHandler` (see
    // `Inner`'s own doc comment for why that's shared rather than stashed
    // in the resource user-data).
    state
        .display_handle
        .create_global::<State, ZwlrGammaControlManagerV1, _>(1, GlobalData);

    let loop_handle = event_loop.handle();

    // DRM device: vblank completion drives the self-perpetuating
    // page-flip loop (see the module doc).
    {
        let inner = inner.clone();
        event_loop
            .handle()
            .insert_source(drm_notifier, move |event, _, state| match event {
                DrmEvent::VBlank(crtc) => frame_finish(state, &inner, crtc),
                DrmEvent::Error(err) => tracing::warn!("DRM device error: {err}"),
            })
            .map_err(|err| format!("failed to register the DRM event source: {err}"))?;
    }

    // Demand-driven rendering: fires on every `State::request_redraw()`
    // call from shared code (PTY output, input, client commits, etc.) —
    // see the module doc for why this is safe to call unconditionally
    // for every known crtc even when nothing actually changed (a no-op
    // `render_frame` finding no damage) or when a previous commit is
    // still in flight (dropped, picked up by the next real vblank).
    {
        let inner = inner.clone();
        event_loop
            .handle()
            .insert_source(redraw_ping_source, move |(), _, state| {
                // Once per whole multi-output frame, before iterating any
                // crtc — see `GraphStack::begin_frame`'s doc comment: the
                // per-frame memoization cache spans every output resolved
                // in this pass, not just one.
                state.stack.begin_frame();
                let crtcs: Vec<_> = inner.borrow().surfaces.keys().copied().collect();
                for crtc in crtcs {
                    render_surface(state, &inner, crtc);
                }
            })
            .map_err(|err| format!("failed to register the redraw ping source: {err}"))?;
    }

    // Session pause/resume (VT switch away/back).
    {
        let inner = inner.clone();
        event_loop
            .handle()
            .insert_source(notifier, move |event, _, state| match event {
                SessionEvent::PauseSession => {
                    tracing::info!("session paused (VT switched away)");
                    inner.borrow_mut().drm_output_manager.pause();
                    // Hard-revoke every active lease rather than trying to
                    // soft-suspend it: losing the VT means DRM master
                    // itself is about to be taken away, so whatever a
                    // leased client thinks it can still do with its
                    // planes/CRTC is moot regardless — matches anvil's own
                    // `active_leases.clear()` here.
                    inner.borrow_mut().active_leases.clear();
                    if let Some(leasing_global) = state.drm_leasing_global.as_mut() {
                        leasing_global.suspend();
                    }
                }
                SessionEvent::ActivateSession => {
                    tracing::info!("session resumed (VT switched back)");
                    if let Err(err) = inner.borrow_mut().drm_output_manager.lock().activate(false) {
                        tracing::warn!("failed to reactivate the DRM device: {err}");
                    }
                    if let Some(leasing_global) = state.drm_leasing_global.as_mut() {
                        leasing_global.resume::<State>();
                    }
                    // A commit queued right as `PauseSession` hit can
                    // leave `frame_pending` stuck `true` forever: `pause()`
                    // above drops DRM master mid-flight, and once that
                    // happens the kernel has no way to ever deliver that
                    // commit's `DrmEvent::VBlank` — the only place that
                    // clears `frame_pending` (`frame_finish`). Without
                    // resetting it here, `render_surface`'s own
                    // `frame_pending` guard would then bail out on every
                    // future redraw for that crtc, leaving the output
                    // frozen until mudhuts restarts. Safe to clear
                    // unconditionally for every surface: DRM master was
                    // just reacquired above, so there is no in-flight
                    // commit left that could still complete after this.
                    for surface in inner.borrow_mut().surfaces.values_mut() {
                        surface.frame_pending = false;
                    }
                    // Nothing else re-kicks rendering after a resume —
                    // explicitly force a fresh pass on every crtc.
                    state.stack.begin_frame();
                    let crtcs: Vec<_> = inner.borrow().surfaces.keys().copied().collect();
                    for crtc in crtcs {
                        render_surface(state, &inner, crtc);
                    }
                }
            })
            .map_err(|err| format!("failed to register the session event source: {err}"))?;
    }

    // Real hardware input, routed through the same
    // `process_input_event<I: InputBackend>` every backend already
    // shares (see `input.rs`'s `InputEvent::PointerMotion` handling,
    // added specifically for this backend's relative-motion events).
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        session.clone().into(),
    );
    if libinput_context.udev_assign_seat(&seat_name).is_err() {
        return Err("failed to assign libinput to the seat".into());
    }
    let libinput_backend = LibinputInputBackend::new(libinput_context);
    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, state| {
            // Keyboard hotplug: track the physical device so
            // `led_state_changed` (`handlers/mod.rs`) can keep its real
            // Caps/Num/Scroll Lock LEDs synced — matches `anvil`'s own
            // `udev.rs` pattern. Seeded with the seat's *current* LED
            // state immediately (not just future changes), so a keyboard
            // plugged in mid-session doesn't start with stale/no LEDs.
            match &event {
                smithay::backend::input::InputEvent::DeviceAdded { device }
                    if device.has_capability(smithay::reexports::input::DeviceCapability::Keyboard) =>
                {
                    let mut device = device.clone();
                    if let Some(led_state) = state.seat.get_keyboard().map(|k| k.led_state()) {
                        device.led_update(led_state.into());
                    }
                    state.keyboards.push(device);
                }
                smithay::backend::input::InputEvent::DeviceRemoved { device } => {
                    state.keyboards.retain(|d| d != device);
                }
                _ => {}
            }
            state.process_input_event(event);
        })
        .map_err(|err| format!("failed to register the libinput event source: {err}"))?;

    // Connector hotplug on the already-chosen primary node — any other
    // GPU node being added/removed is out of scope for v1 (see the
    // module doc).
    let udev_backend = UdevBackend::new(&seat_name)
        .map_err(|err| format!("failed to initialize the udev backend: {err}"))?;
    {
        let inner = inner.clone();
        let loop_handle = loop_handle.clone();
        event_loop
            .handle()
            .insert_source(udev_backend, move |event, _, state| {
                if let UdevEvent::Changed { device_id } = event
                    && DrmNode::from_dev_id(device_id).is_ok_and(|changed| changed == node)
                {
                    rescan_connectors(state, &inner, &loop_handle);
                }
            })
            .map_err(|err| format!("failed to register the udev event source: {err}"))?;
    }

    // Initial connector scan — creates the (single, v1) Output and kicks
    // off the first render pass for it.
    rescan_connectors(state, &inner, &loop_handle);

    Ok(())
}

/// Diff the primary node's connectors since the last scan and set up/
/// tear down an `Output`/`DrmOutput` for whatever changed. Called once
/// at startup (the "initial scan") and again on every `UdevEvent::
/// Changed` for the primary node (routine hardware hotplug — a monitor
/// being plugged/unplugged — not a multi-monitor feature; see the
/// module doc).
fn rescan_connectors(state: &mut State, inner: &Rc<RefCell<Inner>>, handle: &LoopHandle<'static, State>) {
    let scan_result = {
        let mut inner_mut = inner.borrow_mut();
        let inner_mut = &mut *inner_mut;
        inner_mut
            .drm_scanner
            .scan_connectors(inner_mut.drm_output_manager.device())
    };
    let scan_result = match scan_result {
        Ok(scan_result) => scan_result,
        Err(err) => {
            tracing::warn!("failed to scan DRM connectors: {err}");
            return;
        }
    };

    for event in scan_result {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => connector_connected(state, inner, handle, connector, crtc),
            DrmScanEvent::Disconnected {
                connector,
                crtc: Some(crtc),
            } => connector_disconnected(state, inner, connector, crtc),
            _ => {}
        }
    }
}

/// Real (not hardcoded) HiDPI detection for a newly-connected desktop
/// connector: `MUDHUTS_OUTPUT_SCALE` wins if set — an escape hatch for
/// panels whose EDID physical size is wrong or absent, a real and fairly
/// common hardware quirk, not a hypothetical — otherwise a plain pixel-
/// density heuristic derived from `connector.size()`'s own reported
/// physical dimensions, using roughly the same ~192 DPI threshold most
/// desktop environments already default to for "treat this as a HiDPI
/// panel." Diagonal DPI, not width/height checked separately, so this
/// doesn't need to reconcile two different figures for a non-square
/// aspect ratio.
///
/// Deliberately coarse: rounds to a whole-number scale rather than
/// trying to guess an in-between fractional one, since 2x is
/// overwhelmingly the common real case — including this backend's own
/// motivating hardware target, Apple Silicon's built-in Retina panel.
/// Computed once at connector-connect time and never revisited: mudhuts
/// has no live-rescale mechanism (see `State::focused_output_scale`'s doc
/// comment), and a real monitor's physical size/pixel count can't change
/// without a fresh connector-connect event of its own anyway.
fn detect_output_scale(phys_size_mm: (i32, i32), pixels: (i32, i32)) -> f64 {
    if let Some(scale) = std::env::var("MUDHUTS_OUTPUT_SCALE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|scale| *scale > 0.0)
    {
        return scale;
    }

    let (phys_w_mm, phys_h_mm) = phys_size_mm;
    let (px_w, px_h) = pixels;
    if phys_w_mm <= 0 || phys_h_mm <= 0 || px_w <= 0 || px_h <= 0 {
        return 1.0;
    }

    let diag_px = ((px_w * px_w + px_h * px_h) as f64).sqrt();
    let diag_in = ((phys_w_mm * phys_w_mm + phys_h_mm * phys_h_mm) as f64).sqrt() / 25.4;
    if diag_in <= 0.0 {
        return 1.0;
    }

    let dpi = diag_px / diag_in;
    if dpi >= 192.0 { 2.0 } else { 1.0 }
}

/// Look up `connector`'s own boolean-typed DRM property named `name` —
/// `false` if the property doesn't exist, isn't boolean, or the query
/// itself fails, matching every other "no this connector doesn't have
/// this driver/EDID-derived signal" fallback in this module — currently
/// just the DRM-leasing `non-desktop` check (see the module doc's
/// leasing section), but kept generic over the property name rather than
/// hardcoded to that one since the shape (look up a bool property, fall
/// back to `false`) has already needed reuse once before.
///
/// Generic over `impl control::Device` rather than the concrete
/// `DrmDevice` every real caller actually passes (`DrmDevice` implements
/// `control::Device` in smithay itself) — widened specifically so this
/// module's own `vkms_*` test can exercise it directly against a raw
/// opened card node, without needing to stand up `DrmDevice`'s own
/// session/capability-negotiation machinery just to call a handful of
/// read-only property-enumeration methods that don't actually need any
/// of it (caught in review: an earlier version of that test skipped this
/// function entirely for exactly that reason).
fn connector_bool_property(
    drm_device: &impl smithay::reexports::drm::control::Device,
    connector: connector::Handle,
    name: &str,
) -> bool {
    drm_device
        .get_properties(connector)
        .ok()
        .and_then(|props| {
            let (info, value) = props
                .into_iter()
                .filter_map(|(handle, value)| {
                    let info = drm_device.get_property(handle).ok()?;
                    Some((info, value))
                })
                .find(|(info, _)| info.name().to_str() == Ok(name))?;
            info.value_type().convert_value(value).as_boolean()
        })
        .unwrap_or(false)
}

/// Whether a connector should be offered for DRM leasing instead of used
/// as a desktop output — `non_desktop` alone (a real DRM/EDID-reported
/// property, read by the caller via `connector_bool_property`) plus
/// defense-in-depth: the built-in panel must never be leasable regardless
/// of what a driver reports (see the module doc's DRM-leasing section).
/// Pulled out as a pure function over `connector::Interface` (a plain,
/// freely-constructible enum — not the FFI-opaque `connector::Info`/
/// `DrmMode` types this module otherwise deals with) specifically so this
/// safety-critical rule is unit-testable without a live DRM device.
fn is_leasable(non_desktop: bool, interface: connector::Interface) -> bool {
    non_desktop
        && !matches!(
            interface,
            connector::Interface::EmbeddedDisplayPort
                | connector::Interface::LVDS
                | connector::Interface::DSI
                | connector::Interface::DPI
        )
}

/// Which index into a connector's own `modes()` list is flagged
/// `ModeTypeFlags::PREFERRED` — `is_preferred[i]` is that flag for mode
/// `i`, same order `connector.modes()` itself returns. Falls back to `0`
/// if nothing is flagged (a driver quirk, not something to panic over —
/// `connector_connected`'s own caller already treats "no usable modes at
/// all" as a separate, later failure). Pulled out over a plain `&[bool]`
/// rather than `&[DrmMode]` since `DrmMode` is FFI-opaque with no public
/// test constructor, but the actual selection logic never needed
/// anything but the flag itself.
fn pick_preferred_mode_index(is_preferred: &[bool]) -> usize {
    is_preferred.iter().position(|&p| p).unwrap_or(0)
}

/// Where a newly-hotplugged output slot should sit, real multi-monitor's
/// side-by-side-to-the-right-of-everything-else layout policy (see
/// `connector_connected`'s own comment on this) — `existing` is
/// `(position.x, mode width in pixels, scale)` per already-known output
/// slot. Pulled out over these primitives rather than the real
/// `OutputSlot`/`Output` types since neither is the blocker here (both
/// are freely constructible) — the real blocker is `GraphStack`/`State`,
/// heavyweight to build just to exercise this one arithmetic fold.
fn next_output_x(existing: &[(i32, i32, f64)]) -> i32 {
    existing.iter().map(|&(x, w, scale)| x + (w as f64 / scale).round() as i32).max().unwrap_or(0)
}

fn connector_connected(
    state: &mut State,
    inner: &Rc<RefCell<Inner>>,
    handle: &LoopHandle<'static, State>,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let output_name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
    tracing::info!("setting up connector {output_name} on {crtc:?}");

    // Leasable ("non-desktop") vs. desktop connector — see the module
    // doc's DRM-leasing section. Checked before any desktop `Output`
    // setup below: a leasable connector never gets one at all.
    let non_desktop = {
        let inner_ref = inner.borrow();
        let drm_device = inner_ref.drm_output_manager.device();
        connector_bool_property(drm_device, connector.handle(), "non-desktop")
    };
    let is_leasable = is_leasable(non_desktop, connector.interface());

    if is_leasable {
        tracing::info!(
            "connector {output_name} is non-desktop, offering it for DRM leasing instead of desktop use"
        );
        let mut inner_mut = inner.borrow_mut();
        inner_mut.non_desktop_connectors.push((connector.handle(), crtc));
        if let Some(leasing_global) = state.drm_leasing_global.as_mut() {
            leasing_global.add_connector::<State>(connector.handle(), output_name.clone(), output_name);
        }
        return;
    }

    let is_preferred: Vec<bool> =
        connector.modes().iter().map(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED)).collect();
    let mode_id = pick_preferred_mode_index(&is_preferred);
    let Some(&drm_mode) = connector.modes().get(mode_id) else {
        tracing::warn!("connector {output_name} has no usable modes, skipping");
        return;
    };
    let wl_mode = WlMode::from(drm_mode);

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let scale = detect_output_scale((phys_w as i32, phys_h as i32), (wl_mode.size.w, wl_mode.size.h));
    tracing::info!("connector {output_name}: detected output scale {scale}");
    let output = Output::new(
        output_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: connector.subpixel().into(),
            make: "mudhuts".into(),
            model: output_name.clone(),
            serial_number: "unknown".into(),
        },
    );
    let global = output.create_global::<State>(&state.display_handle);
    output.set_preferred(wl_mode);
    output.change_current_state(
        Some(wl_mode),
        None,
        Some(OutputScale::Fractional(scale)),
        Some((0, 0).into()),
    );

    let drm_output = {
        let mut inner_mut = inner.borrow_mut();
        let inner_mut = &mut *inner_mut;
        let mut renderer = inner_mut.renderer.borrow_mut();
        let renderer = &mut *renderer;
        let result = inner_mut
            .drm_output_manager
            .lock()
            .initialize_output::<GlesRenderer, Elements>(
                crtc,
                drm_mode,
                &[connector.handle()],
                &output,
                None,
                renderer,
                &DrmOutputRenderElements::default(),
            );
        match result {
            Ok(drm_output) => drm_output,
            Err(err) => {
                tracing::warn!("failed to initialize DRM output for {output_name}: {err}");
                state.display_handle.remove_global::<State>(global);
                return;
            }
        }
    };

    // Real multi-monitor: the very first desktop connector reuses
    // `GraphStack::new`'s already-existing slot 0 (still holding its
    // harmless synthetic placeholder `Output` at this point — see its own
    // doc comment); every connector after that is a genuinely new
    // `OutputSlot` with its own independent stack, positioned side by side
    // to the right of every output already known, per the user's resolved
    // policy (each output starts as its own independent workspace, no
    // mirroring). `inner.surfaces` (not `state.stack.outputs()`, which
    // never shrinks below one slot — see `GraphStack::remove_output`'s doc
    // comment) is the source of truth for "is this the first real
    // connector": it's empty exactly when no crtc currently drives a real
    // desktop output.
    let output_index = {
        let inner_ref = inner.borrow();
        if inner_ref.surfaces.is_empty() {
            0
        } else {
            let existing: Vec<(i32, i32, f64)> = state
                .stack
                .outputs()
                .iter()
                .filter_map(|slot| {
                    let mode = slot.output.current_mode()?;
                    let scale = slot.output.current_scale().fractional_scale();
                    Some((slot.position.x, mode.size.w, scale))
                })
                .collect();
            let next_x = next_output_x(&existing);
            drop(inner_ref);
            let position = Point::<i32, smithay::utils::Logical>::from((next_x, 0));
            // Real position, not the `(0, 0)` `change_current_state` was
            // given above (unconditionally, before this output's real
            // place in a multi-monitor layout was known) — so clients
            // using `xdg-output`/`wl_output.geometry` to lay out
            // fullscreen/layer-shell surfaces across monitors see this
            // output's genuine side-by-side location.
            output.change_current_state(None, None, None, Some(position));
            match state.stack.add_output(output.clone(), position) {
                Ok(index) => index,
                Err(err) => {
                    tracing::warn!("failed to add output slot for {output_name}: {err}");
                    state.display_handle.remove_global::<State>(global);
                    return;
                }
            }
        }
    };
    if output_index == 0 {
        state.stack.set_output(0, output.clone());
    }
    state.sync_focused_output();
    // Catches up the initial ConsoleHut (spawned in `main.rs` before this
    // backend existed, at scale 1.0) and remembers `scale` for every ConsoleHut
    // spawned from here on — see `Stack::rescale_all`'s doc comment. Shared
    // across every output for now (`GraphStack::scale`'s own doc comment)
    // rather than per-connector, so this stays a single call even for a
    // second connector.
    if let Err(err) = state.stack.rescale_all(scale) {
        tracing::warn!("failed to rescale initial ConsoleHut to real output scale: {err}");
    }
    let usable = state.usable_area_for(output_index).size;
    state.stack.resize_output(output_index, usable.w, usable.h);
    // Nothing else re-pushes capture buffer constraints when the output's
    // mode changes (the only point that happens under this backend, since
    // it has no runtime mode-switching) — without this, a capture session
    // created against a previous connector's size would fail every later
    // capture attempt (see `State::refresh_capture_constraints`).
    state.refresh_capture_constraints();

    inner.borrow_mut().surfaces.insert(
        crtc,
        SurfaceData {
            output,
            global,
            output_index,
            drm_output,
            frame_pending: false,
            gamma_control_bound: false,
            elements: Vec::new(),
        },
    );

    // Deferred to the next event loop iteration (matches anvil's own
    // reference pattern) rather than called synchronously here, still
    // nested inside the same call stack as the modeset that
    // `initialize_output` just performed — giving the display
    // controller's own (asynchronous, coprocessor-driven) power-state
    // machine a chance to settle before another commit lands on it.
    let inner = inner.clone();
    handle.insert_idle(move |state| {
        state.stack.begin_frame();
        render_surface(state, &inner, crtc);
    });
}

fn connector_disconnected(
    state: &mut State,
    inner: &Rc<RefCell<Inner>>,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let mut inner_mut = inner.borrow_mut();
    if let Some(pos) = inner_mut
        .non_desktop_connectors
        .iter()
        .position(|(handle, _)| *handle == connector.handle())
    {
        // Was leasable, never had a desktop `Output`/`SurfaceData` entry
        // to tear down — see `connector_connected`'s classification.
        inner_mut.non_desktop_connectors.remove(pos);
        drop(inner_mut);
        if let Some(leasing_global) = state.drm_leasing_global.as_mut() {
            leasing_global.withdraw_connector(connector.handle());
        }
        return;
    }
    let removed = inner_mut.surfaces.remove(&crtc);
    drop(inner_mut);
    if let Some(surface) = removed {
        // The physical connector is really gone — remove its `wl_output`
        // global regardless of whether `GraphStack::remove_output` below
        // actually drops the logical `OutputSlot` too (it refuses to for
        // the very last one, see its own comment just below): clients
        // shouldn't be able to bind a `wl_output` for a display that no
        // longer exists. Without this, every hotplug-disconnect leaked
        // one global (`create_global`'s `GlobalId` was previously
        // discarded entirely, with nothing anywhere ever calling
        // `DisplayHandle::remove_global`).
        state.display_handle.remove_global::<State>(surface.global);
        // Purge any lock surface keyed to this now-gone `Output` — see
        // `State::lock_surfaces`'s doc comment: nothing else hooks into
        // output removal to drop it, so a monitor unplugged mid-lock
        // otherwise left a stale, un-purgeable entry behind forever.
        state.lock_surfaces.retain(|(o, _)| o != &surface.output);
        // Same reasoning for a stale entry left behind if a monitor is
        // unplugged mid-lock, before the next full lock/unlock cycle
        // clears it — see `State::pending_lock_confirmed_outputs`'s doc
        // comment.
        state.pending_lock_confirmed_outputs.retain(|o| o != &surface.output);
        // Every Hut this output is about to lose (only if `remove_output`
        // below will actually remove the slot — it refuses to for the
        // very last one, in which case nothing is really destroyed yet)
        // needs its Alt-Tab thumbnail cache entry purged too — see
        // `render::purge_hut_content`'s doc comment. `remove_output`
        // itself has no hook back into `State`/`render.rs`'s thread-local
        // caches, so this is the one place responsible for it on the
        // hotplug-disconnect path, same as the shell-exit path in
        // `state.rs`.
        let will_remove_output = state.stack.outputs().len() > 1;
        let removed_hut_ids: Vec<u64> = if will_remove_output {
            state.stack.all_huts_for(surface.output_index).map(|hut| hut.id).collect()
        } else {
            Vec::new()
        };
        // Refuses to actually remove the last remaining slot (see
        // `GraphStack::remove_output`'s doc comment) — the disconnected
        // connector's `OutputSlot` is left in place with its now-stale
        // `Output` until another connector reattaches via `set_output`,
        // matching the exact single-output startup state this module
        // already tolerates before the first connector ever appears.
        state.stack.remove_output(surface.output_index);
        for id in removed_hut_ids {
            crate::render::purge_hut_content(id);
        }
        if will_remove_output {
            // After `remove_output`, not before — `state.stack.outputs()`
            // has to reflect the disconnected output's own removal
            // already, or `all_confirmed` would still see that (just-
            // purged-from-`pending_lock_confirmed_outputs`) slot and
            // never pass. Purging the disconnected output above can be
            // exactly what makes every *remaining* output already
            // confirmed — nothing else re-checks that on its own (see
            // `State::confirm_pending_lock_if_ready`'s doc comment).
            state.confirm_pending_lock_if_ready();
        } else if state.locked && let Some(confirmation) = state.pending_lock.take() {
            // The very last output — `remove_output` refused to actually
            // remove it (its stale `OutputSlot` is kept, per that
            // method's own doc comment), so it stays in `stack.outputs()`
            // forever with an `Output` that was just purged from
            // `pending_lock_confirmed_outputs` above and can never be
            // re-added (its real connector is permanently gone) —
            // `confirm_pending_lock_if_ready`'s `all_confirmed` check
            // could never pass again. But there are now zero real
            // connected displays: nothing could possibly still be
            // showing pre-lock content, since nothing is being displayed
            // at all, so a pending confirmation has nothing left to wait
            // on — confirm immediately instead of hanging forever.
            confirmation.lock();
        }
        // Every other surface pointing at a slot index *after* the
        // removed one shifts down by one along with it — mirrors
        // `GraphStack::remove_output`'s own index-shift internally.
        let mut inner_mut = inner.borrow_mut();
        for other in inner_mut.surfaces.values_mut() {
            if other.output_index > surface.output_index {
                other.output_index -= 1;
            }
        }
        drop(inner_mut);
        state.sync_focused_output();
    }
}

fn render_surface(state: &mut State, inner: &Rc<RefCell<Inner>>, crtc: crtc::Handle) {
    // Bail out early using only `inner`'s own borrow — deliberately
    // *before* touching the renderer at all, so the common "nothing to
    // do" cases (no surface, a commit already in flight) don't pay for a
    // real graph resolve pass (a terminal redraw) just to throw it away.
    let output_index = {
        let inner_ref = inner.borrow();
        let Some(surface) = inner_ref.surfaces.get(&crtc) else {
            tracing::debug!("render_surface: no surface for {crtc:?}, dropping the render chain here");
            return;
        };
        if surface.frame_pending {
            // A previous commit for this crtc hasn't completed yet — see
            // `SurfaceData::frame_pending`'s doc comment. `frame_finish`
            // will call back in here once it does.
            tracing::debug!("render_surface: frame already pending for {crtc:?}, skipping");
            return;
        }
        surface.output_index
    };

    // Every render pass, not just once at connector setup — matches
    // `winit_backend.rs`'s own redraw handler, which does the same
    // unconditionally every frame (cheap no-op via `resize_to_pixels`'s
    // own early-return when size is already correct). Without this, a
    // ConsoleHut spawned *after* the initial connector scan (e.g. Alt-Tabbing
    // past the stack's end to open a new one) never gets resized past
    // `ConsoleHut::spawn`'s tiny 80x24-cell placeholder grid. Sized to the
    // *usable* area, not the raw output — shrinks automatically whenever
    // a layer-shell surface's exclusive zone changes (see
    // `State::focused_usable_area`'s doc comment). This crtc's own output only —
    // real multi-monitor: two outputs can have genuinely different modes
    // (see `GraphStack::resize_output`'s own doc comment).
    let usable = state.usable_area_for(output_index).size;
    state.stack.resize_output(output_index, usable.w, usable.h);

    // Resolved *before* acquiring the renderer borrow below — see
    // `render::resolve_frame_content`'s own doc comment for why that
    // ordering isn't optional (it borrows the exact same
    // `Rc<RefCell<GlesRenderer>>` acquired just below, and `RefCell`
    // panics on a second concurrent borrow).
    let content = render::resolve_frame_content(state, output_index);

    let mut inner_mut = inner.borrow_mut();
    let Inner {
        renderer,
        surfaces,
        pointer_images,
        pointer_element,
        pointer_image_cache,
        ..
    } = &mut *inner_mut;
    let mut renderer = renderer.borrow_mut();
    let renderer = &mut *renderer;
    // Already confirmed to exist and not be frame_pending above — this
    // module never removes an entry between that check and here (single-
    // threaded, no other code runs in between).
    let Some(surface) = surfaces.get_mut(&crtc) else {
        return;
    };

    let size = surface
        .output
        .current_mode()
        .map(|mode| (mode.size.w, mode.size.h))
        .unwrap_or((0, 0));
    let output = surface.output.clone();

    tracing::debug!(
        "render_surface: output_index={output_index} focused hut={} showing_terminal_effective={} output_size={size:?}",
        state.stack.focused_for(output_index).id,
        state.showing_terminal_effective_for(output_index),
    );

    render::build_frame_elements(state, renderer, size, content, output_index, &mut surface.elements);

    // Prepended, not appended — elements render front-to-back (index 0
    // on top, per the same convention `switcher::build`'s doc comment
    // already relies on), and the cursor must stay above absolutely
    // everything else, including the Alt-Tab popup.
    let cursor_elements = build_cursor_elements(
        state,
        renderer,
        pointer_images,
        pointer_element,
        pointer_image_cache,
        output_index,
    );
    surface.elements.splice(0..0, cursor_elements);

    // `FrameFlags::empty()`, not `::DEFAULT` — `DEFAULT` allows the
    // `DrmCompositor` to attempt direct scanout via overlay/cursor
    // planes, not just plain GPU composition into the primary plane.
    // Direct scanout is a known category of driver-specific bugs (see
    // anvil's own NVIDIA overlay-plane workaround and its
    // `ANVIL_DISABLE_DIRECT_SCANOUT` escape hatch, which sets exactly
    // this) — on a display driver as young as Apple Silicon's, staying
    // on the simpler, more universally-exercised pure-composition path
    // is the safer default until there's a specific reason to want the
    // performance/power benefit of scanout.
    // A locked frame reflecting the current locked state has been built
    // (via `render.rs`'s early-return guard) into `elements` above,
    // regardless of whether this particular render pass finds any actual
    // pixel damage — called from both the "queued a new frame" and "no
    // damage, already showing this" branches below. See
    // `handlers/session_lock.rs`'s `lock` doc comment for why confirming
    // can't happen any earlier (e.g. synchronously inside that handler).
    // Only tells the locking client its lock succeeded once *every*
    // currently-connected output has reached this point — see
    // `State::pending_lock_confirmed_outputs`'s doc comment: each crtc
    // queues independently, so confirming on the first one to finish
    // would let every other monitor's pre-lock content stay visible
    // after the client's already been told the session is secured.
    let mark_this_output_confirmed = |state: &mut State| {
        if !state.locked || state.pending_lock.is_none() {
            return;
        }
        if !state.pending_lock_confirmed_outputs.contains(&output) {
            state.pending_lock_confirmed_outputs.push(output.clone());
        }
        state.confirm_pending_lock_if_ready();
    };

    match surface
        .drm_output
        .render_frame(renderer, &surface.elements, [0.0, 0.0, 0.0, 1.0], FrameFlags::empty())
    {
        Ok(result) => {
            if !result.is_empty {
                tracing::debug!("render_surface: damage found for {crtc:?}, queuing frame");
                match surface.drm_output.queue_frame(()) {
                    Ok(()) => {
                        surface.frame_pending = true;
                        mark_this_output_confirmed(state);
                    }
                    Err(err) => tracing::warn!("failed to queue DRM frame: {err}"),
                }
            } else {
                tracing::debug!("render_surface: no damage for {crtc:?}, waiting for the next redraw ping");
                // No damage means this output's *currently displayed*
                // frame already matches `elements` pixel-for-pixel — if
                // locked, it's already correctly showing the locked
                // backdrop this exact render pass just built, so this
                // output can be marked confirmed too. Without this, a
                // lock→unlock→lock cycle fast enough that an output's
                // damage tracker sees no diff against its own
                // already-blanked last frame never got added to
                // `pending_lock_confirmed_outputs` at all, stalling the
                // confirmation forever unless some unrelated redraw
                // happened to perturb that specific output later.
                mark_this_output_confirmed(state);
            }
        }
        // Deliberately never calls `mark_this_output_confirmed` on this
        // (or `queue_frame`'s own `Err` arm above) — unlike "no damage,"
        // a genuine render/queue failure means this output's *actual*
        // on-screen content is unknown; it could still be showing stale
        // pre-lock content from whatever last successfully committed.
        // Confirming here would trade a (safe, fail-closed) stalled lock
        // confirmation for a real content-disclosure risk on a broken
        // output — worse than the bug this class of fix is for. A
        // transient failure still gets retried on the next redraw
        // (`frame_pending` is only ever set `true` in the success path
        // above, so this surface isn't blocked from another attempt);
        // a persistent one legitimately can't have its lock state
        // verified until it recovers or is unplugged
        // (`connector_disconnected` purges it either way).
        Err(err) => tracing::warn!("render_frame failed: {err}"),
    }

    // Missing here entirely until this backend's own frame-callback fix
    // landed — without it, a well-behaved client (anything pacing its
    // own rendering off `wl_surface.frame` callbacks — effectively every
    // real client) draws once and then waits forever for a callback that
    // never comes, looking exactly like "doesn't work". `output_index`
    // (this crtc's own output), not always the globally-focused one —
    // see `render::send_frame_callbacks`'s own doc comment.
    render::send_frame_callbacks(state, output_index, &output);
}

/// Build this frame's cursor render element(s) at `state.pointer_location`
/// — either the loaded Xcursor theme's current frame (the common case,
/// `CursorImageStatus::Named`) or a client-provided cursor surface
/// (`CursorImageStatus::Surface`, set via `wl_pointer.set_cursor`), or
/// nothing at all if the client asked to hide the cursor.
fn build_cursor_elements(
    state: &mut State,
    renderer: &mut GlesRenderer,
    pointer_images: &mut HashMap<CursorIcon, Cursor>,
    pointer_element: &mut PointerElement,
    pointer_image_cache: &mut Vec<((CursorIcon, usize, i32), MemoryRenderBuffer)>,
    output_index: usize,
) -> Vec<Elements> {
    // A client's cursor surface can be destroyed without the client ever
    // telling us to switch away from it (e.g. the client itself exits) —
    // fall back rather than keep pointing at a dead surface.
    if let CursorImageStatus::Surface(surface) = &state.cursor_status
        && !surface.alive()
    {
        state.cursor_status = CursorImageStatus::default_named();
    }

    // This crtc's own output, not `state.focused_output_scale()` (the focused
    // one) — `output_position` a few lines below already resolves
    // per-output via `output_index`; the cursor's own size/hotspot/
    // render scale need to match.
    let scale = state.output_scale_for(output_index);
    // The buffer-scale integer every xcursor-theme frame is picked/
    // uploaded at below — same rounding as `render::texture_buffer_scale`
    // (a real fractional host scale loses sub-pixel precision here, same
    // caveat as that helper's own doc comment), so a HiDPI theme's sharper
    // 2x/3x frames actually get used instead of always the base-size one.
    let scale_int = render::texture_buffer_scale(scale);

    let hotspot = match &state.cursor_status {
        CursorImageStatus::Surface(surface) => with_states(surface, |states| {
            states
                .data_map
                .get::<std::sync::Mutex<CursorImageAttributes>>()
                .and_then(|attrs| attrs.lock().ok())
                .map(|guard| guard.hotspot)
                .unwrap_or_default()
        }),
        CursorImageStatus::Hidden => Point::from((0, 0)),
        // `wl_pointer.set_cursor`'s implicit default, or an explicit
        // `cursor-shape-v1` `set_shape` request (see `handlers/mod.rs`'s
        // `SeatHandler::cursor_image` — both funnel into `state.cursor_status`
        // the same way). Each distinct shape gets its own lazily-loaded
        // `Cursor` here rather than the single always-`Default` one this
        // used to always render, so e.g. `Text`/`Grab`/resize shapes
        // actually look different from the plain arrow.
        CursorImageStatus::Named(icon) => {
            let pointer_image = pointer_images.entry(*icon).or_insert_with(|| Cursor::load(*icon));
            match pointer_image.frame(scale_int as u32, state.start_time.elapsed()) {
                Some((frame_index, image)) => {
                    // Cache key is `(icon, frame_index, scale_int)` — a
                    // cheap identity, not `Image`'s own `PartialEq`, which
                    // includes the raw pixel buffer
                    // (`pixels_rgba`/`pixels_argb`) and did a byte-for-
                    // byte comparison against every cached entry on
                    // essentially every redraw while a named cursor was
                    // showing (pointer motion alone triggers one). Safe
                    // because `Cursor::icons` (`cursor.rs`) is a fixed
                    // list built once at `Cursor::load` time and never
                    // mutated afterward, so a given `(icon, frame_index)`
                    // always names the same frame's pixels for this
                    // session — but `scale_int` still has to be part of
                    // the key: a theme with only one bitmap size per icon
                    // returns the same `frame_index` regardless of the
                    // scale requested, and the buffer built below bakes
                    // `scale_int` into `MemoryRenderBuffer::from_slice`.
                    // Without it, two differently-scaled outputs showing
                    // the same icon collided on one cache entry and
                    // reused a buffer built for the wrong scale.
                    let key = (*icon, frame_index, scale_int);
                    let buffer = pointer_image_cache
                        .iter()
                        .find(|(cached, _)| *cached == key)
                        .map(|(_, buffer)| buffer.clone())
                        .unwrap_or_else(|| {
                            let buffer = MemoryRenderBuffer::from_slice(
                                &image.pixels_rgba,
                                Fourcc::Argb8888,
                                (image.width as i32, image.height as i32),
                                scale_int,
                                Transform::Normal,
                                None,
                            );
                            pointer_image_cache.push((key, buffer.clone()));
                            buffer
                        });
                    pointer_element.set_buffer(buffer);
                    // `image.xhot`/`yhot` are in *this* image's own pixel
                    // space — which, now that a HiDPI theme's scaled-up
                    // variant may have been picked above, is `scale_int`×
                    // the logical hotspot `state.pointer_location` (below)
                    // needs. Divided back down here so the two stay in
                    // the same (Logical) space before being combined —
                    // same buffer-scale-vs-logical-size trap as
                    // `render::texture_buffer_scale`'s doc comment, just
                    // for a hotspot offset instead of an element size.
                    Point::from(scale_down_hotspot(image.xhot, image.yhot, scale_int))
                }
                None => Point::from((0, 0)),
            }
        }
    };

    pointer_element.set_status(state.cursor_status.clone());

    // `state.pointer_location`/`hotspot` are both genuinely Logical (see
    // `state.rs`'s `pointer_location` doc comment) — converted to physical
    // here, at the render boundary, same as everywhere else in this
    // compositor. Real multi-monitor: `pointer_location` lives in one
    // shared global compositor space (`OutputSlot::position`'s own doc
    // comment), so it's re-based to *this* output's own local origin
    // before conversion — rendering it unadjusted would place the cursor
    // at the wrong local offset on every output except whichever one
    // happens to sit at `(0, 0)`. Harmless (just off-screen, naturally
    // clipped) on every output the pointer isn't currently over.
    let output_position = state
        .stack
        .outputs()
        .get(output_index)
        .map(|slot| slot.position)
        .unwrap_or_default();
    let cursor_pos = cursor_physical_position(state.pointer_location, output_position, hotspot, scale);

    pointer_element.render_elements(renderer, cursor_pos, Scale::from(scale), 1.0)
}

/// `image.xhot`/`yhot` (in the loaded cursor frame's own pixel space,
/// `scale_int`× a Logical hotspot once a HiDPI theme variant is picked)
/// descaled back down to Logical — see `build_cursor_elements`'s own
/// comment at its one call site for why the two need to share a space
/// before being combined with `state.pointer_location`. Pure `f64`
/// arithmetic, pulled out only because it used to live inline inside a
/// function that needs a real `&mut GlesRenderer` to reach at all.
fn scale_down_hotspot(xhot: u32, yhot: u32, scale_int: i32) -> (i32, i32) {
    ((xhot as f64 / scale_int as f64).round() as i32, (yhot as f64 / scale_int as f64).round() as i32)
}

/// The cursor's real physical-pixel render position for one output —
/// `pointer_location` (genuinely Logical, shared global compositor space)
/// re-based to `output_position` (this output's own local origin, real
/// multi-monitor), minus the hotspot offset, then converted to physical
/// at `scale`. Pulled out over plain `smithay::utils::Point`/`Scale`
/// values (freely constructible, not the blocker) purely because it used
/// to live inline inside `build_cursor_elements`, which needs a real
/// `&mut GlesRenderer` to reach at all. The output-rebase-then-convert
/// core is shared with `docks.rs::rebase_to_output_physical` (see its own
/// doc comment) — this just also subtracts the hotspot first and rounds
/// to a real pixel at the end, which that shared drag-position function
/// deliberately doesn't need to.
fn cursor_physical_position(
    pointer_location: Point<f64, smithay::utils::Logical>,
    output_position: Point<i32, smithay::utils::Logical>,
    hotspot: Point<i32, smithay::utils::Logical>,
    scale: f64,
) -> Point<i32, smithay::utils::Physical> {
    crate::docks::rebase_to_output_physical(pointer_location - hotspot.to_f64(), output_position, scale)
        .to_i32_round()
}

fn frame_finish(state: &mut State, inner: &Rc<RefCell<Inner>>, crtc: crtc::Handle) {
    tracing::debug!("frame_finish: vblank for {crtc:?}");
    let submitted = {
        let mut inner_mut = inner.borrow_mut();
        let Some(surface) = inner_mut.surfaces.get_mut(&crtc) else {
            tracing::debug!("frame_finish: no surface for {crtc:?}, dropping the render chain here");
            return;
        };
        surface.frame_pending = false;
        surface.drm_output.frame_submitted()
    };
    if let Err(err) = submitted {
        tracing::warn!("frame_submitted failed: {err}");
    }

    // Its own independent render pass — not part of either multi-crtc
    // loop above, so it needs its own `begin_frame` (see
    // `GraphStack::begin_frame`'s doc comment).
    state.stack.begin_frame();
    render_surface(state, inner, crtc);
}

/// DRM leasing (`wp_drm_lease_v1`) — see the module doc's DRM-leasing
/// section for the overall design. `node` is never looked up against
/// anything: mudhuts is single-GPU/single-seat (module doc), so it's
/// always the one `DrmNode` this whole module manages, unlike anvil's
/// `HashMap<DrmNode, BackendData>` indirection.
impl DrmLeaseHandler for State {
    fn drm_lease_state(&mut self, _node: DrmNode) -> &mut DrmLeaseState {
        // The trait forces an unconditional `&mut DrmLeaseState` return
        // with no `Option`/`Result` to signal absence through — and
        // nothing here can substitute a fallback value, so this really
        // is the one spot in this protocol's implementation where a
        // documented `.expect()` (not a silent `.unwrap()`) is the only
        // option, matching anvil's own `.leasing_global.as_mut().unwrap()`
        // for the identical reason. It's provably safe: this method is
        // only ever called by Smithay's own drm_lease dispatch code
        // (`handlers/mod.rs`'s blanket `delegate_dispatch2!(State)`),
        // which is only reachable once a client has bound the
        // `wp_drm_lease_device_v1` global — and that global is only ever
        // created by `init_udev` in the same place that sets this field,
        // right after a successful `DrmLeaseState::new`. If that call
        // had failed, the global was never registered, so no client
        // request could ever reach this method in the first place.
        self.drm_leasing_global
            .as_mut()
            .expect("drm_lease_state is only reachable after init_udev's DrmLeaseState::new succeeded")
    }

    fn lease_request(
        &mut self,
        _node: DrmNode,
        request: DrmLeaseRequest,
    ) -> Result<DrmLeaseBuilder, LeaseRejected> {
        // Unlike `drm_lease_state` above, this method has a real `Result`
        // to signal failure through — so an absent `udev_inner` (should
        // never happen for the same reason as `drm_lease_state`'s doc
        // comment, but there's no reason to risk a panic when rejecting
        // the lease costs nothing) just denies the request instead.
        let Some(udev_inner) = self.udev_inner.as_ref() else {
            tracing::warn!("lease_request with no udev backend state, denying");
            return Err(LeaseRejected::default());
        };
        let inner = udev_inner.borrow();
        let drm_device = inner.drm_output_manager.device();
        let mut builder = DrmLeaseBuilder::new(drm_device);
        for conn in request.connectors {
            let Some((_, crtc)) = inner
                .non_desktop_connectors
                .iter()
                .find(|(handle, _)| *handle == conn)
            else {
                tracing::warn!(?conn, "lease requested for a desktop connector, denying");
                return Err(LeaseRejected::default());
            };
            builder.add_connector(conn);
            builder.add_crtc(*crtc);
            // At least the primary plane (required to actually drive the
            // CRTC) plus the cursor plane if one is free — matches
            // anvil's own `lease_request` exactly; the plane-claiming
            // mechanism (`DrmDevice::claim_plane`) is the same one
            // `DrmOutputManager`/`DrmCompositor` use internally for
            // desktop rendering, so this can't double-claim a plane
            // that's already in use there.
            let planes = drm_device.planes(crtc).map_err(LeaseRejected::with_cause)?;
            let Some((primary_plane, primary_claim)) = planes.primary.iter().find_map(|plane| {
                drm_device
                    .claim_plane(plane.handle, *crtc)
                    .map(|claim| (plane, claim))
            }) else {
                tracing::warn!(?conn, "no free primary plane available to lease, denying");
                return Err(LeaseRejected::default());
            };
            builder.add_plane(primary_plane.handle, primary_claim);
            if let Some((cursor_plane, cursor_claim)) = planes.cursor.iter().find_map(|plane| {
                drm_device
                    .claim_plane(plane.handle, *crtc)
                    .map(|claim| (plane, claim))
            }) {
                builder.add_plane(cursor_plane.handle, cursor_claim);
            }
        }
        Ok(builder)
    }

    fn new_active_lease(&mut self, _node: DrmNode, lease: DrmLease) {
        let Some(udev_inner) = self.udev_inner.as_ref() else {
            // Should never happen (see `drm_lease_state`'s doc comment) —
            // but there's nowhere to stash this lease without `Inner`, so
            // just let it drop immediately, which revokes it.
            tracing::warn!("new_active_lease with no udev backend state, revoking the lease immediately");
            return;
        };
        udev_inner.borrow_mut().active_leases.push(lease);
    }

    fn lease_destroyed(&mut self, _node: DrmNode, lease_id: u32) {
        let Some(udev_inner) = self.udev_inner.as_ref() else {
            return;
        };
        udev_inner.borrow_mut().active_leases.retain(|lease| lease.id() != lease_id);
    }
}

// `wlr-gamma-control-unstable-v1`: lets a privileged client (night-light
// tools like gammastep/wlsunset) adjust a CRTC's hardware gamma ramp.
// There's no Smithay-provided handler for this (unlike `DmabufHandler`
// above) — no `wayland::gamma_control` module exists — so this is a
// hand-rolled `Dispatch2`/`GlobalDispatch2` pair against the raw generated
// protocol bindings, the same pattern `handlers/shell.rs` already
// establishes for `mudhuts_shell_v1`. It lives here rather than in
// `handlers/` because the actual gamma-ramp ioctls
// (`drm::control::Device::get_gamma`/`set_gamma` — a standalone legacy
// ioctl, entirely separate from the atomic-commit/property-blob machinery
// `DrmOutputManager` otherwise uses for page-flips) can only be reached
// through `Inner::drm_output_manager`, which stays private to this
// module; see `Inner`'s own doc comment for why that's threaded through
// `State::udev_inner` (shared with `DrmLeaseHandler` above) rather than
// stashed directly in Wayland resource user-data.
//
// Deliberately never touched from `winit_backend.rs`: there's no real
// display hardware there to apply a gamma ramp to, and `state.udev_inner`
// is simply never set to `Some` under that backend, so the global is
// never registered and no client ever sees the interface in the registry
// — matching the `dmabuf_global` precedent exactly (see `state.rs`'s doc
// comment on both fields).

impl GlobalDispatch2<ZwlrGammaControlManagerV1, State> for GlobalData {
    fn bind(
        &self,
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl Dispatch2<ZwlrGammaControlManagerV1, State> for GlobalData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } => {
                get_gamma_control(state, id, output, data_init);
            }
            // "All objects created by the manager will still remain
            // valid, until their appropriate destroy request has been
            // called" — nothing to do here for the manager itself.
            zwlr_gamma_control_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

/// Per-`zwlr_gamma_control_v1` state. `None` when this object was created
/// only to be told `.failed()` immediately (see `get_gamma_control`) — no
/// exclusive CRTC access was ever granted, so `SetGamma`/`destroyed`
/// below become no-ops rather than acting on a CRTC this object was never
/// given.
struct GammaControlUserData {
    active: Option<ActiveGammaControl>,
}

struct ActiveGammaControl {
    crtc: crtc::Handle,
    gamma_size: u32,
    /// The CRTC's gamma ramp exactly as found the moment this object was
    /// created — restored via `set_gamma` once this object is destroyed
    /// (explicit `destroy` request or the client disconnecting; both
    /// funnel through `Dispatch2::destroyed`, see its default-no-op doc
    /// comment), matching the protocol's own requirement ("When the gamma
    /// control object is destroyed, the gamma table is restored to its
    /// original value").
    original: (Vec<u16>, Vec<u16>, Vec<u16>),
}

/// Handle `zwlr_gamma_control_manager_v1.get_gamma_control`: resolve
/// `output` to the `SurfaceData`/`crtc::Handle` that's actually driving
/// it, then either grant exclusive access — capturing the CRTC's current
/// ramp so `GammaControlUserData`'s `destroyed` hook can restore it later
/// — or send an immediate `.failed()` per the protocol (unknown output,
/// already bound by another client, or a CRTC reporting a zero-length
/// ramp, i.e. no gamma hardware support). A resource is always created
/// either way — the client needs a live object to receive `.failed()` on
/// in the first place.
fn get_gamma_control(
    state: &mut State,
    id: New<ZwlrGammaControlV1>,
    output: WlOutput,
    data_init: &mut DataInit<'_, State>,
) {
    let active = (|| -> Option<ActiveGammaControl> {
        let target = Output::from_resource(&output)?;
        let inner = state.udev_inner.clone()?;
        let mut inner_mut = inner.borrow_mut();
        let Inner {
            drm_output_manager,
            surfaces,
            ..
        } = &mut *inner_mut;
        let (&crtc, surface) = surfaces.iter_mut().find(|(_, s)| s.output == target)?;
        if surface.gamma_control_bound {
            return None;
        }
        let gamma_size = drm_output_manager.device().get_crtc(crtc).ok()?.gamma_length();
        if gamma_size == 0 {
            return None;
        }
        let gamma_size = gamma_size as usize;
        let mut red = vec![0u16; gamma_size];
        let mut green = vec![0u16; gamma_size];
        let mut blue = vec![0u16; gamma_size];
        drm_output_manager
            .device()
            .get_gamma(crtc, &mut red, &mut green, &mut blue)
            .ok()?;
        surface.gamma_control_bound = true;
        Some(ActiveGammaControl {
            crtc,
            gamma_size: gamma_size as u32,
            original: (red, green, blue),
        })
    })();

    match active {
        Some(active) => {
            let gamma_size = active.gamma_size;
            let resource = data_init.init(id, GammaControlUserData { active: Some(active) });
            // "Sent immediately when the gamma control object is created."
            resource.gamma_size(gamma_size);
        }
        None => {
            let resource = data_init.init(id, GammaControlUserData { active: None });
            resource.failed();
        }
    }
}

impl Dispatch2<ZwlrGammaControlV1, State> for GammaControlUserData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        let Some(active) = &self.active else {
            // Already `.failed()` at creation — nothing was ever granted
            // for this object to act on.
            return;
        };
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                if let Err(err) = apply_gamma(state, active, fd) {
                    tracing::warn!("wlr-gamma-control: set_gamma failed: {err}");
                    resource.failed();
                }
            }
            zwlr_gamma_control_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut State, _client: ClientId, _resource: &ZwlrGammaControlV1) {
        let Some(active) = &self.active else {
            return;
        };
        let Some(inner) = state.udev_inner.as_ref() else {
            return;
        };
        let mut inner_mut = inner.borrow_mut();
        let (red, green, blue) = &active.original;
        if let Err(err) = inner_mut
            .drm_output_manager
            .device()
            .set_gamma(active.crtc, red, green, blue)
        {
            tracing::warn!("wlr-gamma-control: failed to restore the original gamma ramp: {err}");
        }
        if let Some(surface) = inner_mut.surfaces.get_mut(&active.crtc) {
            surface.gamma_control_bound = false;
        }
    }
}

/// Parse a raw gamma table buffer — per the protocol, three concatenated
/// `u16` ramps (red, green, blue), each `gamma_size` long, exactly what
/// the legacy `set_gamma` ioctl itself expects. Native-endian: like an
/// mmap'd C `uint16_t[]`, this is raw memory the client wrote directly,
/// not a serialized wire format with its own defined byte order. Decoded
/// as one flat `red ++ green ++ blue` sequence — the caller splits it
/// three ways with a zero-copy `split_at` (trivial slicing, not worth its
/// own extraction/test). `None` if `buf`'s length doesn't match
/// `gamma_size * 3 * 2` exactly — in practice `apply_gamma`'s own caller
/// always allocates `buf` at exactly that size before filling it via
/// `read_exact` (so a short read surfaces as its own separate `Err`
/// before this is ever called), but checking here too rather than
/// assuming means this stays panic-free even if that invariant is ever
/// violated by a future caller. Pulled out as a pure function over
/// `&[u8]` specifically so it's testable without a real client fd —
/// returns one flat `Vec<u16>` rather than three separately-owned ones
/// (an earlier version did, then a review caught that the caller
/// immediately re-borrowed all three anyway — three extra heap copies of
/// the whole gamma table for no benefit, on every `SetGamma` request a
/// night-light-style client sends).
fn parse_gamma_words(buf: &[u8], gamma_size: usize) -> Option<Vec<u16>> {
    if buf.len() != gamma_size * 3 * 2 {
        return None;
    }
    Some(buf.chunks_exact(2).map(|pair| u16::from_ne_bytes([pair[0], pair[1]])).collect())
}

/// Read the client's raw gamma table off `fd` and apply it via the real
/// `set_gamma` ioctl — see [`parse_gamma_words`] for the actual table
/// layout/parsing. A short read, a malformed table, or the ioctl call
/// itself failing all surface as a plain `Err` — the caller sends
/// `.failed()` rather than trusting a client-supplied fd to always
/// behave.
fn apply_gamma(state: &mut State, active: &ActiveGammaControl, fd: OwnedFd) -> Result<(), String> {
    let mut file = File::from(fd);
    let gamma_size = active.gamma_size as usize;
    let mut buf = vec![0u8; gamma_size * 3 * 2];
    file.read_exact(&mut buf)
        .map_err(|err| format!("short read of the gamma table: {err}"))?;

    let words =
        parse_gamma_words(&buf, gamma_size).ok_or_else(|| "gamma table length mismatch".to_string())?;
    let (red, rest) = words.split_at(gamma_size);
    let (green, blue) = rest.split_at(gamma_size);

    let inner = state
        .udev_inner
        .as_ref()
        .ok_or_else(|| "gamma control backend is gone".to_string())?;
    inner
        .borrow()
        .drm_output_manager
        .device()
        .set_gamma(active.crtc, red, green, blue)
        .map_err(|err| format!("set_gamma ioctl failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::drm::control::Mode as DrmMode;

    #[test]
    fn non_desktop_and_not_a_built_in_panel_interface_is_leasable() {
        assert!(is_leasable(true, connector::Interface::DisplayPort));
        assert!(is_leasable(true, connector::Interface::HDMIA));
    }

    #[test]
    fn not_reported_non_desktop_is_never_leasable() {
        assert!(!is_leasable(false, connector::Interface::DisplayPort));
    }

    #[test]
    fn built_in_panel_interfaces_are_never_leasable_even_if_reported_non_desktop() {
        // Defense-in-depth: the built-in panel must never be leasable
        // regardless of what the `non-desktop` DRM property (driver/EDID
        // derived) reports — see `is_leasable`'s own doc comment.
        for interface in [
            connector::Interface::EmbeddedDisplayPort,
            connector::Interface::LVDS,
            connector::Interface::DSI,
            connector::Interface::DPI,
        ] {
            assert!(
                !is_leasable(true, interface),
                "{interface:?} should never be leasable even if reported non-desktop"
            );
        }
    }

    #[test]
    fn picks_the_flagged_preferred_mode() {
        assert_eq!(pick_preferred_mode_index(&[false, true, false]), 1);
    }

    #[test]
    fn falls_back_to_the_first_mode_when_nothing_is_flagged_preferred() {
        assert_eq!(pick_preferred_mode_index(&[false, false, false]), 0);
        assert_eq!(pick_preferred_mode_index(&[]), 0);
    }

    #[test]
    fn parse_gamma_words_decodes_a_well_formed_buffer_into_the_flat_red_green_blue_sequence() {
        let gamma_size = 2;
        let mut buf = Vec::new();
        for v in [1u16, 2, 3, 4, 5, 6] {
            buf.extend_from_slice(&v.to_ne_bytes());
        }
        let words = parse_gamma_words(&buf, gamma_size).unwrap();
        let (red, rest) = words.split_at(gamma_size);
        let (green, blue) = rest.split_at(gamma_size);
        assert_eq!(red, [1, 2]);
        assert_eq!(green, [3, 4]);
        assert_eq!(blue, [5, 6]);
    }

    #[test]
    fn parse_gamma_words_rejects_a_buffer_with_the_wrong_length() {
        assert_eq!(parse_gamma_words(&[0u8; 5], 2), None);
        assert_eq!(parse_gamma_words(&[], 2), None);
    }

    #[test]
    fn scale_down_hotspot_descales_by_the_integer_buffer_scale() {
        assert_eq!(scale_down_hotspot(20, 30, 2), (10, 15));
        assert_eq!(scale_down_hotspot(0, 0, 1), (0, 0));
    }

    #[test]
    fn cursor_physical_position_subtracts_output_position_and_hotspot_then_converts_to_physical() {
        let pointer_location = Point::<f64, smithay::utils::Logical>::from((110.0, 220.0));
        let output_position = Point::<i32, smithay::utils::Logical>::from((100, 200));
        let hotspot = Point::<i32, smithay::utils::Logical>::from((2, 3));
        let result = cursor_physical_position(pointer_location, output_position, hotspot, 2.0);
        // Logical delta: (110-100-2, 220-200-3) = (8, 17); at scale 2.0 -> (16, 34).
        assert_eq!(result, Point::<i32, smithay::utils::Physical>::from((16, 34)));
    }

    #[test]
    fn next_output_x_starts_at_zero_with_nothing_placed_yet() {
        assert_eq!(next_output_x(&[]), 0);
    }

    #[test]
    fn next_output_x_places_a_fresh_output_to_the_right_of_every_existing_one() {
        // (position_x, mode_width_px, scale)
        let existing = [(0, 1920, 1.0), (1920, 2560, 2.0)];
        // Second output's own right edge: 1920 + (2560 / 2.0) = 3200.
        assert_eq!(next_output_x(&existing), 3200);
    }

    /// `detect_output_scale`'s tests all need `MUDHUTS_OUTPUT_SCALE`
    /// genuinely absent — it overrides the function's return value
    /// unconditionally when set (see the module doc's own note on this
    /// escape hatch), so a developer who happens to have it exported in
    /// their shell (e.g. left over from manually driving `mudhuts --tty`
    /// on real hardware) would otherwise see these fail spuriously,
    /// asserting the override's value instead of the computed one
    /// (caught in review). `unsafe` since Rust 2024 — `std::env::
    /// remove_var` on a value nothing else in this process reads is safe
    /// in practice; the signature just can't express that.
    fn clear_output_scale_override() {
        unsafe {
            std::env::remove_var("MUDHUTS_OUTPUT_SCALE");
        }
    }

    #[test]
    fn detect_output_scale_defaults_to_1x_below_the_192_dpi_threshold() {
        clear_output_scale_override();
        // A 24" 1920x1080 display: ~92 DPI, well under the threshold.
        assert_eq!(detect_output_scale((531, 299), (1920, 1080)), 1.0);
    }

    #[test]
    fn detect_output_scale_switches_to_2x_at_the_192_dpi_threshold() {
        clear_output_scale_override();
        // This module's own motivating hardware target (see
        // `detect_output_scale`'s doc comment) — Apple Silicon's built-in
        // Retina panel, comfortably past 192 DPI: a 13" MacBook panel is
        // roughly 254mm x 159mm at 2560x1600.
        assert_eq!(detect_output_scale((254, 159), (2560, 1600)), 2.0);
    }

    #[test]
    fn detect_output_scale_degenerate_zero_size_falls_back_to_1x() {
        clear_output_scale_override();
        assert_eq!(detect_output_scale((0, 0), (1920, 1080)), 1.0);
        assert_eq!(detect_output_scale((531, 299), (0, 0)), 1.0);
    }

    // `MUDHUTS_OUTPUT_SCALE`'s override path *itself* is deliberately not
    // covered here: `std::env::set_var` mutates real, process-global
    // state, and Rust runs `#[test]`s in parallel within one process by
    // default — setting it in one test could leak into any other test
    // running concurrently (including the ones just above, which rely on
    // the override being *absent* — see `clear_output_scale_override`).
    // Not worth a `serial_test`-style dependency just for this one
    // branch.

    /// Finds a `vkms`-backed DRM card node under `/sys/class/drm`, if one
    /// is currently loaded (`sudo modprobe vkms`). vkms registers via the
    /// kernel's "faux device" API, so its own `driver` symlink resolves
    /// to `faux_driver`, not literally `vkms` — confirmed directly on
    /// `ilama` (this session's own live machine, which already ships the
    /// vkms kernel module) that the reliable identifier is `vkms`
    /// appearing in the real, canonicalized sysfs device path instead
    /// (`/sys/devices/faux/vkms/drm/cardN`).
    fn find_vkms_card_path() -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip `cardN-<connector>` sysfs entries — only the bare
            // `cardN` device directory itself has a `device` symlink
            // worth following.
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let Ok(real_path) = std::fs::canonicalize(entry.path().join("device")) else {
                continue;
            };
            // vkms's `device` symlink resolves to `/sys/devices/faux/
            // vkms` (a *faux* platform device, no further path segment
            // after its own name) — not `.../vkms/drm/cardN`, as an
            // earlier version of this check assumed by requiring a
            // trailing `/` after `vkms` (caught the hard way: it never
            // matched against this session's own real, live vkms card).
            // `file_name()` (the path's own last component) is the
            // robust check either way.
            if real_path.file_name().is_some_and(|n| n == "vkms") {
                return Some(std::path::PathBuf::from(format!("/dev/dri/{name}")));
            }
        }
        None
    }

    /// Minimal `drm::Device`/`drm::control::Device` impl over a raw
    /// opened card node — same shape smithay's own `DrmDevice` wraps
    /// internally, but without any of its session/seat/GBM machinery,
    /// which this test deliberately doesn't need: it only ever reads
    /// connector/mode info, never attempts a real mode-set.
    struct VkmsCard(std::fs::File);

    impl std::os::fd::AsFd for VkmsCard {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    impl smithay::reexports::drm::Device for VkmsCard {}
    impl smithay::reexports::drm::control::Device for VkmsCard {}

    /// Shared setup for every `vkms_*` test below: opens the card
    /// read-only (nothing here ever mode-sets, so no write access is
    /// needed — an earlier version opened it `.write(true)` regardless,
    /// caught in review) and picks a real connector to test against,
    /// preferring one already `Connected` over just taking whichever one
    /// enumerates first (vkms can register extra non-display connectors,
    /// e.g. a writeback one if `enable_writeback` is set — enumeration
    /// order isn't a contract). `DrmDevice`'s own session/capability-
    /// negotiation machinery (`ClientCapability::UniversalPlanes`/
    /// `Atomic`, negotiated by every real production call path in this
    /// file via `LibSeatSession`) is deliberately *not* set up here: this
    /// only ever reads connector/mode/property info, none of which needs
    /// it, and standing that up for real is bigger, separate scope (see
    /// the wishlist's own "genuine infra project" framing) — so this
    /// suite doesn't catch a regression that only manifests once those
    /// capabilities are actually negotiated.
    fn open_vkms_card() -> (VkmsCard, connector::Info) {
        use smithay::reexports::drm::control::Device as ControlDevice;

        let path = find_vkms_card_path()
            .expect("no vkms card found under /sys/class/drm — run `sudo modprobe vkms` first");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .unwrap_or_else(|err| panic!("failed to open {path:?}: {err}"));
        let card = VkmsCard(file);

        let resources = card.resource_handles().expect("failed to get vkms resource handles");
        let mut infos: Vec<connector::Info> =
            resources.connectors().iter().filter_map(|&h| card.get_connector(h, true).ok()).collect();
        // Requires `Connected` *with* real modes, failing right here with
        // a clear message if nothing qualifies — an earlier version fell
        // back through several weaker tiers (any `Connected`, then just
        // index `0`) down to a mode-less connector, which turned a clear
        // "no usable connector" failure into a confusing out-of-bounds
        // panic three tests down the line instead (caught in review,
        // twice: once for the missing modes check, once more for why
        // silently falling back at all was the wrong shape here).
        let summary: Vec<_> = infos.iter().map(|i| (i.interface(), i.state(), i.modes().len())).collect();
        let index = infos
            .iter()
            .position(|i| i.state() == connector::State::Connected && !i.modes().is_empty())
            .unwrap_or_else(|| panic!("vkms should report at least one Connected connector with real modes among: {summary:?}"));
        let info = infos.swap_remove(index);
        (card, info)
    }

    /// `info`'s own preferred mode — shared by every `vkms_*` test below
    /// that needs one, replacing three copies of the identical
    /// `is_preferred`/`pick_preferred_mode_index` dance an earlier
    /// version hand-rolled separately in each (caught in review: exactly
    /// the kind of duplication a future fix to this logic could update
    /// in two places and silently miss the third). Panics with a clear
    /// message on an empty mode list rather than silently returning
    /// nothing — matches this suite's own "fail loudly, not quietly" for
    /// a real-hardware precondition that should always hold for vkms.
    fn preferred_mode_of(info: &connector::Info) -> DrmMode {
        assert!(!info.modes().is_empty(), "vkms should always report at least one mode");
        info.modes()[pick_preferred_mode_index(&is_preferred_flags(info))]
    }

    /// `info`'s own modes, mapped to whether each is flagged
    /// `ModeTypeFlags::PREFERRED` — the one piece `preferred_mode_of`
    /// and `vkms_preferred_mode_index_picks_the_flagged_mode` both genuinely
    /// need (the latter also needs the intermediate flags themselves for
    /// its own bounds/consistency assertions, not just the final picked
    /// mode `preferred_mode_of` returns, so it can't just call that
    /// instead — but the flag computation itself doesn't need
    /// duplicating for that).
    fn is_preferred_flags(info: &connector::Info) -> Vec<bool> {
        info.modes().iter().map(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).collect()
    }

    /// Real exercise of `connector_bool_property` against a genuine
    /// connector. vkms never sets the `non-desktop` property at all, so
    /// `non_desktop` here should resolve to that function's own
    /// documented not-found fallback (`false`) — a real, non-tautological
    /// check of the property-lookup path itself.
    ///
    /// The obvious complementary `is_leasable`-against-real-data check
    /// (asserting a vkms connector is never leasable) turns out to be
    /// tautological no matter how it's written: with `non_desktop`
    /// pinned to `false` by vkms's own real, unchangeable behavior,
    /// `is_leasable`'s `&&` guarantees `false` regardless of interface —
    /// an earlier version of this test asserted exactly that anyway,
    /// which would still pass even with the entire interface-denylist
    /// clause deleted (caught in review, twice: once for hardcoding
    /// `non_desktop = false` directly, once more for realizing the *real*
    /// value is unconditionally `false` too). vkms simply can't produce
    /// the one input (`non_desktop = true`) that clause exists to guard
    /// against, so that half of the contract can only ever be exercised
    /// by the synthetic `built_in_panel_interfaces_are_never_leasable_
    /// even_if_reported_non_desktop` test above, with hand-crafted data.
    /// What *can* be checked for real here instead: that vkms's own real
    /// interface value isn't accidentally in that denylist — genuinely
    /// exercises the denylist match against real hardware-reported data,
    /// just crossed with a hypothetical `non_desktop = true` rather than
    /// vkms's real (always-`false`) one.
    #[test]
    #[ignore = "needs a real vkms card (`sudo modprobe vkms`) present under /dev/dri — not portable to arbitrary machines/CI without it"]
    fn vkms_connector_is_never_leasable() {
        let (card, info) = open_vkms_card();
        let non_desktop = connector_bool_property(&card, info.handle(), "non-desktop");
        assert!(!non_desktop, "vkms doesn't set the non-desktop property, so this should fall back to false");

        assert!(
            is_leasable(true, info.interface()),
            "vkms's own real interface ({:?}) must not be in the built-in-panel denylist — a future vkms \
             reporting non-desktop=true would otherwise be wrongly treated as never-leasable",
            info.interface()
        );
    }

    #[test]
    #[ignore = "needs a real vkms card (`sudo modprobe vkms`) present under /dev/dri — not portable to arbitrary machines/CI without it"]
    fn vkms_preferred_mode_index_picks_the_flagged_mode() {
        let (_card, info) = open_vkms_card();
        assert!(!info.modes().is_empty(), "vkms should always report at least one mode");
        let is_preferred = is_preferred_flags(&info);
        let preferred_index = pick_preferred_mode_index(&is_preferred);
        // Deliberately *not* also asserting `preferred_index <
        // info.modes().len()` here — that bound holds by construction
        // (`pick_preferred_mode_index` is `position(...).unwrap_or(0)`
        // over a slice built 1:1 from `info.modes()`), so it can never
        // be false regardless of whether the function is even correct;
        // an earlier version of this test asserted it anyway, giving a
        // false sense of coverage (caught in review). The check below is
        // the only one that actually exercises real selection behavior.
        if is_preferred.iter().any(|&p| p) {
            assert!(is_preferred[preferred_index], "a flagged-preferred mode exists, so it must be the one picked");
        }
        // The `unwrap_or(0)`-fallback branch (nothing flagged preferred)
        // isn't exercised here — vkms's own real data does flag one, and
        // this test can't fabricate different data to force the other
        // branch; `falls_back_to_the_first_mode_when_nothing_is_flagged_
        // preferred` above already covers it directly.
    }

    /// A real exercise of the degenerate-input guard against vkms's own
    /// genuine "no physical size reported" connector, not just the
    /// hand-crafted `(0, 0)` in `detect_output_scale_degenerate_zero_
    /// size_falls_back_to_1x` above.
    #[test]
    #[ignore = "needs a real vkms card (`sudo modprobe vkms`) present under /dev/dri — not portable to arbitrary machines/CI without it"]
    fn vkms_reports_a_sane_detect_output_scale() {
        clear_output_scale_override();
        let (_card, info) = open_vkms_card();
        let preferred_mode = preferred_mode_of(&info);

        let (phys_w, phys_h) = info.size().unwrap_or((0, 0));
        let (mode_w, mode_h) = preferred_mode.size();
        let scale = detect_output_scale((phys_w as i32, phys_h as i32), (mode_w as i32, mode_h as i32));
        assert!(scale == 1.0 || scale == 2.0, "detect_output_scale should only ever return 1.0 or 2.0, got {scale}");
    }
}
