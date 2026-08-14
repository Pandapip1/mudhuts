//! Wayland client half of `mudhuts-portal`: connects to whatever
//! compositor `WAYLAND_DISPLAY` points to (mudhuts, in the intended
//! deployment) as a completely normal client — no special privilege, same
//! pattern as `mudhuts-authority-helper` — and does one-shot whole-output
//! screenshot capture via `ext-image-copy-capture-v1` +
//! `ext-output-image-capture-source-v1`. This is the exact client-side
//! counterpart of what `mudhuts/src/handlers/capture.rs` implements
//! server-side: SHM buffers only, whole-output only, no dmabuf, no
//! per-toplevel/region capture.
//!
//! Runs on its own OS thread with its own connection and event queue,
//! driven by a blocking dispatch loop. The async D-Bus side
//! (`screenshot.rs`) submits [`Job`]s over a channel and awaits the reply
//! on a oneshot channel, so a slow or wedged Wayland round-trip can never
//! block the zbus executor or any other in-flight D-Bus call.

use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};

use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    self, ExtImageCopyCaptureManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self, ExtImageCopyCaptureSessionV1,
};

/// One captured frame, converted to tightly-packed 8-bit RGBA rows (top
/// row first) — ready to hand straight to a PNG encoder.
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Work submitted from the async D-Bus side to the Wayland thread.
/// `Screenshot` is one-shot; the `*Capture` jobs manage a single
/// long-lived [`CastSession`] for `screencast.rs`'s continuous PipeWire
/// producer — `StartCapture` creates it (returning the fixed
/// width/height frames will come in at), `CaptureFrame` pulls the next
/// frame from it (called once per `pipewire_stream`'s `process`
/// callback), and `StopCapture` tears it down. At most one capture
/// session exists at a time, matching mudhuts being single-output and
/// this backend only ever handling one active ScreenCast session.
pub enum Job {
    Screenshot(oneshot::Sender<Result<CapturedImage, String>>),
    StartCapture(oneshot::Sender<Result<(u32, u32), String>>),
    CaptureFrame(oneshot::Sender<Result<CapturedImage, String>>),
    StopCapture,
}

/// Everything the Wayland thread binds once at startup and reuses for
/// every capture. Any of these being `None` (compositor doesn't speak the
/// protocol, or isn't mudhuts) means capture requests fail cleanly with a
/// clear error rather than panicking.
struct AppState {
    output: Option<wl_output::WlOutput>,
    shm: Option<wl_shm::WlShm>,
    source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    /// The one live continuous-capture session, if `StartCapture` has run
    /// and `StopCapture` hasn't yet — see [`Job`]'s doc.
    cast: Option<CastSession>,
}

/// A long-lived `ext_image_copy_capture_session_v1` plus the one SHM
/// buffer every frame captures into. Reusing the same session and buffer
/// across many `CaptureFrame` jobs (rather than the create-session/
/// create-buffer/destroy-everything dance `capture_once` does per shot)
/// is what makes per-frame capture cheap enough to drive a video stream:
/// only a per-frame `create_frame`/`capture` round trip is needed, no
/// buffer reallocation. Safe to keep reading `mmap` after each capture
/// completes since `memmap2::Mmap::map` is a `MAP_SHARED` mapping — it
/// reflects whatever the compositor most recently wrote into the
/// underlying file, no remap needed between frames.
struct CastSession {
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    width: u32,
    height: u32,
    format: wl_shm::Format,
    buffer: wl_buffer::WlBuffer,
    mmap: memmap2::Mmap,
}

#[derive(Default)]
struct SessionCapture {
    width: u32,
    height: u32,
    shm_format: Option<wl_shm::Format>,
    done: bool,
    stopped: bool,
}

