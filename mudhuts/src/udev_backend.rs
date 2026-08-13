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
//! deferred: multi-GPU `GpuManager`/`MultiRenderer`, DRM leasing,
//! explicit GPU sync via drm-syncobj, 10-bit color, dmabuf client-buffer
//! import). Cursor rendering (`cursor.rs`) is no longer on that list —
//! implemented once real GUI clients confirmed the rest of the backend
//! actually worked.
//!
//! Rendering is demand-driven, same principle as `winit_backend.rs`'s use
//! of `redraw_ping`/`request_redraw()`: nothing here polls on a timer.
//! `redraw_ping_source` fires `render_surface` for every known crtc
//! whenever shared code calls `State::request_redraw()` (PTY output, a
//! keypress, a client commit, etc.) — but a render attempt only actually
//! submits a new atomic commit if `render_frame` finds real damage *and*
//! no previous commit for that crtc is still in flight
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
//! a fixed wall-clock interval — if Asahi's DRM driver exposes adaptive
//! sync on a given connector (untested; not verified either way), the
//! kernel decides how soon that next vblank actually is, and nothing
//! about this render-triggering logic needs to change either way.

use std::cell::RefCell;
use std::collections::HashMap;
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
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties};
use smithay::reexports::calloop::ping::PingSource;
use smithay::reexports::calloop::{EventLoop, LoopHandle};
use smithay::reexports::drm::control::{ModeTypeFlags, connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::{DeviceFd, IsAlive, Point, Scale, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::State;
use crate::cursor::{Cursor, PointerElement};
use crate::render::{self, OutputRenderElements};

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type OutputManager = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type CrtcOutput = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;
type Elements = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

struct SurfaceData {
    output: Output,
    drm_output: CrtcOutput,
    /// Whether a submitted atomic commit for this crtc is still waiting
    /// on its `DrmEvent::VBlank` completion. Gates `render_surface`: a
    /// second commit can't safely go out before the first one's
    /// page-flip event has been processed (see the module doc), so a
    /// `redraw_ping` that arrives mid-flight is dropped here rather than
    /// attempted — `frame_finish` re-renders once the pending commit
    /// actually completes, picking up anything that changed meanwhile.
    frame_pending: bool,
}

struct Inner {
    /// `Rc<RefCell<_>>`, not owned outright — `State::dmabuf_renderer`
    /// holds a clone of the same renderer so `DmabufHandler::dmabuf_imported`
    /// (`handlers/mod.rs`) can attempt a client buffer import, since
    /// `State` otherwise has no renderer of its own (that's normally
    /// backend-private state — see this module's doc).
    renderer: Rc<RefCell<GlesRenderer>>,
    drm_output_manager: OutputManager,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// The loaded Xcursor theme (see `cursor.rs`) and the last-seen
    /// `CursorImageStatus`'s composited render state — both persist
    /// across frames rather than being rebuilt each render pass.
    pointer_image: Cursor,
    pointer_element: PointerElement,
    /// Caches one uploaded `MemoryRenderBuffer` per distinct xcursor
    /// frame `Image` (an animated cursor theme has only a handful of
    /// these) — without this, every single render pass would re-upload
    /// the same pixel data to a fresh GPU texture even when the cursor's
    /// visible frame hasn't changed since the last one.
    pointer_image_cache: Vec<(xcursor::parser::Image, MemoryRenderBuffer)>,
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
        pointer_image: Cursor::load(),
        pointer_element: PointerElement::default(),
        pointer_image_cache: Vec::new(),
    }));

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
                }
                SessionEvent::ActivateSession => {
                    tracing::info!("session resumed (VT switched back)");
                    if let Err(err) = inner.borrow_mut().drm_output_manager.lock().activate(false) {
                        tracing::warn!("failed to reactivate the DRM device: {err}");
                    }
                    // Nothing else re-kicks rendering after a resume —
                    // explicitly force a fresh pass on every crtc.
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

fn connector_connected(
    state: &mut State,
    inner: &Rc<RefCell<Inner>>,
    handle: &LoopHandle<'static, State>,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let output_name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
    tracing::info!("setting up connector {output_name} on {crtc:?}");

    let mode_id = connector
        .modes()
        .iter()
        .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(0);
    let Some(&drm_mode) = connector.modes().get(mode_id) else {
        tracing::warn!("connector {output_name} has no usable modes, skipping");
        return;
    };
    let wl_mode = WlMode::from(drm_mode);

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
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
    let _global = output.create_global::<State>(&state.display_handle);
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), None, None, Some((0, 0).into()));

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
                return;
            }
        }
    };

    state.space.map_output(&output, (0, 0));
    state.output_size = (wl_mode.size.w, wl_mode.size.h);
    state.stack.resize_all(wl_mode.size.w, wl_mode.size.h);
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
            drm_output,
            frame_pending: false,
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
        render_surface(state, &inner, crtc);
    });
}

