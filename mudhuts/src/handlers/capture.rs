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
    /// buffer-size validation against a stale size. Per-session, not one
    /// shared push of the focused output's own mode — real multi-monitor:
    /// each session is bound to whichever specific output its own source
    /// was created for (recovered the same way `capture_constraints`
    /// already does, via the `WeakOutput` `output_source_created` stashed
    /// in the source's `user_data()`), not necessarily the focused one.
    pub fn refresh_capture_constraints(&mut self) {
        for session in &self.image_copy_sessions {
            let Some(output) = session.source().user_data().get::<WeakOutput>().and_then(WeakOutput::upgrade)
            else {
                continue;
            };
            let Some(mode) = output.current_mode() else {
                continue;
            };
            session.update_constraints(buffer_constraints_for_mode(mode));
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
        // The specific output this session's own source was created for
        // — same `WeakOutput` recovery `capture_constraints` already
        // does correctly. Not `self.output` (the *focused* output):
        // real multi-monitor means a capture session for a non-focused
        // monitor must still render *that* monitor's own content, not
        // whichever one the user currently has input focus on.
        let output = session
            .source()
            .user_data()
            .get::<WeakOutput>()
            .and_then(WeakOutput::upgrade)
            .ok_or(FailureReason::Unknown)?;
        // KNOWN TRADEOFF, not yet addressed: `output_index_for` is an
        // O(outputs) linear scan, paid on every captured frame (an active
        // screencast/screen-share is typically 30-60fps) where the old
        // single-output code just read `self.output` directly. Left
        // as-is: realistic output counts (a handful of monitors) make
        // this a small constant-factor cost, unlike the graph-wide scans
        // this codebase's other hot-path perf fixes targeted — not worth
        // the complexity of caching an output_index on the session
        // without a clear invalidation story for hotplug/renumbering.
        let output_index = self.stack.output_index_for(&output).ok_or(FailureReason::Unknown)?;
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
        // `RefCell` panics on a second concurrent borrow. `output_index`,
        // not always `0` — this session's own output, resolved above.
        // `begin_frame` here since this is its own resolve pass,
        // independent of whatever frame the render loop last built — see
        // `Graph::begin_frame`'s doc comment.
        self.stack.begin_frame();
        let content = render::resolve_frame_content(self, output_index);

        let pixels = if let Some(renderer) = self.dmabuf_renderer.clone() {
            let mut renderer = renderer.borrow_mut();
            self.render_capture(&mut renderer, size, fourcc, tracker, content, output_index)?
        } else if let Some(backend) = self.winit_backend.clone() {
            let mut backend = backend.borrow_mut();
            self.render_capture(backend.renderer(), size, fourcc, tracker, content, output_index)?
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
        output_index: usize,
    ) -> Result<Vec<u8>, FailureReason> {
        // A plain local `Vec`, not a persistent scratch buffer — unlike
        // the real per-frame render path (`winit_backend.rs`/
        // `udev_backend.rs`, which reuse one via `build_frame_elements`'s
        // `elements` out-param), a screenshot capture is a rare, one-off
        // event, not something worth threading a long-lived buffer
        // through `State` for.
        let mut elements = Vec::new();
        render::build_frame_elements(self, renderer, size, content, output_index, &mut elements);

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

/// One row's byte ranges within the tightly-packed source and the
/// (possibly differently-strided) destination, for every row that
/// actually fits within both `src_len` and `dst_len` — stops at the
/// first row that wouldn't (shouldn't happen once Smithay's own
/// `validate_buffer` has run before calling into this handler, but
/// stays panic-free rather than assuming that invariant, per project
/// convention) and every row after, since both offsets only grow with
/// `row`. Pulled out of `write_shm_buffer` as pure arithmetic over
/// lengths/strides so the bounds logic is directly testable without a
/// real `WlBuffer`. Returns a lazy iterator, not a `Vec` — `capture_frame`
/// calls this once per captured frame (an active screencast is typically
/// 30-60fps), so this needs to stay allocation-free like the rest of
/// that hot path.
fn shm_copy_plan(
    size: (i32, i32),
    offset: i32,
    stride: i32,
    src_len: usize,
    dst_len: usize,
) -> impl Iterator<Item = (std::ops::Range<usize>, std::ops::Range<usize>)> {
    let row_bytes = size.0 as usize * 4;
    let height = size.1 as usize;
    let offset = offset as usize;
    let stride = stride as usize;

    (0..height)
        .map(move |row| {
            let src_start = row * row_bytes;
            let dst_start = offset + row * stride;
            (src_start..src_start + row_bytes, dst_start..dst_start + row_bytes)
        })
        .take_while(move |(src_range, dst_range)| src_range.end <= src_len && dst_range.end <= dst_len)
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
    with_buffer_contents_mut(buffer, |ptr, len, _| {
        // Safety: `ptr`/`len` describe the client's SHM pool for exactly as
        // long as this closure runs (see `with_buffer_contents_mut`'s own
        // safety doc) — never stored past this call.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        for (src_range, dst_range) in shm_copy_plan(size, offset, stride, pixels.len(), dst.len()) {
            dst[dst_range].copy_from_slice(&pixels[src_range]);
        }
    })
    .map_err(|err| {
        tracing::warn!("capture: failed to write SHM buffer: {err}");
        FailureReason::Unknown
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tightly_packed_destination_copies_every_row_contiguously() {
        let row_bytes = 4 * 4; // 4px wide, 4 bytes/px
        let plan: Vec<_> = shm_copy_plan((4, 3), 0, row_bytes as i32, row_bytes * 3, row_bytes * 3).collect();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0], (0..row_bytes, 0..row_bytes));
        assert_eq!(plan[1], (row_bytes..2 * row_bytes, row_bytes..2 * row_bytes));
        assert_eq!(plan[2], (2 * row_bytes..3 * row_bytes, 2 * row_bytes..3 * row_bytes));
    }

    #[test]
    fn a_wider_destination_stride_leaves_a_gap_between_destination_rows() {
        let row_bytes = 16;
        let stride = 32; // padded to double the tightly-packed width
        let plan: Vec<_> = shm_copy_plan((4, 2), 0, stride, row_bytes * 2, stride as usize * 2).collect();
        assert_eq!(plan[0].1, 0..row_bytes);
        assert_eq!(plan[1].1, stride as usize..stride as usize + row_bytes);
        // The source side stays tightly packed regardless of the
        // destination's stride.
        assert_eq!(plan[1].0, row_bytes..2 * row_bytes);
    }

    #[test]
    fn a_nonzero_offset_shifts_every_destination_row() {
        let row_bytes = 16;
        let offset = 100;
        let plan: Vec<_> =
            shm_copy_plan((4, 2), offset, row_bytes as i32, row_bytes * 2, offset as usize + row_bytes * 2).collect();
        assert_eq!(plan[0].1, offset as usize..offset as usize + row_bytes);
        assert_eq!(plan[1].1, offset as usize + row_bytes..offset as usize + 2 * row_bytes);
    }

    #[test]
    fn stops_before_a_row_that_would_overrun_the_source() {
        let row_bytes = 16;
        // Only enough source bytes for 2 of the requested 5 rows.
        let plan: Vec<_> = shm_copy_plan((4, 5), 0, row_bytes as i32, row_bytes * 2, row_bytes * 10).collect();
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn stops_before_a_row_that_would_overrun_the_destination() {
        let row_bytes = 16;
        let plan: Vec<_> = shm_copy_plan((4, 5), 0, row_bytes as i32, row_bytes * 10, row_bytes * 2).collect();
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn zero_height_produces_an_empty_plan() {
        assert!(shm_copy_plan((4, 0), 0, 16, 0, 0).next().is_none());
    }
}
