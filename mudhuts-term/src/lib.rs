//! PTY + VTE grid + glyph-rasterization glue for mudhuts' built-in terminal
//! emulator (one instance of this per Hut).
//!
//! `alacritty_terminal` owns PTY spawning and ANSI/VTE parsing and runs its
//! own background thread pumping bytes between the PTY and the grid; we
//! just feed it key input, read the grid back for rendering, and get
//! woken up (via a `calloop` channel) whenever there's new content.

pub mod keys;
pub mod palette;
pub mod render;

use std::io;
use std::sync::Arc;

use alacritty_terminal::event::{
    Event as AlacrittyEvent, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;

pub use render::GlyphCache;

/// Something changed in the terminal that the compositor should react to.
#[derive(Debug, Clone)]
pub enum TermEvent {
    /// New content is available; redraw.
    Wakeup,
    /// The window title changed.
    Title(String),
    /// The shell exited.
    Exited,
}

#[derive(Clone)]
struct EventProxy(calloop::channel::Sender<TermEvent>);

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        let mapped = match event {
            AlacrittyEvent::Wakeup => Some(TermEvent::Wakeup),
            AlacrittyEvent::Title(t) => Some(TermEvent::Title(t)),
            AlacrittyEvent::Exit | AlacrittyEvent::ChildExit(_) => Some(TermEvent::Exited),
            _ => None,
        };
        if let Some(event) = mapped {
            let _ = self.0.send(event);
        }
    }
}

#[derive(Clone, Copy)]
struct GridSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// One built-in terminal: PTY, shell, and VTE grid state for a single Hut.
pub struct Terminal {
    term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: Notifier,
    size: GridSize,
    cell_size: (u16, u16),
    /// Set once the shell has exited; kept around so callers can decide
    /// whether to respawn or tear down the owning Hut.
    pub exited: bool,
}

fn window_size(size: GridSize, cell_size: (u16, u16)) -> WindowSize {
    WindowSize {
        num_lines: size.lines as u16,
        num_cols: size.cols as u16,
        cell_width: cell_size.0,
        cell_height: cell_size.1,
    }
}

impl Terminal {
    /// Spawn a new shell in a fresh PTY. `cell_size` is the glyph cell
    /// size in pixels (from [`GlyphCache`]), used only for `TIOCSWINSZ`
    /// bookkeeping. Returns the terminal plus a `calloop` channel the
    /// caller should insert into its event loop to know when to redraw.
    pub fn spawn(
        cols: usize,
        lines: usize,
        cell_size: (u16, u16),
    ) -> io::Result<(Terminal, calloop::channel::Channel<TermEvent>)> {
        tty::setup_env();

        let (tx, rx) = calloop::channel::channel();
        let event_proxy = EventProxy(tx);
        let size = GridSize { cols, lines };

        let term = Term::new(Config::default(), &size, event_proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let pty = tty::new(&tty::Options::default(), window_size(size, cell_size), 0)?;

        let pty_event_loop = PtyEventLoop::new(term.clone(), event_proxy, pty, false, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let _join = pty_event_loop.spawn();

        Ok((
            Terminal {
                term,
                notifier,
                size,
                cell_size,
                exited: false,
            },
            rx,
        ))
    }

    /// Send input bytes (already encoded, see [`keys::encode`]) to the shell.
    pub fn write_input(&self, bytes: Vec<u8>) {
        self.notifier.notify(bytes);
    }

    /// Resize the grid (in cells) and notify the PTY of the new size.
    pub fn resize(&mut self, cols: usize, lines: usize, cell_size: (u16, u16)) {
        self.size = GridSize { cols, lines };
        self.cell_size = cell_size;
        self.term.lock().resize(self.size);
        self.notifier.on_resize(window_size(self.size, cell_size));
    }

    pub fn cols(&self) -> usize {
        self.size.cols
    }

    pub fn lines(&self) -> usize {
        self.size.lines
    }

    /// Current terminal mode flags (application-cursor mode, etc.), needed
    /// by [`keys::encode`].
    pub fn mode(&self) -> alacritty_terminal::term::TermMode {
        *self.term.lock().mode()
    }

    /// Rasterize the current grid into `buf` (RGBA8, `width * height * 4`
    /// bytes) using `glyphs`, touching only what changed since the last
    /// call. Returns the redrawn pixel rectangles for damage tracking.
    pub fn render(
        &self,
        glyphs: &mut GlyphCache,
        buf: &mut [u8],
        width: usize,
        height: usize,
    ) -> Vec<render::PixelRect> {
        let mut term = self.term.lock();
        let damage = match term.damage() {
            alacritty_terminal::term::TermDamage::Full => render::Damage::Full,
            alacritty_terminal::term::TermDamage::Partial(lines) => render::Damage::Lines(
                lines
                    .map(|l| render::LineDamage {
                        line: l.line,
                        left: l.left,
                        right: l.right,
                    })
                    .collect(),
            ),
        };
        term.reset_damage();
        render::render(
            term.renderable_content(),
            glyphs,
            buf,
            width,
            height,
            &damage,
        )
    }
}
