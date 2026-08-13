//! A Console Hut: one built-in terminal plus its Main Windows — the leaf node of
//! the [`Hut`](crate::hut::Hut) tree. Renamed from the original "Hut" as part of
//! the composable Hut hierarchy redesign (see
//! `docs/rfcs/composable-hut-hierarchy.md`), once "Hut" itself became the general,
//! recursively composable tree node type — see the plan at
//! `/home/gavin/.claude/plans/cryptic-honking-lamport.md`.

use std::sync::atomic::{AtomicU64, Ordering};

use smithay::backend::renderer::Texture;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{CommitCounter, DamageBag, DamageSnapshot};
use smithay::desktop::Window;
use smithay::desktop::space::Space;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Buffer, Rectangle};

use mudhuts_term::{GlyphCache, TermEvent, Terminal};
use mudhuts_term::palette::Rgb;

use crate::gpu_term::{GpuTermRenderer, LabelRenderer};
use crate::main_window::MainWindowEntry;
use crate::render::{ChangeTracker, LabelCache};
use crate::space_element::{HutSpaceElement, synthetic_output};

/// Initial grid size used before the real output size is known.
const INITIAL_COLS: usize = 80;
const INITIAL_LINES: usize = 24;

fn next_hut_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct ConsoleHut {
    /// Stable identity for this ConsoleHut, independent of its position in [The
    /// Stack](crate::stack::MruStackHut) (which shifts as entries are added
    /// and discarded) — used to route its `TermEvent` channel to the right
    /// entry once there are several.
    pub id: u64,
    pub terminal: Terminal,
    pub glyphs: GlyphCache,
    /// Whether this ConsoleHut has ever been interacted with (a keystroke sent to
    /// its terminal) since it was spawned. A freshly-spawned, never-touched
    /// ConsoleHut is discarded rather than kept around once The Stack moves away
    /// from it — see the plan's Phase 3 notes.
    touched: bool,
    /// Lazily created on first [`ConsoleHut::redraw`] call, once a renderer is
    /// actually available (Phase 1 spawns Huts before the winit backend
    /// exists).
    gpu: Option<GpuTermRenderer>,
    /// Lazily created on first [`ConsoleHut::render_label`] call (Phase 4's
    /// tab-strip chrome) — shares `gpu`'s glyph atlas rather than
    /// rasterizing the same glyphs into a second one.
    label_renderer: Option<LabelRenderer>,
    /// What `redraw` returned last time, reused when nothing changed
    /// (cheap: an `Arc` clone, not a re-render).
    last_texture: Option<GlesTexture>,
    pixel_size: (i32, i32),
    /// Stable identity for this ConsoleHut's terminal render element across
    /// frames (matters for the compositor's outer damage tracking, which
    /// compares elements by id between frames).
    pub element_id: Id,
    /// Stable identities for this ConsoleHut's own "Terminal" tab in `chrome.rs`
    /// (its text label and background) — like `element_id`, these must
    /// stay the same across frames rather than being freshly generated
    /// each call, or the outer damage tracker sees a "new" element every
    /// frame instead of recognizing it as the same one, which a
    /// multi-buffer-swapchain-aware tracker (`DrmCompositor`, used by
    /// the real udev/DRM backend) can handle very differently — and much
    /// worse — than a simpler single-buffer one (`OutputDamageTracker`,
    /// used only by the winit backend, which is why this went unnoticed
    /// there).
    pub terminal_tab_text_id: Id,
    pub terminal_tab_bg_id: Id,
    /// Caches the "Terminal" tab's rendered text label, only actually
    /// re-rendering it (real GPU work) when its active/inactive state
    /// flips — see `render::LabelCache`'s doc comment. Also backs real
    /// damage tracking for its background, bumped on the same flip —
    /// see `render::ChangeTracker`'s doc comment.
    terminal_tab_text_cache: LabelCache<bool>,
    terminal_tab_bg_tracker: ChangeTracker<bool>,
    /// Stable identities for this ConsoleHut's own thumbnail/highlight in the
    /// Alt-Tab preview popup (`switcher.rs`) — same reasoning as above.
    pub thumbnail_id: Id,
    pub thumbnail_highlight_id: Id,
    /// Real damage tracking for this ConsoleHut's terminal texture, bumped in
    /// [`Self::redraw`] whenever the terminal grid actually had dirty
    /// cells. `TextureRenderElement::from_static_texture` (used for
    /// `element_id`/`thumbnail_id` above) is documented by Smithay as
    /// creating an element *without* damage tracking — its wrapped
    /// snapshot never advances, so the outer per-element damage tracker
    /// (`DrmCompositor`'s, in particular) sees zero damage forever after
    /// the first frame a given Id is rendered, no matter how many times
    /// the underlying texture's pixel content actually changes. That's
    /// fine for genuinely static content but wrong for a terminal, whose
    /// content changes on every keystroke — this tracker backs a real
    /// `from_texture_with_damage` snapshot instead (see `render.rs`/
    /// `switcher.rs`).
    damage_tracker: DamageBag<i32, Buffer>,

    /// Client toplevels belonging to this ConsoleHut (see the plan's Phase 4
    /// notes on PID-ancestry assignment), tab-ordered, plus whatever's
    /// been tagged as their Floating Windows/Alerts (Phase 5). At most one
    /// Main Window's tab is ever visible/mapped at a time — see
    /// `active_main_window` and `State::sync_visible_main_window` — but
    /// that one's floating Floating Windows and Alerts are all visible
    /// alongside it.
    main_windows: Vec<MainWindowEntry>,
    /// Index into `main_windows` of the tab that's active — meaningless
    /// while `main_windows` is empty.
    active_main_window: usize,
    /// Whether *this ConsoleHut's* terminal (vs. its active Main Window) is the
    /// visible view when this ConsoleHut is focused. Per-ConsoleHut so switching Huts
    /// (or Main Window tabs) doesn't disturb what each one was last
    /// showing. Ignored (treated as `true`) while `main_windows` is
    /// empty — see `State::showing_terminal_effective`.
    pub showing_terminal: bool,
    /// Fractional scroll-wheel/trackpad distance (physical pixels,
    /// signed the same way `input.rs`'s `PointerAxis` handling reads
    /// `vertical_amount`) not yet converted into a whole line of
    /// scrollback movement. See `input.rs`'s `PointerAxis` handler for
    /// why accumulating this (instead of flooring every event to at
    /// least one line) matters for continuous-scroll devices like a
    /// trackpad, which send many small events per swipe.
    pub scroll_accum: f64,

    /// This ConsoleHut's own `Space<HutSpaceElement>` (composable Hut
    /// hierarchy RFC migration step 5 sub-step 2) — replaces the old single
    /// global `state.space: Space<Window>`'s window-composition role,
    /// scoped per-instance instead. Bound to [`Self::space_output`], a
    /// synthetic output sized to [`Self::pixel_size`] (kept in sync by
    /// [`Self::resize_to_pixels`]), not the real one — see
    /// `docs/rfcs/composable-hut-hierarchy.md`'s Q1. Populated by
    /// `State::sync_visible_main_window` whenever this is the focused
    /// ConsoleHut, and read by `render.rs`'s `content_elements`,
    /// `State::surface_under`, `input.rs`'s click routing, and
    /// `docks.rs`/`grabs.rs`'s drag handling.
    pub space: Space<HutSpaceElement>,
    pub(crate) space_output: Output,
}

