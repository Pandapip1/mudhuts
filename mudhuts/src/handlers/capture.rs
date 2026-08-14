//! `ext-image-copy-capture-v1` + `ext-output-image-capture-source-v1`
//! (screenshot capture) — whole-output only, SHM buffers only. No dmabuf
//! export and no toplevel/region capture in v1 (see the plan notes on why
//! that's deliberately out of scope for now).
//!
//! Smithay's own protocol plumbing (`smithay::wayland::image_copy_capture`/
//! `image_capture_source`) handles the whole request/event dance and buffer
//! validation; everything below this module doc is the part Smithay leaves
//! to the compositor: what a capture *source* actually refers to, and how
//! to actually produce pixels for a capture *frame*.
//!
//! ## Why a second render pass
//!
//! There is no framebuffer-aliasing shortcut that generalizes across both
//! backends here: under udev/DRM, `DrmCompositor::render_frame` owns its own
//! GBM swapchain internally and never exposes a bindable target that could
//! be read back into an *external* client buffer; under winit,
//! `ExportMem::copy_framebuffer` on the primary framebuffer would in
//! principle be reachable right after `render_output` but before `submit`,
//! but that's backend-asymmetric and capture is client-pull-driven (fires
//! from a `Dispatch` callback, not tied to either backend's redraw tick)
//! rather than something that can piggyback on whichever redraw happened to
//! run most recently. So every capture does a genuine second render pass:
//! bind an offscreen `GlesTexture`, run the exact same element list
//! `render::build_frame_elements` would hand the real redraw through
//! `OutputDamageTracker::render_output`, then read it back with
//! `ExportMem::copy_framebuffer`/`map_texture` and copy the bytes into the
//! client's SHM buffer. Reusing `build_frame_elements` (rather than
//! building a separate element list for capture) is what guarantees a
//! screenshot can never drift from what's actually on screen.

use std::cell::RefCell;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
use smithay::output::{Mode, Output, WeakOutput};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Buffer as BufferCoord, Rectangle, Size, Transform};
use smithay::wayland::image_capture_source::{
    ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler, OutputCaptureSourceState,
};
use smithay::wayland::image_copy_capture::{
    BufferConstraints, CaptureFailureReason as FailureReason, Frame, ImageCopyCaptureHandler,
    ImageCopyCaptureState, Session, SessionRef,
};
use smithay::wayland::shm::{shm_format_to_fourcc, with_buffer_contents, with_buffer_contents_mut};

use crate::State;
use crate::render;

/// Buffer constraints mudhuts advertises for output capture: exactly the
/// output's current pixel size, in either of the two SHM formats every
/// client-side screenshot tool already knows how to allocate. No dmabuf
/// (`dma: None`) — see this module's doc on scope.
fn buffer_constraints_for_mode(mode: Mode) -> BufferConstraints {
    BufferConstraints {
        size: mode.size.to_logical(1).to_buffer(1, Transform::Normal),
        shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
        dma: None,
    }
}

impl ImageCaptureSourceHandler for State {}

impl OutputCaptureSourceHandler for State {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        // Stashes which `Output` this opaque source refers to, recovered
        // later by `capture_constraints`/`frame` via the same key — the one
        // correct line in anvil's otherwise-nonfunctional reference impl.
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let output = source.user_data().get::<WeakOutput>()?.upgrade()?;
        let mode = output.current_mode()?;
        Some(buffer_constraints_for_mode(mode))
    }

    fn new_session(&mut self, session: Session) {
        // Must be kept alive: `Session`'s `Drop` impl immediately sends
        // `stopped()` and fails every pending frame otherwise (see this
        // protocol's own module doc) — exactly the mistake anvil's stub
        // makes by dropping the session it's handed straight away.
        self.image_copy_sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        match self.capture_frame(session, &frame) {
            Ok(()) => frame.success(Transform::Normal, None, self.start_time.elapsed()),
            Err(reason) => frame.fail(reason),
        }
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.image_copy_sessions.retain(|s| s.as_ref() != session);
    }
}