#[derive(Default)]
struct FrameCapture {
    ready: bool,
    failed: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Every global this process needs is bound once at startup right
        // after the initial roundtrip — dynamic add/remove afterward
        // (e.g. an output appearing later) isn't handled in this pass.
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, Arc<Mutex<SessionCapture>>> for AppState {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        data: &Arc<Mutex<SessionCapture>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        let mut s = lock(data);
        match event {
            Event::BufferSize { width, height } => {
                s.width = width;
                s.height = height;
            }
            // Prefer Argb8888 if it's on offer (real alpha channel);
            // otherwise keep whatever came first.
            Event::ShmFormat { format: WEnum::Value(format) }
                if s.shm_format.is_none() || format == wl_shm::Format::Argb8888 =>
            {
                s.shm_format = Some(format);
            }
            Event::Done => s.done = true,
            Event::Stopped => s.stopped = true,
            // dmabuf_device/dmabuf_format ignored — SHM only, matching
            // mudhuts' own server-side scope for this protocol.
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, Arc<Mutex<FrameCapture>>> for AppState {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        data: &Arc<Mutex<FrameCapture>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Ready => lock(data).ready = true,
            Event::Failed { reason } => {
                lock(data).failed = Some(format!("capture failed: {reason:?}"));
            }
            // transform/damage/presentation_time ignored: mudhuts always
            // reports Transform::Normal for capture (see its own
            // `capture.rs`), and this is a single whole-buffer capture so
            // partial damage regions carry no useful information here.
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(AppState: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(AppState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(AppState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(AppState: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(AppState: ignore ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(AppState: ignore ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(AppState: ignore ExtImageCopyCaptureManagerV1);

/// Recovers the guard on a poisoned mutex instead of panicking — this
/// state is only ever touched from this one thread, so poisoning
/// shouldn't happen, but a defensive recovery costs nothing and keeps
/// this in line with the project's no-panics convention.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drains `jobs` forever, replying to every one with `err` — used when
/// startup fails partway through so callers get a clean error instead of
/// hanging on a channel nothing will ever answer.
fn drain_with_error(mut jobs: mpsc::UnboundedReceiver<Job>, err: String) {
    while let Some(job) = jobs.blocking_recv() {
        match job {
            Job::Screenshot(reply) => {
                let _ = reply.send(Err(err.clone()));
            }
            Job::StartCapture(reply) => {
                let _ = reply.send(Err(err.clone()));
            }
            Job::CaptureFrame(reply) => {
                let _ = reply.send(Err(err.clone()));
            }
            Job::StopCapture => {}
        }
    }
}

/// Entry point for the dedicated Wayland thread. Never returns while the
/// process is alive (except on unrecoverable connection failure, in which
/// case it degrades to draining jobs with an error rather than exiting
/// the whole daemon over a Wayland hiccup — the D-Bus side stays up and
/// keeps answering Settings requests either way).
pub fn run(jobs: mpsc::UnboundedReceiver<Job>) {
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(err) => {
            tracing::error!(
                "mudhuts-portal: failed to connect to the Wayland display ({err}) — \
                 Screenshot will be unavailable, Settings/ScreenCast are unaffected"
            );
            return drain_with_error(jobs, format!("no Wayland connection: {err}"));
        }
    };

    let (globals, mut event_queue) = match registry_queue_init::<AppState>(&conn) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!("mudhuts-portal: failed to initialize Wayland globals: {err}");
            return drain_with_error(jobs, format!("failed to initialize Wayland globals: {err}"));
        }
    };
    let qh = event_queue.handle();

    let mut state = AppState {
        output: bind_optional(&globals, &qh, "wl_output"),
        shm: bind_optional(&globals, &qh, "wl_shm"),
        source_manager: bind_optional(&globals, &qh, "ext_output_image_capture_source_manager_v1"),
        capture_manager: bind_optional(&globals, &qh, "ext_image_copy_capture_manager_v1"),
        cast: None,
    };

    if let Err(err) = event_queue.roundtrip(&mut state) {
        tracing::warn!("mudhuts-portal: initial Wayland roundtrip failed: {err}");
    }

    tracing::info!("mudhuts-portal: Wayland client ready (screenshot capture available)");

    let mut jobs = jobs;
    while let Some(job) = jobs.blocking_recv() {
        match job {
            Job::Screenshot(reply) => {
                let result = capture_once(&mut state, &mut event_queue, &qh);
                if let Err(ref err) = result {
                    tracing::warn!("mudhuts-portal: screenshot capture failed: {err}");
                }
                let _ = reply.send(result);
            }
            Job::StartCapture(reply) => {
                let result = start_capture(&mut state, &mut event_queue, &qh);
                if let Err(ref err) = result {
                    tracing::warn!("mudhuts-portal: failed to start a capture session: {err}");
                }
                let _ = reply.send(result);
            }
            Job::CaptureFrame(reply) => {
                let result = capture_next_frame(&mut state, &mut event_queue, &qh);
                if let Err(ref err) = result {
                    tracing::warn!("mudhuts-portal: failed to capture a frame: {err}");
                }
                let _ = reply.send(result);
            }
            Job::StopCapture => {
                if let Some(cast) = state.cast.take() {
                    cast.buffer.destroy();
                    cast.session.destroy();
                    cast.source.destroy();
                }
            }
        }
    }
}

/// Small `globals.bind` wrapper that logs and returns `None` instead of
/// failing the whole thread when one optional global is missing (e.g.
/// this process is running under a compositor other than mudhuts, or an
/// older mudhuts without image-copy-capture support).
fn bind_optional<I>(
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<AppState>,
    name: &str,
) -> Option<I>
where
    I: wayland_client::Proxy + 'static,
    AppState: Dispatch<I, ()>,
{
    match globals.bind::<I, _, _>(qh, 1..=u32::MAX, ()) {
        Ok(global) => Some(global),
        Err(err) => {
            tracing::warn!("mudhuts-portal: {name} global not available ({err})");
            None
        }
    }
}

/// Does one full capture-session round trip: create a source + session
/// for the (single) output, wait for buffer constraints, allocate a
/// matching SHM buffer, capture one frame, and convert it to RGBA8.
/// Returns a plain `String` error rather than panicking anywhere — every
/// step here can legitimately fail (compositor race, backend hiccup) and
/// none of that should be able to take the whole daemon down.
fn capture_once(
    state: &mut AppState,
    event_queue: &mut EventQueue<AppState>,
    qh: &QueueHandle<AppState>,
) -> Result<CapturedImage, String> {
    let output = state.output.clone().ok_or("no wl_output available")?;
    let source_manager = state
        .source_manager
        .clone()
        .ok_or("ext_output_image_capture_source_manager_v1 not available — is this running under mudhuts?")?;
    let capture_manager = state
        .capture_manager
        .clone()
        .ok_or("ext_image_copy_capture_manager_v1 not available — is this running under mudhuts?")?;
    let shm = state.shm.clone().ok_or("wl_shm not available")?;

    let source = source_manager.create_source(&output, qh, ());
    let session_data = Arc::new(Mutex::new(SessionCapture::default()));
    let session = capture_manager.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::empty(),
        qh,
        session_data.clone(),
    );

    loop {
        event_queue
            .blocking_dispatch(state)
            .map_err(|err| format!("Wayland dispatch error while awaiting capture constraints: {err}"))?;
        let s = lock(&session_data);
        if s.stopped {
            return Err("capture session stopped before buffer constraints arrived".to_string());
        }
        if s.done {
            break;
        }
    }

    let (width, height, format) = {
        let s = lock(&session_data);
        let format = s
            .shm_format
            .ok_or("compositor advertised no SHM format for this capture session")?;
        if s.width == 0 || s.height == 0 {
            return Err("compositor advertised a zero-sized capture buffer".to_string());
        }
        (s.width, s.height, format)
    };

    let stride = width * 4;
    let size = (stride as u64) * (height as u64);

    let file = tempfile::tempfile().map_err(|err| format!("failed to create an SHM backing file: {err}"))?;
    file.set_len(size)
        .map_err(|err| format!("failed to size the SHM backing file: {err}"))?;
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
    pool.destroy();

    let frame_data = Arc::new(Mutex::new(FrameCapture::default()));
    let frame = session.create_frame(qh, frame_data.clone());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();

    loop {
        event_queue
            .blocking_dispatch(state)
            .map_err(|err| format!("Wayland dispatch error while awaiting the captured frame: {err}"))?;
        let f = lock(&frame_data);
        if let Some(reason) = &f.failed {
            return Err(reason.clone());
        }
        if f.ready {
            break;
        }
    }

    // Safety: `file` is a plain, fully-sized regular file this process
    // owns exclusively at this point (the compositor writes into it via
    // the fd it was sent, which is the same shared mapping) — no other
    // thread in this process touches it.
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|err| format!("failed to map the SHM buffer for readback: {err}"))?;
    let rgba = convert_to_rgba(&mmap, width, height, stride, format)?;

    buffer.destroy();
    frame.destroy();
    session.destroy();
    source.destroy();

    Ok(CapturedImage { width, height, rgba })
}

/// Creates the long-lived session + SHM buffer a stream of `CaptureFrame`
/// jobs will reuse — everything `capture_once` does up through buffer
/// allocation, minus the per-shot frame capture and minus tearing any of
/// it down afterward. Returns the fixed `(width, height)` every
/// subsequent frame will come in at. Fails if a session is already
/// active — `screencast.rs` never starts a second one without stopping
/// the first (mudhuts is single-output, so there's nothing a second
/// session could target differently anyway).
fn start_capture(
    state: &mut AppState,
    event_queue: &mut EventQueue<AppState>,
    qh: &QueueHandle<AppState>,
) -> Result<(u32, u32), String> {
    if state.cast.is_some() {
        return Err("a capture session is already active".to_string());
    }

    let output = state.output.clone().ok_or("no wl_output available")?;
    let source_manager = state
        .source_manager
        .clone()
        .ok_or("ext_output_image_capture_source_manager_v1 not available — is this running under mudhuts?")?;
    let capture_manager = state
        .capture_manager
        .clone()
        .ok_or("ext_image_copy_capture_manager_v1 not available — is this running under mudhuts?")?;
    let shm = state.shm.clone().ok_or("wl_shm not available")?;

    let source = source_manager.create_source(&output, qh, ());
    let session_data = Arc::new(Mutex::new(SessionCapture::default()));
    let session = capture_manager.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::empty(),
        qh,
        session_data.clone(),
    );

    loop {
        event_queue
            .blocking_dispatch(state)
            .map_err(|err| format!("Wayland dispatch error while awaiting capture constraints: {err}"))?;
        let s = lock(&session_data);
        if s.stopped {
            return Err("capture session stopped before buffer constraints arrived".to_string());
        }
        if s.done {
            break;
        }
    }

    let (width, height, format) = {
        let s = lock(&session_data);
        let format = s
            .shm_format
            .ok_or("compositor advertised no SHM format for this capture session")?;
        if s.width == 0 || s.height == 0 {
            return Err("compositor advertised a zero-sized capture buffer".to_string());
        }
        (s.width, s.height, format)
    };

    let stride = width * 4;
    let size = (stride as u64) * (height as u64);

    let file = tempfile::tempfile().map_err(|err| format!("failed to create an SHM backing file: {err}"))?;
    file.set_len(size)
        .map_err(|err| format!("failed to size the SHM backing file: {err}"))?;
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
    pool.destroy();

    // Safety: same reasoning as `capture_once` — this is a plain,
    // fully-sized regular file this process owns exclusively, mapped
    // `MAP_SHARED` so it keeps reflecting whatever the compositor writes
    // into it on every subsequent capture.
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|err| format!("failed to map the SHM buffer for readback: {err}"))?;

    state.cast = Some(CastSession { source, session, width, height, format, buffer, mmap });
    Ok((width, height))
}

/// Captures exactly one more frame on the session `start_capture`
/// created — the steady-state operation `pipewire_stream`'s `process`
/// callback drives once per PipeWire graph cycle. Reuses the same SHM
/// buffer every time; safe because each call fully waits for `Ready`
/// (compositor done writing) before reading it, and the previous frame
/// object is destroyed before the caller could plausibly still be
/// reading stale data from it.
fn capture_next_frame(
    state: &mut AppState,
    event_queue: &mut EventQueue<AppState>,
    qh: &QueueHandle<AppState>,
) -> Result<CapturedImage, String> {
    let (session, buffer, width, height, format) = {
        let cast = state.cast.as_ref().ok_or("no active capture session")?;
        (cast.session.clone(), cast.buffer.clone(), cast.width, cast.height, cast.format)
    };

    let frame_data = Arc::new(Mutex::new(FrameCapture::default()));
    let frame = session.create_frame(qh, frame_data.clone());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();

    loop {
        event_queue
            .blocking_dispatch(state)
            .map_err(|err| format!("Wayland dispatch error while awaiting the captured frame: {err}"))?;
        let f = lock(&frame_data);
        if let Some(reason) = &f.failed {
            return Err(reason.clone());
        }
        if f.ready {
            break;
        }
    }
    frame.destroy();

    let cast = state.cast.as_ref().ok_or("capture session disappeared mid-frame")?;
    let rgba = convert_to_rgba(&cast.mmap, width, height, width * 4, format)?;
    Ok(CapturedImage { width, height, rgba })
}

/// Converts a native-endian `wl_shm` ARGB8888/XRGB8888 buffer (in memory
/// on a little-endian host, that's byte order B,G,R,A) into tightly
/// packed RGBA8 — the layout the `png` crate wants. XRGB8888's alpha byte
/// is unspecified per the format's own definition, so it's always forced
/// to fully opaque rather than trusted.
fn convert_to_rgba(
    mmap: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
    for y in 0..height {
        let row_start = (y * stride) as usize;
        for x in 0..width {
            let px = row_start + (x * 4) as usize;
            if px + 4 > mmap.len() {
                return Err("captured SHM buffer is shorter than its advertised size".to_string());
            }
            let (b, g, r) = (mmap[px], mmap[px + 1], mmap[px + 2]);
            let a = if format == wl_shm::Format::Argb8888 { mmap[px + 3] } else { 0xFF };
            let out_idx = ((y * width + x) * 4) as usize;
            out[out_idx] = r;
            out[out_idx + 1] = g;
            out[out_idx + 2] = b;
            out[out_idx + 3] = a;
        }
    }
    Ok(out)
}