impl ConsoleHut {
    /// Spawn a new ConsoleHut (shell + empty framebuffer). `extra_env` is set in
    /// the shell's environment only (see [`Terminal::spawn`] — notably,
    /// this is how mudhuts points the shell at its own Wayland socket
    /// without touching the compositor's own `WAYLAND_DISPLAY`, which the
    /// backend needs untouched to find whatever it's nested inside) — on
    /// top of `extra_env`, every ConsoleHut's shell also gets `MUDHUTS_HUT_ID`
    /// set to this ConsoleHut's own id, inherited by every descendant process
    /// regardless of `fork()`/`exec()` (see `ownership.rs`'s doc comment
    /// on why this is needed alongside the PID-ancestry walk). Returns
    /// the ConsoleHut plus a channel the caller must insert into the calloop
    /// event loop to learn about terminal events (title changes, shell
    /// exit).
    pub fn spawn(
        extra_env: impl IntoIterator<Item = (String, String)>,
        scale: f64,
    ) -> Result<
        (
            ConsoleHut,
            smithay::reexports::calloop::channel::Channel<TermEvent>,
        ),
        String,
    > {
        let id = next_hut_id();
        let glyphs = GlyphCache::new(scale)?;
        let cell_size = (glyphs.cell_width() as u16, glyphs.cell_height() as u16);
        let extra_env = extra_env
            .into_iter()
            .chain(std::iter::once(("MUDHUTS_HUT_ID".to_string(), id.to_string())));
        let (terminal, events) = Terminal::spawn(INITIAL_COLS, INITIAL_LINES, cell_size, extra_env)
            .map_err(|e| e.to_string())?;

        let pixel_size = (
            (INITIAL_COLS * cell_size.0 as usize) as i32,
            (INITIAL_LINES * cell_size.1 as usize) as i32,
        );

        let space_output = synthetic_output("console-hut-space", pixel_size);
        let mut space = Space::default();
        space.map_output(&space_output, (0, 0));

        Ok((
            ConsoleHut {
                id,
                terminal,
                glyphs,
                touched: false,
                gpu: None,
                last_texture: None,
                pixel_size,
                space,
                space_output,
                element_id: Id::new(),
                terminal_tab_text_id: Id::new(),
                terminal_tab_bg_id: Id::new(),
                terminal_tab_text_cache: LabelCache::new(),
                terminal_tab_bg_tracker: ChangeTracker::new(),
                thumbnail_id: Id::new(),
                thumbnail_highlight_id: Id::new(),
                damage_tracker: DamageBag::default(),
                label_renderer: None,
                main_windows: Vec::new(),
                active_main_window: 0,
                showing_terminal: true,
                scroll_accum: 0.0,
            },
            events,
        ))
    }