impl State {
    /// Re-push buffer constraints to every live capture session — nothing
    /// else does this automatically (`SessionRef::update_constraints` only
    /// ever runs once, at session creation, unless a handler calls it
    /// again), so without calling this on every output resize a session
    /// outlives the first resize and every later capture attempt fails
    /// buffer-size validation against a stale size. Only ever one output in
    /// this compositor (see the module doc), so unconditionally pushing the
    /// same constraints to every live session is correct.
    pub fn refresh_capture_constraints(&mut self) {
        let Some(mode) = self.output.as_ref().and_then(Output::current_mode) else {
            return;
        };
        let constraints = buffer_constraints_for_mode(mode);
        for session in &self.image_copy_sessions {
            session.update_constraints(constraints.clone());
        }
    }

    /// Do the actual capture work for one `Frame`: render a second time into
    /// an offscreen buffer, then copy the result into the client's SHM
    /// buffer. Returns the [`FailureReason`] to report on any error rather
    /// than panicking anywhere along the way (renderer/import failures are
    /// always plausible — a client can legitimately race a mode change, a
    /// backend can legitimately run out of GPU memory — never something to
    /// crash the whole compositor over).
    fn capture_frame(&mut self, session: &SessionRef, frame: &Frame) -> Result<(), FailureReason> {
        // Single-output compositor: `session` itself has nothing more to
        // key off of than the source already validated via
        // `capture_constraints` — only its `user_data()` (the per-session
        // damage tracker below) is actually needed here.
        let output = self.output.clone().ok_or(FailureReason::Unknown)?;
        let mode = output.current_mode().ok_or(FailureReason::Unknown)?;
        let size = (mode.size.w, mode.size.h);

        // `Frame::buffer()` panics if called with nothing attached, but
        // Smithay already validated a buffer is attached (and matches this
        // session's constraints) before ever calling `ImageCopyCaptureHandler::frame`.
        let buffer = frame.buffer();
        let shm_info =
            with_buffer_contents(&buffer, |_, _, data| data).map_err(|_| FailureReason::Unknown)?;
        let fourcc = shm_format_to_fourcc(shm_info.format).ok_or(FailureReason::BufferConstraints)?;

        // One `OutputDamageTracker` per session, reused across repeated
        // captures on the same session (matches `OutputDamageTracker::
        // render_output`'s own doc comment on what per-session reuse is
        // for) rather than allocated fresh every call.
        let tracker = session
            .user_data()
            .get_or_insert(|| RefCell::new(OutputDamageTracker::from_output(&output)));

        // Resolved *before* either branch below acquires its own borrow
        // of the (possibly shared) renderer — see
        // `render::resolve_frame_content`'s own doc comment for why that
        // ordering isn't optional: it internally borrows the same
        // `Rc<RefCell<GlesRenderer>>` `self.dmabuf_renderer` shares, and
        // `RefCell` panics on a second concurrent borrow.
        // Screenshot capture always targets the focused output (index 0's
        // slot is not assumed — `self.output` above already resolved to
        // whichever output this session is bound to); per-output capture
        // selection is a separate, not-yet-requested feature. `begin_frame`
        // here since this is its own resolve pass, independent of whatever
        // frame the render loop last built — see `Graph::begin_frame`'s doc
        // comment.
        self.stack.begin_frame();
        let content = render::resolve_frame_content(self, 0);

        let pixels = if let Some(renderer) = self.dmabuf_renderer.clone() {
            let mut renderer = renderer.borrow_mut();
            self.render_capture(&mut renderer, size, fourcc, tracker, content)?
        } else if let Some(backend) = self.winit_backend.clone() {
            let mut backend = backend.borrow_mut();
            self.render_capture(backend.renderer(), size, fourcc, tracker, content)?
        } else {
            // Shouldn't happen — one of the two is always set once either
            // backend has finished starting up — but stay panic-free.
            return Err(FailureReason::Unknown);
        };

        write_shm_buffer(&buffer, &pixels, size, shm_info.offset, shm_info.stride)
    }

