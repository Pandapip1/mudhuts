//! A Hut: one built-in terminal plus (eventually) its Main Windows. Phase 1
//! only has a single Hut and doesn't yet organize client windows into it —
//! see the plan at `/home/gavin/.claude/plans/cryptic-honking-lamport.md`.

use std::sync::atomic::{AtomicU64, Ordering};

use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};

use mudhuts_term::{GlyphCache, TermEvent, Terminal};

use crate::gpu_term::GpuTermRenderer;

/// Initial grid size used before the real output size is known.
const INITIAL_COLS: usize = 80;
const INITIAL_LINES: usize = 24;

fn next_hut_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct Hut {
    /// Stable identity for this Hut, independent of its position in [The
    /// Stack](crate::stack::HutStack) (which shifts as entries are added
    /// and discarded) — used to route its `TermEvent` channel to the right
    /// entry once there are several.
    pub id: u64,
    pub terminal: Terminal,
    pub glyphs: GlyphCache,
    /// Whether this Hut has ever been interacted with (a keystroke sent to
    /// its terminal) since it was spawned. A freshly-spawned, never-touched
    /// Hut is discarded rather than kept around once The Stack moves away
    /// from it — see the plan's Phase 3 notes.
    touched: bool,
    /// Lazily created on first [`Hut::redraw`] call, once a renderer is
    /// actually available (Phase 1 spawns Huts before the winit backend
    /// exists).
    gpu: Option<GpuTermRenderer>,
    /// What `redraw` returned last time, reused when nothing changed
    /// (cheap: an `Arc` clone, not a re-render).
    last_texture: Option<GlesTexture>,
    pixel_size: (i32, i32),
    /// Stable identity for this Hut's terminal render element across
    /// frames (matters for the compositor's outer damage tracking, which
    /// compares elements by id between frames).
    pub element_id: Id,
}

impl Hut {
    /// Spawn a new Hut (shell + empty framebuffer). `extra_env` is set in
    /// the shell's environment only (see [`Terminal::spawn`] — notably,
    /// this is how mudhuts points the shell at its own Wayland socket
    /// without touching the compositor's own `WAYLAND_DISPLAY`, which the
    /// backend needs untouched to find whatever it's nested inside).
    /// Returns the Hut plus a channel the caller must insert into the
    /// calloop event loop to learn about terminal events (title changes,
    /// shell exit).
    pub fn spawn(
        extra_env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<
        (
            Hut,
            smithay::reexports::calloop::channel::Channel<TermEvent>,
        ),
        String,
    > {
        let glyphs = GlyphCache::new()?;
        let cell_size = (glyphs.cell_width() as u16, glyphs.cell_height() as u16);
        let (terminal, events) = Terminal::spawn(INITIAL_COLS, INITIAL_LINES, cell_size, extra_env)
            .map_err(|e| e.to_string())?;

        let pixel_size = (
            (INITIAL_COLS * cell_size.0 as usize) as i32,
            (INITIAL_LINES * cell_size.1 as usize) as i32,
        );

        Ok((
            Hut {
                id: next_hut_id(),
                terminal,
                glyphs,
                touched: false,
                gpu: None,
                last_texture: None,
                pixel_size,
                element_id: Id::new(),
            },
            events,
        ))
    }

    /// Whether this Hut has ever received a keystroke since it was
    /// spawned — see the `touched` field doc.
    pub fn touched(&self) -> bool {
        self.touched
    }

    /// Whatever [`Self::redraw`] last produced, without triggering a new
    /// render — for the Alt-Tab preview popup's thumbnails, which read
    /// every Hut's texture rather than just the focused one. `None` if
    /// this Hut has never been drawn yet (e.g. `redraw` was never called
    /// on it — its GPU renderer hasn't even been created).
    pub fn cached_texture(&self) -> Option<GlesTexture> {
        self.last_texture.clone()
    }

    pub fn mark_touched(&mut self) {
        self.touched = true;
    }

    /// Resize the Hut's terminal grid to fill an output of `width`x`height`
    /// physical pixels.
    pub fn resize_to_pixels(&mut self, width: i32, height: i32) {
        if (width, height) == self.pixel_size {
            return;
        }
        self.pixel_size = (width, height);

        let cell_w = self.glyphs.cell_width().max(1);
        let cell_h = self.glyphs.cell_height().max(1);
        let cols = (width.max(0) as usize / cell_w).max(1);
        let lines = (height.max(0) as usize / cell_h).max(1);
        self.terminal
            .resize(cols, lines, (cell_w as u16, cell_h as u16));
    }

    /// Convert a pixel position (relative to the Hut's own buffer, i.e.
    /// output-space when the terminal is the visible view) into a 0-based
    /// `(column, row)` grid cell, clamped to the current grid size, plus
    /// which half of that cell the position falls in (`true` = left half)
    /// — matters for selection boundary precision when starting/extending
    /// a drag from partway into a cell. Note this is 0-based (matching
    /// `alacritty_terminal`'s own grid coordinates, used for selection),
    /// *not* the 1-based coordinates SGR mouse-reporting escape sequences
    /// use — callers doing mouse reporting need to add 1.
    pub fn pixel_to_cell(&self, x: f64, y: f64) -> (usize, usize, bool) {
        let cell_w = self.glyphs.cell_width().max(1);
        let cell_h = self.glyphs.cell_height().max(1);
        let px_x = x.max(0.0) as usize;
        let px_y = y.max(0.0) as usize;
        let col = (px_x / cell_w).min(self.terminal.cols().saturating_sub(1));
        let row = (px_y / cell_h).min(self.terminal.lines().saturating_sub(1));
        let left_half = (px_x % cell_w) < cell_w / 2;
        (col, row, left_half)
    }

    /// Re-render (via the GPU glyph-atlas renderer, if anything changed
    /// since last time) and return the current texture to composite, or
    /// `None` if there's nothing to show yet (zero pixel size, or the GPU
    /// renderer failed to initialize).
    pub fn redraw(&mut self, renderer: &mut GlesRenderer) -> Option<GlesTexture> {
        let (width, height) = self.pixel_size;
        if width <= 0 || height <= 0 {
            return None;
        }

        if self.gpu.is_none() {
            match GpuTermRenderer::new(renderer) {
                Ok(gpu) => self.gpu = Some(gpu),
                Err(err) => {
                    tracing::error!("failed to initialize GPU terminal renderer: {err}");
                    return None;
                }
            }
        }
        let gpu = self.gpu.as_mut()?;

        let Some(cells) = self.terminal.take_dirty_cells() else {
            return self.last_texture.clone();
        };

        let cell_w = self.glyphs.cell_width();
        let cell_h = self.glyphs.cell_height();
        let baseline = self.glyphs.baseline();
        match gpu.redraw(
            renderer,
            &mut self.glyphs,
            &cells,
            cell_w,
            cell_h,
            baseline,
            width,
            height,
        ) {
            Ok(texture) => {
                self.last_texture = Some(texture.clone());
                Some(texture)
            }
            Err(err) => {
                tracing::error!("GPU terminal redraw failed: {err}");
                self.last_texture.clone()
            }
        }
    }
}