    pub fn shell_pid(&self) -> u32 {
        self.terminal.shell_pid
    }

    /// The Main Window whose tab is currently active, if any.
    pub fn active_window(&self) -> Option<&Window> {
        self.main_windows.get(self.active_main_window).map(|e| &e.window)
    }

    /// The active tab's full entry (Floating Windows/Alerts included), if any.
    pub fn active_main_window_entry(&self) -> Option<&MainWindowEntry> {
        self.main_windows.get(self.active_main_window)
    }

    /// Index into `main_windows()` of the active tab — meaningless while
    /// `main_windows()` is empty.
    pub fn active_main_window_index(&self) -> usize {
        self.active_main_window
    }

    pub fn main_windows(&self) -> &[MainWindowEntry] {
        &self.main_windows
    }

    /// Directly select a Main Window tab (clamped to bounds) — for
    /// clicking a specific tab in `chrome.rs`'s strip, unlike
    /// [`Self::cycle_tab`]'s relative forward/backward step.
    pub fn set_active_main_window(&mut self, index: usize) {
        self.active_main_window = index.min(self.main_windows.len().saturating_sub(1));
    }

    pub fn main_windows_mut(&mut self) -> &mut [MainWindowEntry] {
        &mut self.main_windows
    }

    /// The "Terminal" tab's text-label texture and a damage snapshot for
    /// it — reused from the cache (no GPU work) unless `active` differs
    /// from last frame's, matching `render::LabelCache`'s whole point.
    pub fn terminal_tab_label(
        &mut self,
        renderer: &mut GlesRenderer,
        active: bool,
        fg: Rgb,
        bg: Rgb,
    ) -> Result<(GlesTexture, DamageSnapshot<i32, Buffer>), String> {
        if self.terminal_tab_text_cache.is_stale(&active) {
            let texture = self.render_label(renderer, "Terminal", fg, bg)?;
            return Ok(self.terminal_tab_text_cache.store(active, texture));
        }
        match self.terminal_tab_text_cache.cached() {
            Some(result) => Ok(result),
            None => {
                // Shouldn't happen (`is_stale` would've been true), but
                // stays panic-free rather than assumed.
                let texture = self.render_label(renderer, "Terminal", fg, bg)?;
                Ok(self.terminal_tab_text_cache.store(active, texture))
            }
        }
    }