    /// Render `output`'s current content into a fresh offscreen texture
    /// (reusing `render::build_frame_elements` — see this module's doc on
    /// why that's the part that keeps a screenshot honest) and read it back
    /// into an owned byte buffer. `fourcc` is the client's actual buffer
    /// format (either of the two offered in `capture_constraints`), used
    /// both as the offscreen target's storage format and the format
    /// `copy_framebuffer` converts into on readback, so what comes back
    /// already matches what the client asked for.
    fn render_capture(
        &mut self,
        renderer: &mut GlesRenderer,
        size: (i32, i32),
        fourcc: Fourcc,
        tracker: &RefCell<OutputDamageTracker>,
        content: Vec<crate::graph::ContentPiece>,
    ) -> Result<Vec<u8>, FailureReason> {
        let elements = render::build_frame_elements(self, renderer, size, content, 0);

        let buffer_size: Size<i32, BufferCoord> = (size.0, size.1).into();
        let mut texture = Offscreen::<GlesTexture>::create_buffer(renderer, fourcc, buffer_size)
            .map_err(|err| {
                tracing::warn!("capture: failed to create offscreen buffer: {err}");
                FailureReason::Unknown
            })?;
        let mut target = renderer.bind(&mut texture).map_err(|err| {
            tracing::warn!("capture: failed to bind offscreen buffer: {err}");
            FailureReason::Unknown
        })?;

        // `age: 0` unconditionally forces Smithay's damage tracker to treat
        // the whole output as damaged (see its own age-handling logic) —
        // exactly what's wanted here, since the offscreen texture backing
        // this capture never carries over content from a previous one.
        tracker
            .borrow_mut()
            .render_output(renderer, &mut target, 0, &elements, [0.0, 0.0, 0.0, 1.0])
            .map_err(|err| {
                tracing::warn!("capture: render_output failed: {err}");
                FailureReason::Unknown
            })?;

        let region = Rectangle::from_size(buffer_size);
        let mapping = renderer.copy_framebuffer(&target, region, fourcc).map_err(|err| {
            tracing::warn!("capture: copy_framebuffer failed: {err}");
            FailureReason::Unknown
        })?;
        renderer.map_texture(&mapping).map(<[u8]>::to_vec).map_err(|err| {
            tracing::warn!("capture: map_texture failed: {err}");
            FailureReason::Unknown
        })
    }
}

/// Copy `pixels` (tightly-packed, `size.0 * size.1 * 4` bytes, one row after
/// another) into the client's SHM buffer at its real `offset`/`stride` —
/// never assumed to equal `width * 4`, per the client's own pool layout.
fn write_shm_buffer(
    buffer: &WlBuffer,
    pixels: &[u8],
    size: (i32, i32),
    offset: i32,
    stride: i32,
) -> Result<(), FailureReason> {
    let row_bytes = size.0 as usize * 4;
    let height = size.1 as usize;
    let offset = offset as usize;
    let stride = stride as usize;

    with_buffer_contents_mut(buffer, |ptr, len, _| {
        // Safety: `ptr`/`len` describe the client's SHM pool for exactly as
        // long as this closure runs (see `with_buffer_contents_mut`'s own
        // safety doc) — never stored past this call.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        for row in 0..height {
            let src_start = row * row_bytes;
            let dst_start = offset + row * stride;
            if src_start + row_bytes > pixels.len() || dst_start + row_bytes > dst.len() {
                // Shouldn't happen once Smithay's own `validate_buffer` has
                // run before calling into this handler — stay panic-free
                // rather than assume it, per project convention.
                break;
            }
            dst[dst_start..dst_start + row_bytes].copy_from_slice(&pixels[src_start..src_start + row_bytes]);
        }
    })
    .map_err(|err| {
        tracing::warn!("capture: failed to write SHM buffer: {err}");
        FailureReason::Unknown
    })
}
