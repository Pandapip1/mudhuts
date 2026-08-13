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
//! import, and real xcursor-theme cursor rendering).
//!
//! The page-flip loop is self-perpetuating and naturally vsync-throttled
//! (each `DrmEvent::VBlank` completion schedules the next repaint) — a
//! fundamentally different, correctly-paced mechanism from the
//! `redraw_ping`/`Ping` demand-driven model Phase 2.6 built for the winit
//! path (that fix targeted winit's `request_redraw()` having *no*
//! natural rate limit). This backend does not use `State::
//! request_redraw()`/`redraw_ping` at all — the passed-in
//! `redraw_ping_source` is accepted only for signature symmetry with
//! `init_winit` and is intentionally never inserted into the event loop.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::{GbmFramebufferExporter, NodeFilter};
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties};
use smithay::reexports::calloop::ping::PingSource;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, LoopHandle};
use smithay::reexports::drm::control::{ModeTypeFlags, connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::State;
use crate::render::{self, OutputRenderElements};

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type OutputManager = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type CrtcOutput = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;
type Elements = OutputRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

struct SurfaceData {
    output: Output,
    drm_output: CrtcOutput,
}

struct Inner {
    renderer: GlesRenderer,
    drm_output_manager: OutputManager,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
}

pub fn init_udev(
    event_loop: &mut EventLoop<'static, State>,
    state: &mut State,
    _redraw_ping_source: PingSource,
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

    let inner = Rc::new(RefCell::new(Inner {
        renderer,
        drm_output_manager,
        drm_scanner: DrmScanner::new(),
        surfaces: HashMap::new(),
    }));

    let loop_handle = event_loop.handle();

    // DRM device: vblank completion drives the self-perpetuating
    // page-flip loop (see the module doc).
    {
        let inner = inner.clone();
        let loop_handle = loop_handle.clone();
        event_loop
            .handle()
            .insert_source(drm_notifier, move |event, _, state| match event {
                DrmEvent::VBlank(crtc) => frame_finish(state, &inner, &loop_handle, crtc),
                DrmEvent::Error(err) => tracing::warn!("DRM device error: {err}"),
            })
            .map_err(|err| format!("failed to register the DRM event source: {err}"))?;
    }

    // Session pause/resume (VT switch away/back).
    {
        let inner = inner.clone();
        let loop_handle = loop_handle.clone();
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
                        render_surface(state, &inner, &loop_handle, crtc);
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
        let result = inner_mut
            .drm_output_manager
            .lock()
            .initialize_output::<GlesRenderer, Elements>(
                crtc,
                drm_mode,
                &[connector.handle()],
                &output,
                None,
                &mut inner_mut.renderer,
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

    inner
        .borrow_mut()
        .surfaces
        .insert(crtc, SurfaceData { output, drm_output });

    // Deferred to the next event loop iteration (matches anvil's own
    // reference pattern) rather than called synchronously here, still
    // nested inside the same call stack as the modeset that
    // `initialize_output` just performed — giving the display
    // controller's own (asynchronous, coprocessor-driven) power-state
    // machine a chance to settle before another commit lands on it.
    let inner = inner.clone();
    let handle_clone = handle.clone();
    handle.insert_idle(move |state| {
        render_surface(state, &inner, &handle_clone, crtc);
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

fn render_surface(
    state: &mut State,
    inner: &Rc<RefCell<Inner>>,
    handle: &LoopHandle<'static, State>,
    crtc: crtc::Handle,
) {
    let mut inner_mut = inner.borrow_mut();
    let Inner {
        renderer, surfaces, ..
    } = &mut *inner_mut;
    let Some(surface) = surfaces.get_mut(&crtc) else {
        tracing::debug!("render_surface: no surface for {crtc:?}, dropping the render chain here");
        return;
    };

    let size = surface
        .output
        .current_mode()
        .map(|mode| (mode.size.w, mode.size.h))
        .unwrap_or((0, 0));
    let output = surface.output.clone();

    let elements = render::build_frame_elements(state, renderer, &output, size);

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
                if let Err(err) = surface.drm_output.queue_frame(()) {
                    tracing::warn!("failed to queue DRM frame: {err}");
                }
            } else {
                tracing::debug!("render_surface: no damage for {crtc:?}, rescheduling in 16ms");
                // No damage — re-check for it in about a frame rather
                // than busy-looping (trimmed of anvil's metadata-driven
                // latency tuning; see the module doc).
                reschedule(inner, handle, crtc, Duration::from_millis(16));
            }
        }
        Err(err) => tracing::warn!("render_frame failed: {err}"),
    }
}

fn frame_finish(state: &mut State, inner: &Rc<RefCell<Inner>>, handle: &LoopHandle<'static, State>, crtc: crtc::Handle) {
    tracing::debug!("frame_finish: vblank for {crtc:?}");
    let submitted = {
        let mut inner_mut = inner.borrow_mut();
        let Some(surface) = inner_mut.surfaces.get_mut(&crtc) else {
            tracing::debug!("frame_finish: no surface for {crtc:?}, dropping the render chain here");
            return;
        };
        surface.drm_output.frame_submitted()
    };
    if let Err(err) = submitted {
        tracing::warn!("frame_submitted failed: {err}");
    }

    render_surface(state, inner, handle, crtc);
}

fn reschedule(inner: &Rc<RefCell<Inner>>, handle: &LoopHandle<'static, State>, crtc: crtc::Handle, delay: Duration) {
    let inner = inner.clone();
    let handle_clone = handle.clone();
    let result = handle.insert_source(Timer::from_duration(delay), move |_, _, state| {
        tracing::debug!("reschedule: retry timer fired for {crtc:?}");
        render_surface(state, &inner, &handle_clone, crtc);
        TimeoutAction::Drop
    });
    if let Err(err) = result {
        tracing::warn!("failed to schedule a repaint retry: {err}");
    }
}