    /// A commit counter for the "Terminal" tab's background element,
    /// bumped only if `active` differs from last frame's.
    pub fn terminal_tab_bg_commit(&mut self, active: bool) -> CommitCounter {
        self.terminal_tab_bg_tracker.commit(active)
    }

    pub fn main_window_count(&self) -> usize {
        self.main_windows.len()
    }

    /// A new client toplevel was assigned to this ConsoleHut — appended as a new
    /// tab. Only becomes the active tab if `make_active` is set — callers
    /// pass `true` when there was nothing else to keep showing (this
    /// ConsoleHut's very first Main Window), matching the auto-switch spirit
    /// from Phase 2.5, now per-ConsoleHut and per-tab rather than global.
    /// `false` for a window arriving while this ConsoleHut is already showing a
    /// *different* tab: it just joins the tab strip, exactly like the
    /// existing "background ConsoleHut" case already does — without this, a
    /// second/third window opening in a ConsoleHut that already has one visible
    /// would silently steal the view out from under whatever the user
    /// was looking at (`Self::main_windows`'s `active_main_window` index
    /// changing regardless of the caller's own `should_show_now`
    /// decision was exactly this bug — see `new_toplevel`'s notes).
    pub fn push_main_window(
        &mut self,
        window: Window,
        make_active: bool,
        foreign_handle: smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle,
    ) {
        self.main_windows
            .push(MainWindowEntry::new(window, foreign_handle));
        if make_active {
            self.active_main_window = self.main_windows.len() - 1;
        }
    }

    /// Whether `surface` is currently a bare (untagged) Main Window in
    /// this ConsoleHut — used to resolve `mudhuts_window_role_v1.set_floating`/
    /// `set_alert`'s target toplevel (which must start out as a plain
    /// Main Window; `new_toplevel` always adds new clients as one before
    /// any role-assignment request can arrive on the same connection).
    pub fn has_bare_main_window(&self, surface: &WlSurface) -> bool {
        self.main_windows.iter().any(|e| e.matches(surface))
    }

    /// Remove and return a bare Main Window by surface, adjusting the
    /// active tab index the same way [`Self::remove_window`] does.
    /// Doesn't touch `showing_terminal`/redraw bookkeeping itself — the
    /// caller (role assignment) always re-inserts it somewhere else
    /// immediately, so nothing user-visible actually changes.
    pub fn take_bare_main_window(&mut self, surface: &WlSurface) -> Option<Window> {
        let idx = self.main_windows.iter().position(|e| e.matches(surface))?;
        let entry = self.main_windows.remove(idx);
        if idx < self.active_main_window {
            self.active_main_window -= 1;
        }
        self.active_main_window = self
            .active_main_window
            .min(self.main_windows.len().saturating_sub(1));
        Some(entry.window)
    }

    /// Find a bare Main Window's entry by surface, mutably — for
    /// `set_sub`/`set_alert` to reach the *target* ("main") toplevel's
    /// `floating_windows`/`alerts` list.
    pub fn find_main_window_mut(&mut self, surface: &WlSurface) -> Option<&mut MainWindowEntry> {
        self.main_windows.iter_mut().find(|e| e.matches(surface))
    }

    /// Remove and return a Floating Window or Alert (searching every Main
    /// Window's own lists) by surface — for `mudhuts_window_role_v1.
    /// set_main`, which moves a tagged window back to being a bare Main
    /// Window.
    pub fn take_nested_window(&mut self, surface: &WlSurface) -> Option<Window> {
        for entry in &mut self.main_windows {
            if let Some(idx) = entry.floating_windows.iter().position(|s| s.matches(surface)) {
                return Some(entry.floating_windows.remove(idx).window);
            }
            if let Some(idx) = entry.alerts.iter().position(|a| a.matches(surface)) {
                return Some(entry.alerts.remove(idx).window);
            }
        }
        None
    }

    /// Find a Floating Window's own entry (for updating its `Dock` state
    /// while dragging), searching every Main Window's `floating_windows`.
    pub fn floating_window_mut(
        &mut self,
        surface: &WlSurface,
    ) -> Option<&mut crate::main_window::FloatingWindow> {
        self.main_windows
            .iter_mut()
            .find_map(|e| e.floating_windows.iter_mut().find(|s| s.matches(surface)))
    }