fn connector_disconnected(
    state: &mut State,
    inner: &Rc<RefCell<Inner>>,
    _connector: connector::Info,
    crtc: crtc::Handle,
) {
    let removed = inner.borrow_mut().surfaces.remove(&crtc);
    if let Some(surface) = removed {
        state.space.unmap_output(&surface.output);
    }
}

fn render_surface(state: &mut State, inner: &Rc<RefCell<Inner>>, crtc: crtc::Handle) {
    let mut inner_mut = inner.borrow_mut();
    let Inner {
        renderer,
        surfaces,
        pointer_image,
        pointer_element,
        pointer_image_cache,
        ..
    } = &mut *inner_mut;
    let mut renderer = renderer.borrow_mut();
    let renderer = &mut *renderer;
    let Some(surface) = surfaces.get_mut(&crtc) else {
        tracing::debug!("render_surface: no surface for {crtc:?}, dropping the render chain here");
        return;
    };

    if surface.frame_pending {
        // A previous commit for this crtc hasn't completed yet — see
        // `SurfaceData::frame_pending`'s doc comment. `frame_finish` will
        // call back in here once it does.
        tracing::debug!("render_surface: frame already pending for {crtc:?}, skipping");
        return;
    }

    let size = surface
        .output
        .current_mode()
        .map(|mode| (mode.size.w, mode.size.h))
        .unwrap_or((0, 0));
    let output = surface.output.clone();

    // Every render pass, not just once at connector setup — matches
    // `winit_backend.rs`'s own redraw handler, which does the same
    // unconditionally every frame (cheap no-op via `resize_to_pixels`'s
    // own early-return when size is already correct). Without this, a
    // Hut spawned *after* the initial connector scan (e.g. Alt-Tabbing
    // past the stack's end to open a new one) never gets resized past
    // `Hut::spawn`'s tiny 80x24-cell placeholder grid.
    state.stack.resize_all(size.0, size.1);

    tracing::debug!(
        "render_surface: focused hut={} showing_terminal_effective={} output_size={size:?}",
        state.stack.focused().id,
        state.showing_terminal_effective(),
    );

    let mut elements = render::build_frame_elements(state, renderer, &output, size);

    // Prepended, not appended — elements render front-to-back (index 0
    // on top, per the same convention `switcher::build`'s doc comment
    // already relies on), and the cursor must stay above absolutely
    // everything else, including the Alt-Tab popup.
    let cursor_elements = build_cursor_elements(
        state,
        renderer,
        pointer_image,
        pointer_element,
        pointer_image_cache,
    );
    elements.splice(0..0, cursor_elements);

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
    match surface
        .drm_output
        .render_frame(renderer, &elements, [0.0, 0.0, 0.0, 1.0], FrameFlags::empty())
    {
        Ok(result) => {
            if !result.is_empty {
                tracing::debug!("render_surface: damage found for {crtc:?}, queuing frame");
                match surface.drm_output.queue_frame(()) {
                    Ok(()) => surface.frame_pending = true,
                    Err(err) => tracing::warn!("failed to queue DRM frame: {err}"),
                }
            } else {
                tracing::debug!("render_surface: no damage for {crtc:?}, waiting for the next redraw ping");
            }
        }
        Err(err) => tracing::warn!("render_frame failed: {err}"),
    }

    // Missing here entirely until now — `winit_backend.rs`'s redraw
    // handler has always done this, but nothing under this backend ever
    // did. Without `send_frame`, a well-behaved client (anything pacing
    // its own rendering off `wl_surface.frame` callbacks — which is
    // effectively every real client) draws once and then waits forever
    // for a callback that never comes, looking exactly like "doesn't
    // work". `flush_clients` matters just as much: `dispatch_clients`
    // (wired up in `state.rs`'s `init_wayland_listener`) only reads
    // *incoming* client requests — nothing anywhere else flushes
    // *outgoing* protocol messages (configures, frame callbacks, ...) to
    // client sockets under this backend.
    state.space.elements().for_each(|window| {
        window.send_frame(
            &output,
            state.start_time.elapsed(),
            Some(std::time::Duration::ZERO),
            |_, _| Some(output.clone()),
        )
    });
    state.space.refresh();
    state.popups.cleanup();
    // `session_destroyed` only removes mudhuts' own owned `Session`s
    // (`state.image_copy_sessions`) — it doesn't touch `ImageCopyCaptureState`'s
    // separate internal tracking Vecs, so those need this periodic sweep too.
    state.image_copy_capture_state.cleanup();
    let _ = state.display_handle.flush_clients();
}

