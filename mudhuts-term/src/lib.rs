//! PTY + VTE grid + glyph-rasterization glue for mudhuts' built-in terminal
//! emulator (one instance of this per ConsoleHut).
//!
//! `alacritty_terminal` owns PTY spawning and ANSI/VTE parsing and runs its
//! own background thread pumping bytes between the PTY and the grid; we
//! just feed it key input, read the grid back for rendering, and get
//! woken up (via a `calloop` channel) whenever there's new content.

pub mod keys;
pub mod mouse;
pub mod palette;
pub mod render;

use std::io;
use std::sync::Arc;

use alacritty_terminal::event::{
    Event as AlacrittyEvent, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;

use keys::Mods;

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

/// One built-in terminal: PTY, shell, and VTE grid state for a single ConsoleHut.
pub struct Terminal {
    term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: Notifier,
    size: GridSize,
    cell_size: (u16, u16),
    /// The shell's own PID (not this process's) — used to recognize a new
    /// Wayland client as belonging to this ConsoleHut by walking its process
    /// ancestry back to this PID (see the plan's Phase 4 notes; no
    /// protocol needed for the default case).
    pub shell_pid: u32,
    /// Set once the shell has exited; kept around so callers can decide
    /// whether to respawn or tear down the owning ConsoleHut.
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
    /// bookkeeping. `extra_env` is set in the shell's (and thus its
    /// children's) environment only — deliberately *not* via
    /// `std::env::set_var` on the compositor's own process, which would
    /// also affect e.g. the backend's own connection to whatever it's
    /// nested inside. Returns the terminal plus a `calloop` channel the
    /// caller should insert into its event loop to know when to redraw.
    pub fn spawn(
        cols: usize,
        lines: usize,
        cell_size: (u16, u16),
        extra_env: impl IntoIterator<Item = (String, String)>,
    ) -> io::Result<(Terminal, calloop::channel::Channel<TermEvent>)> {
        Self::spawn_with_command(cols, lines, cell_size, extra_env, None)
    }

    /// [`Self::spawn`], running `command` (program + argv) as the PTY's own
    /// child in place of the default shell — `None` for the normal shell.
    /// See [`crate::autostart`](../mudhuts/src/autostart.rs) (mudhuts'
    /// own caller): each XDG autostart entry gets its own dedicated
    /// `Terminal` running its `Exec=` line directly this way, rather than
    /// a shell that then launches it as an untracked child.
    pub fn spawn_with_command(
        cols: usize,
        lines: usize,
        cell_size: (u16, u16),
        extra_env: impl IntoIterator<Item = (String, String)>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<(Terminal, calloop::channel::Channel<TermEvent>)> {
        tty::setup_env();

        let (tx, rx) = calloop::channel::channel();
        let event_proxy = EventProxy(tx);
        let size = GridSize { cols, lines };

        let term = Term::new(Config::default(), &size, event_proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let pty_options = tty::Options {
            shell: command.map(|(program, args)| tty::Shell::new(program, args)),
            env: extra_env.into_iter().collect(),
            ..Default::default()
        };
        let pty = tty::new(&pty_options, window_size(size, cell_size), 0)?;
        // Captured before `pty` moves into `PtyEventLoop::new` below — the
        // event loop takes ownership of the `Pty` (and so the `Child`)
        // from here on.
        let shell_pid = pty.child().id();

        let pty_event_loop = PtyEventLoop::new(term.clone(), event_proxy, pty, false, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let _join = pty_event_loop.spawn();

        Ok((
            Terminal {
                term,
                notifier,
                size,
                cell_size,
                shell_pid,
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

    /// Whether the running program has requested any form of SGR mouse
    /// reporting (click, drag, or full motion).
    pub fn wants_mouse_reports(&self) -> bool {
        mouse::wants_reports(self.mode())
    }

    /// Whether motion should be reported while a button is held.
    pub fn wants_drag_reports(&self) -> bool {
        mouse::wants_drag_reports(self.mode())
    }

    /// Report a mouse button press/release at the given 1-based cell
    /// coordinates, if the running program requested mouse reporting.
    pub fn report_mouse_button(
        &self,
        button: u32,
        mods: Mods,
        pressed: bool,
        col: usize,
        row: usize,
    ) {
        self.write_input(mouse::encode_button(button, mods, pressed, col, row));
    }

    /// Report motion while `button` is held.
    pub fn report_mouse_drag(&self, button: u32, mods: Mods, col: usize, row: usize) {
        self.write_input(mouse::encode_drag(button, mods, col, row));
    }

    /// Start a new text selection anchored at the given 1-based cell
    /// coordinates (see [`mouse::start_selection`] for `left_half`).
    pub fn start_selection(&self, col: usize, row: usize, left_half: bool) {
        self.term.lock().selection = Some(mouse::start_selection(col, row, left_half));
    }

    /// Extend the in-progress selection, if any, to the given coordinates.
    pub fn extend_selection(&self, col: usize, row: usize, left_half: bool) {
        let mut term = self.term.lock();
        if let Some(selection) = term.selection.as_mut() {
            mouse::extend_selection(selection, col, row, left_half);
        }
    }

    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
    }

    /// Scroll the scrollback view by `lines` (positive moves further up
    /// into history, negative moves back down toward the live bottom) —
    /// for mouse-wheel scrolling when the running program hasn't grabbed
    /// the mouse itself (see [`Self::wants_mouse_reports`]).
    pub fn scroll(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    pub fn has_selection(&self) -> bool {
        self.term.lock().selection.is_some()
    }

    /// The currently selected text, if any. Backs both the visual
    /// highlight and mudhuts' Wayland clipboard/primary-selection sources
    /// (see `input.rs`'s `PointerButton` handler and `Action::CopySelection`,
    /// which call this to hand the completed selection to
    /// `set_primary_selection`/`set_data_device_selection`).
    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// If anything changed since the last call (content damage or an
    /// active selection): every changed cell's resolved character/colors,
    /// plus the real [`render::Damage`] region they came from — the GPU
    /// atlas rendering path uses this to scissor its own redraw (and the
    /// outer compositor-facing damage it reports) to that same real
    /// region instead of the whole terminal, see
    /// [`crate::gpu_term::GpuTermRenderer::redraw`]'s doc comment.
    /// Returns `None` when nothing changed, so the caller can skip
    /// re-rendering entirely.
    pub fn take_dirty_cells(&self) -> Option<(Vec<render::CellInfo>, render::Damage)> {
        let mut term = self.term.lock();
        let cursor_point = term.grid().cursor.point;
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

        // `Term::damage()` unconditionally marks the cursor's current cell
        // damaged on *every* call (`damage_cursor()` in alacritty_terminal,
        // called regardless of whether the cursor moved), as a single
        // `LineDamageBounds` covering just that one column. Since we call
        // `damage()` once per attempted redraw rather than once per actual
        // terminal event, that single-cell entry is present even on a
        // fully idle terminal — without filtering it out, every frame
        // looks "damaged" and we'd redraw the whole grid continuously at
        // whatever rate the compositor tries to redraw, regardless of
        // whether anything is actually happening. Only affects this
        // "did anything real change" check, not `damage` itself — the
        // cursor's own cell still belongs in the real redraw region below
        // (its row needs repainting wherever the cursor actually is).
        let has_content_damage = match &damage {
            render::Damage::Full => true,
            render::Damage::Lines(lines) => lines.iter().any(|l| {
                !(l.line == cursor_point.line.0 as usize
                    && l.left == cursor_point.column.0
                    && l.right == cursor_point.column.0)
            }),
        };
        let has_selection = term.selection.is_some();
        if !(has_content_damage || has_selection) {
            return None;
        }
        // Selection changes (e.g. dragging to extend it) aren't reflected
        // in `Term`'s own damage tracking at all — it only tracks cell
        // *content* changes — so treat the whole grid as damaged whenever
        // one's active, same as `Terminal::render`'s own identical
        // handling, rather than trying to cheaply bound a selection's own
        // (frequently changing, easily grid-spanning) extent.
        let damage = if has_selection { render::Damage::Full } else { damage };

        let capacity = term.columns() * term.screen_lines();
        let cells = render::collect_cells(term.renderable_content(), capacity, &damage);
        Some((cells, damage))
    }

    /// Rasterize the current grid into `buf` (RGBA8, `width * height * 4`
    /// bytes) using `glyphs`, touching only what changed since the last
    /// call. Returns the redrawn pixel rectangles for damage tracking.
    ///
    /// This is the CPU rendering path (kept as a fallback/reference — the
    /// GPU atlas path in `mudhuts::gpu_term` is what's actually used now,
    /// see the Phase 2.6 plan notes on why the CPU path doesn't scale to a
    /// full 4K120Hz screen for a program that redraws large regions
    /// frequently, e.g. `btop`).
    pub fn render(
        &self,
        glyphs: &mut GlyphCache,
        buf: &mut [u8],
        width: usize,
        height: usize,
    ) -> Vec<render::PixelRect> {
        let mut term = self.term.lock();
        // Selection changes (e.g. dragging to extend it) aren't reflected
        // in `Term`'s own damage tracking at all — that only tracks cell
        // *content* changes — so without this, drag-selecting wouldn't
        // visibly update most frames.
        let has_selection = term.selection.is_some();
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
        let damage = if has_selection {
            render::Damage::Full
        } else {
            damage
        };
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