    /// Find an Alert's own entry (for updating its tracked position while
    /// dragging), searching every Main Window's `alerts`.
    pub fn alert_mut(&mut self, surface: &WlSurface) -> Option<&mut crate::main_window::Alert> {
        self.main_windows
            .iter_mut()
            .find_map(|e| e.alerts.iter_mut().find(|a| a.matches(surface)))
    }

    /// A client toplevel belonging to this ConsoleHut was destroyed — a bare
    /// Main Window, or a Floating Window/Alert of one. Returns whether it was
    /// actually found here (callers check every ConsoleHut). Falls back to
    /// showing the terminal if a Main Window was removed and that was
    /// the last tab.
    pub fn remove_window(&mut self, surface: &WlSurface) -> bool {
        if let Some(idx) = self.main_windows.iter().position(|e| e.matches(surface)) {
            self.main_windows.remove(idx);
            if idx < self.active_main_window {
                self.active_main_window -= 1;
            }
            self.active_main_window = self
                .active_main_window
                .min(self.main_windows.len().saturating_sub(1));
            if self.main_windows.is_empty() {
                self.showing_terminal = true;
            }
            return true;
        }
        for entry in &mut self.main_windows {
            if let Some(idx) = entry.floating_windows.iter().position(|s| s.matches(surface)) {
                entry.floating_windows.remove(idx);
                return true;
            }
            if let Some(idx) = entry.alerts.iter().position(|a| a.matches(surface)) {
                entry.alerts.remove(idx);
                return true;
            }
        }
        false
    }

    /// Meta+Right/Left within this ConsoleHut: cycle the active Main Window tab.
    /// No-op with fewer than 2 — there's no Tab/Tile-Hut to bubble up
    /// to yet (Phase 6).
    pub fn cycle_tab(&mut self, forward: bool) {
        let len = self.main_windows.len();
        if len < 2 {
            return;
        }
        self.active_main_window = if forward {
            (self.active_main_window + 1) % len
        } else {
            (self.active_main_window + len - 1) % len
        };
    }

    /// Whether this ConsoleHut has ever received a keystroke since it was
    /// spawned — see the `touched` field doc.
    pub fn touched(&self) -> bool {
        self.touched
    }

    /// Whatever [`Self::redraw`] last produced, without triggering a new
    /// render — for the Alt-Tab preview popup's thumbnails, which read
    /// every ConsoleHut's texture rather than just the focused one. `None` if
    /// this ConsoleHut has never been drawn yet (e.g. `redraw` was never called
    /// on it — its GPU renderer hasn't even been created).
    pub fn cached_texture(&self) -> Option<GlesTexture> {
        self.last_texture.clone()
    }

    pub fn mark_touched(&mut self) {
        self.touched = true;
    }

    /// Resize the ConsoleHut's terminal grid to fill an output of `width`x`height`
    /// physical pixels.
    pub fn resize_to_pixels(&mut self, width: i32, height: i32) {
        if (width, height) == self.pixel_size {
            return;
        }
        self.pixel_size = (width, height);
        // Keep `space_output`'s mode in lockstep — `Self::space`'s content
        // is always exactly this ConsoleHut's own pixel size (see
        // `Self::space`'s doc comment). Deletes the old mode first so
        // repeated resizes (a resizable nested winit window) don't leave
        // `space_output`'s mode list growing forever — harmless either way
        // (nothing ever enumerates it; this output is never globalized),
        // just tidy.
        if let Some(old_mode) = self.space_output.current_mode() {
            self.space_output.delete_mode(old_mode);
        }
        let mode = smithay::output::Mode { size: (width, height).into(), refresh: 60_000 };
        self.space_output.change_current_state(Some(mode), None, None, None);
        self.space_output.set_preferred(mode);

        let cell_w = self.glyphs.cell_width().max(1);
        let cell_h = self.glyphs.cell_height().max(1);
        let cols = (width.max(0) as usize / cell_w).max(1);
        let lines = (height.max(0) as usize / cell_h).max(1);
        self.terminal
            .resize(cols, lines, (cell_w as u16, cell_h as u16));
    }