/// Build this frame's cursor render element(s) at `state.pointer_location`
/// — either the loaded Xcursor theme's current frame (the common case,
/// `CursorImageStatus::Named`) or a client-provided cursor surface
/// (`CursorImageStatus::Surface`, set via `wl_pointer.set_cursor`), or
/// nothing at all if the client asked to hide the cursor.
fn build_cursor_elements(
    state: &mut State,
    renderer: &mut GlesRenderer,
    pointer_image: &Cursor,
    pointer_element: &mut PointerElement,
    pointer_image_cache: &mut Vec<(xcursor::parser::Image, MemoryRenderBuffer)>,
) -> Vec<Elements> {
    // A client's cursor surface can be destroyed without the client ever
    // telling us to switch away from it (e.g. the client itself exits) —
    // fall back rather than keep pointing at a dead surface.
    if let CursorImageStatus::Surface(surface) = &state.cursor_status
        && !surface.alive()
    {
        state.cursor_status = CursorImageStatus::default_named();
    }

    let hotspot = match &state.cursor_status {
        CursorImageStatus::Surface(surface) => with_states(surface, |states| {
            states
                .data_map
                .get::<std::sync::Mutex<CursorImageAttributes>>()
                .and_then(|attrs| attrs.lock().ok())
                .map(|guard| guard.hotspot)
                .unwrap_or_default()
        }),
        _ => match pointer_image.frame(1, state.start_time.elapsed()) {
            Some(image) => {
                let buffer = pointer_image_cache
                    .iter()
                    .find(|(cached, _)| cached == image)
                    .map(|(_, buffer)| buffer.clone())
                    .unwrap_or_else(|| {
                        let buffer = MemoryRenderBuffer::from_slice(
                            &image.pixels_rgba,
                            Fourcc::Argb8888,
                            (image.width as i32, image.height as i32),
                            1,
                            Transform::Normal,
                            None,
                        );
                        pointer_image_cache.push((image.clone(), buffer.clone()));
                        buffer
                    });
                pointer_element.set_buffer(buffer);
                Point::from((image.xhot as i32, image.yhot as i32))
            }
            None => Point::from((0, 0)),
        },
    };

    pointer_element.set_status(state.cursor_status.clone());

    let cursor_pos = (state.pointer_location - hotspot.to_f64())
        .to_physical(Scale::from(1.0))
        .to_i32_round();

    pointer_element.render_elements(renderer, cursor_pos, Scale::from(1.0), 1.0)
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

    render_surface(state, inner, crtc);
}
