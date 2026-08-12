//! A Hut: one built-in terminal plus (eventually) its Main Windows. Phase 1
//! only has a single Hut and doesn't yet organize client windows into it —
//! see the plan at `/home/gavin/.claude/plans/cryptic-honking-lamport.md`.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::Transform;

use mudhuts_term::{GlyphCache, TermEvent, Terminal};

/// Initial grid size used before the real output size is known.
const INITIAL_COLS: usize = 80;
const INITIAL_LINES: usize = 24;

pub struct Hut {
    pub terminal: Terminal,
    pub glyphs: GlyphCache,
    pub buffer: MemoryRenderBuffer,
    pixel_size: (i32, i32),
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
        let buffer =
            MemoryRenderBuffer::new(Fourcc::Abgr8888, pixel_size, 1, Transform::Normal, None);

        Ok((
            Hut {
                terminal,
                glyphs,
                buffer,
                pixel_size,
            },
            events,
        ))
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
        self.buffer.render().resize((width.max(0), height.max(0)));
    }

    /// Re-rasterize the terminal grid into the backing buffer.
    pub fn redraw(&mut self) {
        let (width, height) = self.pixel_size;
        if width <= 0 || height <= 0 {
            return;
        }
        let (width, height) = (width as usize, height as usize);
        let terminal = &self.terminal;
        let glyphs = &mut self.glyphs;
        let _ = self.buffer.render().draw(|buf| {
            let rects = terminal.render(glyphs, buf, width, height);
            let damage = rects
                .into_iter()
                .map(|r| {
                    smithay::utils::Rectangle::new(
                        (r.x as i32, r.y as i32).into(),
                        (r.width as i32, r.height as i32).into(),
                    )
                })
                .collect::<Vec<_>>();
            Ok::<_, ()>(damage)
        });
    }
}