    /// Rebuild this ConsoleHut's glyph cache for a newly-known real output scale,
    /// re-deriving its terminal grid's cols/lines from the new cell size
    /// at the same physical `pixel_size`, and dropping its GPU glyph atlas
    /// (`gpu`/`label_renderer`) so it's rebuilt from scratch at the new
    /// glyph size the next time this ConsoleHut is drawn — a `GlyphCache` can't
    /// be rescaled in place (see its own doc comment: every cached glyph
    /// bitmap was rasterized at the size it was built for).
    ///
    /// Only ever needed once per ConsoleHut, right after the real output scale
    /// becomes known for the first time — `main.rs` spawns the very first
    /// ConsoleHut before any backend/output exists yet (so it starts at scale
    /// 1.0), and this is what catches it up once `winit_backend.rs`/
    /// `udev_backend.rs` learn the real value (see `Stack::rescale_all`).
    pub fn rescale(&mut self, scale: f64) -> Result<(), String> {
        self.glyphs = GlyphCache::new(scale)?;
        self.gpu = None;
        self.label_renderer = None;

        // `space_output`'s own reported scale (not just its mode/pixel
        // size) has to track the real one too — `space_render_elements`
        // derives the scale it renders `Self::space`'s contents at from
        // whatever output it's given (`Output::current_scale`), not from
        // `space_render_elements`'s own `alpha` parameter (its only other
        // input) — a mismatch here would silently mis-scale every Main
        // Window/Floating Window/Alert once the real display isn't 1.0.
        self.space_output.change_current_state(
            None,
            None,
            Some(smithay::output::Scale::Fractional(scale)),
            None,
        );

        let (width, height) = self.pixel_size;
        let cell_w = self.glyphs.cell_width().max(1);
        let cell_h = self.glyphs.cell_height().max(1);
        let cols = (width.max(0) as usize / cell_w).max(1);
        let lines = (height.max(0) as usize / cell_h).max(1);
        self.terminal
            .resize(cols, lines, (cell_w as u16, cell_h as u16));
        Ok(())
    }

    /// Convert a pixel position (relative to the ConsoleHut's own buffer, i.e.
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
                self.damage_tracker
                    .add([Rectangle::from_size(texture.size())]);
                self.last_texture = Some(texture.clone());
                Some(texture)
            }
            Err(err) => {
                tracing::error!("GPU terminal redraw failed: {err}");
                self.last_texture.clone()
            }
        }
    }

    /// A snapshot of [`Self::damage_tracker`] to hand to
    /// `TextureRenderElement::from_texture_with_damage` — see that field's
    /// doc comment for why `from_static_texture` isn't correct for the
    /// terminal's own (or its thumbnail's) render element.
    pub fn element_damage_snapshot(&self) -> DamageSnapshot<i32, Buffer> {
        self.damage_tracker.snapshot()
    }

    /// Render `text` as a small standalone label texture (Phase 4's
    /// tab-strip chrome — window titles, not the terminal grid), sharing
    /// this ConsoleHut's own glyph atlas with its terminal renderer rather than
    /// rasterizing the same glyphs into a second, separate one. Lazily
    /// initializes both if needed.
    pub fn render_label(
        &mut self,
        renderer: &mut GlesRenderer,
        text: &str,
        fg: mudhuts_term::palette::Rgb,
        bg: mudhuts_term::palette::Rgb,
    ) -> Result<GlesTexture, String> {
        if self.gpu.is_none() {
            self.gpu = Some(GpuTermRenderer::new(renderer)?);
        }
        let Some(gpu) = self.gpu.as_ref() else {
            return Err("terminal GPU renderer unavailable".to_string());
        };
        if self.label_renderer.is_none() {
            self.label_renderer = Some(LabelRenderer::new(renderer, gpu.atlas())?);
        }
        let Some(label_renderer) = self.label_renderer.as_mut() else {
            return Err("label renderer unavailable".to_string());
        };
        let cell_w = self.glyphs.cell_width();
        let cell_h = self.glyphs.cell_height();
        let baseline = self.glyphs.baseline();
        label_renderer.render(
            renderer,
            &mut self.glyphs,
            text,
            cell_w,
            cell_h,
            baseline,
            fg,
            bg,
        )
    }
}
