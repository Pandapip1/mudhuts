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
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle};

use mudhuts_term::{GlyphCache, TermEvent, Terminal};
use mudhuts_term::palette::Rgb;

use crate::gpu_term::{GpuTermRenderer, LabelRenderer};
use crate::main_window::MainWindowEntry;
use crate::redraw::{Redrawable, RedrawHandle, Signal};
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
    /// while `main_windows` is empty. A [`Signal`] (composable Hut
    /// hierarchy RFC migration step 4's generic follow-up — see
    /// `redraw::Signal`'s doc comment) so nothing that changes it can
    /// forget to request a redraw, same reasoning as `TabbedHut::active`/
    /// `TileHut::active`.
    active_main_window: Signal<usize>,
    /// Whether *this ConsoleHut's* terminal (vs. its active Main Window) is the
    /// visible view when this ConsoleHut is focused. Per-ConsoleHut so switching Huts
    /// (or Main Window tabs) doesn't disturb what each one was last
    /// showing. Ignored (treated as `true`) while `main_windows` is
    /// empty — see `State::focused_showing_terminal_effective`. A [`Signal`] —
    /// this used to need a manual `State::request_redraw()` right after
    /// every write (and once shipped without one, `Action::ToggleTerminal`
    /// — see `redraw::Signal`'s doc comment), which is no longer possible
    /// to forget.
    pub showing_terminal: Signal<bool>,
    /// Fractional scroll-wheel/trackpad distance (physical pixels,
    /// signed the same way `input.rs`'s `PointerAxis` handling reads
    /// `vertical_amount`) not yet converted into a whole discrete wheel
    /// "click" — thresholded against `input.rs::WHEEL_CLICK_PX`, the
    /// ~15px-per-click convention a grabbing TUI app (vim/less/btop/...)
    /// expects its SGR mouse-wheel reports to use. See `input.rs`'s
    /// `PointerAxis` handler for why accumulating this (instead of
    /// flooring every event to at least one click) matters for
    /// continuous-scroll devices like a trackpad, which send many small
    /// events per swipe.
    pub wheel_click_accum: f64,
    /// Same idea as [`Self::wheel_click_accum`], but thresholded against
    /// the terminal's own real cell height instead of a wheel-click
    /// convention — used only when nothing's grabbed the mouse, to scroll
    /// this ConsoleHut's *own* scrollback exactly one line per full
    /// cell-height's worth of accumulated motion, rather than
    /// `wheel_click_accum`'s click-then-multiply approach (which made a
    /// gentle trackpad swipe jump several lines at once, tied to an
    /// unrelated 15px unit instead of how tall a line actually is).
    pub scroll_line_accum: f64,

    /// This ConsoleHut's own `Space<HutSpaceElement>` (composable Hut
    /// hierarchy RFC migration step 5 sub-step 2) — replaces the old single
    /// global `state.space: Space<Window>`'s window-composition role,
    /// scoped per-instance instead. Bound to [`Self::space_output`], a
    /// synthetic output sized to [`Self::pixel_size`] (kept in sync by
    /// [`Self::resize_to_pixels`]), not the real one — see
    /// `docs/rfcs/composable-hut-hierarchy.md`'s Q1.
    ///
    /// Private on purpose: across several rounds of code review, real
    /// bugs kept turning up where a Hut's window model was mutated (a
    /// window opened/closed/retagged) on some Hut *other* than the
    /// focused one — a backgrounded output's own Hut, say — and the
    /// reactive `sync_main_window_space` call that should have followed
    /// it was missed, leaving `space` stale until something unrelated
    /// happened to trigger a resync. A raw `pub` field made every one of
    /// those a silent, easy-to-miss mistake at the *mutation* site — one
    /// among many call sites across several files, all easy to add a new
    /// one to and just as easy to forget. Flipped instead: nothing
    /// outside this module can reach `space` directly at all, so there's
    /// no mutation-site discipline to remember in the first place — see
    /// [`Self::space_mut`]/[`Self::space`]/[`Self::space_raw_mut`].
    space: Space<HutSpaceElement>,
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
        Self::spawn_with_command(extra_env, scale, None)
    }

    /// [`Self::spawn`], running `command` (program + argv) as the PTY's
    /// own child in place of the default shell — see
    /// [`mudhuts_term::Terminal::spawn_with_command`]'s doc comment.
    /// `autostart.rs`'s own caller: each XDG autostart entry gets its own
    /// dedicated ConsoleHut this way, rather than every entry sharing one
    /// Hut whose own shell never actually runs the app at all.
    pub fn spawn_with_command(
        extra_env: impl IntoIterator<Item = (String, String)>,
        scale: f64,
        command: Option<(String, Vec<String>)>,
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
        let (terminal, events) =
            Terminal::spawn_with_command(INITIAL_COLS, INITIAL_LINES, cell_size, extra_env, command)
                .map_err(|e| e.to_string())?;

        let pixel_size = (
            (INITIAL_COLS * cell_size.0 as usize) as i32,
            (INITIAL_LINES * cell_size.1 as usize) as i32,
        );

        let space_output = synthetic_output("console-hut-space", pixel_size, 1.0);
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
                active_main_window: Signal::new(0),
                showing_terminal: Signal::new(true),
                wheel_click_accum: 0.0,
                scroll_line_accum: 0.0,
            },
            events,
        ))
    }

    pub fn shell_pid(&self) -> u32 {
        self.terminal.shell_pid
    }

    /// The Main Window whose tab is currently active, if any.
    pub fn active_window(&self) -> Option<&Window> {
        self.main_windows.get(*self.active_main_window).map(|e| &e.window)
    }

    /// The active tab's full entry (Floating Windows/Alerts included), if any.
    pub fn active_main_window_entry(&self) -> Option<&MainWindowEntry> {
        self.main_windows.get(*self.active_main_window)
    }

    /// Index into `main_windows()` of the active tab — meaningless while
    /// `main_windows()` is empty.
    pub fn active_main_window_index(&self) -> usize {
        *self.active_main_window
    }

    pub fn main_windows(&self) -> &[MainWindowEntry] {
        &self.main_windows
    }

    /// Directly select a Main Window tab (clamped to bounds) — for
    /// clicking a specific tab in `chrome.rs`'s strip, unlike
    /// [`Self::cycle_tab`]'s relative forward/backward step.
    pub fn set_active_main_window(&mut self, index: usize) {
        *self.active_main_window = index.min(self.main_windows.len().saturating_sub(1));
    }

    pub fn main_windows_mut(&mut self) -> &mut [MainWindowEntry] {
        &mut self.main_windows
    }

    /// Make this ConsoleHut's own `space` match what it should currently
    /// be showing: unmap whatever's mapped (harmless if nothing was),
    /// then map the active Main Window (if this ConsoleHut isn't showing
    /// its terminal) plus every currently-floating Floating Window and
    /// every Alert belonging to it — docked Floating Windows stay
    /// unmapped (`docks.rs` draws a handle instead), Alerts are mapped
    /// last so they end up on top. `area_origin` is the real usable
    /// area's own origin — matters once a layer-shell surface reserves
    /// part of the output (e.g. a left-anchored panel) — **genuinely
    /// Logical, not physical**: this is a real `Space<HutSpaceElement>`,
    /// and `Space::map_element` (which this calls, below) requires a
    /// Logical point (Smithay's pinned source:
    /// `P: Into<Point<i32, Logical>>`), so the parameter itself is typed
    /// `Point<i32, Logical>`, not a bare `(i32, i32)` — every caller must
    /// pass `State::focused_usable_area_logical`/`usable_area_logical_for`'s
    /// own `.loc`, *not* `focused_usable_area`/`usable_area_for`'s
    /// (physical-pixel, and since a fix caught in review, genuinely
    /// `Rectangle<i32, Physical>`-typed) one. A bare `(i32, i32)` tuple
    /// used to type-check either way with no compiler warning — a real,
    /// previously-shipped bug passed the physical value here, invisible
    /// at scale 1.0 but silently shifting every Main Window down/right by
    /// roughly one extra copy of whatever's reserving space at the
    /// output's origin at any other scale (this value gets converted back
    /// to physical a second time downstream, in `render.rs`'s
    /// `content_pieces_to_elements`, which — correctly — treats a mapped
    /// Window's position as genuinely Logical and multiplies by scale
    /// once). Passing `focused_usable_area()`'s `.loc` here now simply
    /// doesn't compile: `Rectangle<i32, Physical>::loc` is a
    /// `Point<i32, Physical>`, not `Point<i32, Logical>`.
    ///
    /// Extracted from `State::sync_visible_main_window`'s old body
    /// (composable Hut hierarchy RFC migration step 5 sub-step 2) so it
    /// can run for *any* ConsoleHut, not just the focused one —
    /// `render.rs`'s `refresh_hut_content_thumbnail` is the other caller,
    /// syncing a backgrounded entry's `space` only while its Alt-Tab
    /// thumbnail is actually about to be shown.
    pub fn sync_main_window_space(&mut self, area_origin: Point<i32, Logical>) {
        let mapped: Vec<_> = self.space.elements().cloned().collect();
        for window in mapped {
            self.space.unmap_elem(&window);
        }
        if *self.showing_terminal {
            return;
        }
        let Some(entry) = self.active_main_window_entry() else {
            return;
        };
        // Cloned out into owned locals before touching `self.space` again
        // — `entry` borrows `self` immutably, which can't coexist with
        // the `self.space.map_element` calls below.
        let main_window = entry.window.clone();
        let floating: Vec<_> = entry
            .floating_windows
            .iter()
            .filter_map(|sub| match sub.dock {
                crate::main_window::Dock::Floating(pos) => Some((sub.window.clone(), pos)),
                crate::main_window::Dock::Docked(_) => None,
            })
            .collect();
        let alerts: Vec<_> = entry.alerts.iter().map(|a| (a.window.clone(), a.position)).collect();

        self.space
            .map_element(HutSpaceElement::Window(main_window), area_origin, false);
        for (window, pos) in floating {
            self.space.map_element(HutSpaceElement::Window(window), pos, false);
        }
        for (window, pos) in alerts {
            self.space.map_element(HutSpaceElement::Window(window), pos, false);
        }
    }

    /// This Hut's own `space`, freshly rebuilt from the current logical
    /// window model before being returned — [`Self::sync_main_window_space`]
    /// is cheap and idempotent (pure `Vec`/`Space` bookkeeping, no
    /// renderer/GPU/IO involved), so calling it unconditionally here is
    /// deliberate, not a shortcut: it's what makes `space` impossible to
    /// read stale after a *specific, known* model mutation — the reason
    /// this exists at all is real bugs, across several rounds of review,
    /// from a mutation call site forgetting to sync reactively for the
    /// right Hut afterward.
    ///
    /// NOT a blanket "always use this to read/write `space`" accessor,
    /// though — an earlier version of this doc comment claimed exactly
    /// that, and wiring it into every read site (`State::surface_under`,
    /// `input.rs`'s click routing, `udev_backend.rs`/`winit_backend.rs`'s
    /// per-frame callback sweep) broke dragging and `raise_element`'s
    /// z-order entirely: those paths run interleaved with (or more
    /// often than) `grabs.rs`/`docks.rs`'s live, not-yet-in-the-model
    /// drag writes via [`Self::space_raw_mut`], and a forced sync
    /// discards whatever a live write hasn't persisted back to the model
    /// yet. Reach for this specifically after a mutation you just made
    /// (`render.rs`'s `refresh_hut_content_thumbnail`, `State::sync_visible_main_window`/
    /// `sync_hut_space`), not as the default way to *read* `space` for
    /// rendering/hit-testing/frame callbacks — see [`Self::space`]'s own
    /// doc comment for that.
    pub fn space_mut(&mut self, area_origin: Point<i32, Logical>) -> &mut Space<HutSpaceElement> {
        self.sync_main_window_space(area_origin);
        &mut self.space
    }

    /// This Hut's own `space`, read-only and *without* syncing first —
    /// whatever was last written there, stale or not. This is the normal
    /// way to *read* `space` for rendering/hit-testing/frame callbacks
    /// (`State::surface_under`, `input.rs`'s click routing,
    /// `udev_backend.rs`/`winit_backend.rs`'s frame-callback sweep,
    /// `handlers/xdg_shell.rs`'s `move_request`, `grabs.rs`/`docks.rs`'s
    /// own live in-progress drag position reads) — see
    /// [`Self::space_mut`]'s own doc comment for why forcing a sync at
    /// these particular call sites is actively wrong, not just
    /// unnecessary: reading whatever's *actually* currently there
    /// (live-drag positions and `raise_element`'s z-order included) is
    /// the behavior these callers want anyway, not a model-derived
    /// rebuild. Genuine staleness (a mutation site that forgot to call
    /// [`Self::space_mut`]/[`Self::sync_main_window_space`] for the Hut
    /// it actually changed) is a real risk this doesn't protect
    /// against — but that's a smaller, better-understood surface (a
    /// handful of mutation call sites, not every read site) than this
    /// method's own history already proved the alternative to be.
    pub fn space(&self) -> &Space<HutSpaceElement> {
        &self.space
    }

    /// `root`'s real absolute **physical**-pixel rect — matching
    /// `GraphStack::leaf_absolute_rect`'s own documented contract, the
    /// same one its Main-Window case already satisfies via
    /// `usable_area_for` — if it's a currently-mapped Floating Window or
    /// Alert. `GraphStack::leaf_absolute_rect`'s own fallback for a popup
    /// root that isn't a bare Main Window (those are always fullscreen,
    /// hence `area` alone suffices for them; see that function's own doc
    /// comment). A Floating Window/Alert is neither fullscreen nor
    /// Hut-usable-area-origin-relative — it floats at its own tracked
    /// position anywhere on screen — so falling back to the coarse
    /// whole-output/whole-usable-area rect the way `leaf_absolute_rect`
    /// used to (there was no other option before this existed) positions
    /// any popup it opens relative to the wrong origin entirely, off by
    /// however far the Floating Window/Alert itself is from `(0, 0)`.
    ///
    /// Reads straight from `self.space` (kept in sync with the *active*
    /// Main Window entry's own Floating Windows/Alerts by
    /// `sync_main_window_space`, which maps each one in at its own tracked
    /// position — see that function's own doc comment) rather than
    /// reconstructing the rect by hand from `MainWindowEntry::floating_windows`/
    /// `alerts`: `Space`'s own `SpaceElement::geometry` is the single
    /// authoritative source for "where a mapped element actually is,"
    /// already combining position and the window's own geometry
    /// size/offset exactly the way rendering and hit-testing already read
    /// it — hand-deriving the same answer a second way here risks it
    /// silently drifting from that if either ever changes independently.
    /// That geometry is genuinely [`Logical`](smithay::utils::Logical)
    /// though (`self.space`'s own convention — see `sync_main_window_space`'s
    /// doc comment), so it's converted to physical here via
    /// `self.space_output`'s own tracked scale (kept in lockstep with the
    /// real output's — see [`Self::rescale`]'s doc comment) before
    /// returning: a caught-in-review regression where this used to return
    /// Logical values that `handlers/xdg_shell.rs`'s `unconstrain_popup`
    /// then treated as physical and divided by scale a *second* time,
    /// silently shrinking a Floating Window/Alert's own popups at any
    /// scale other than 1.0 — the exact bug class this whole file's
    /// module doc/`sync_main_window_space`'s own doc comment already warn
    /// about.
    ///
    /// `None` if `root` isn't currently mapped at all (not a Floating
    /// Window/Alert, or one that belongs to a currently-backgrounded Main
    /// Window entry and so isn't in `space` right now) — the caller's own
    /// coarser fallback still applies in that case.
    pub fn floating_or_alert_absolute_rect(&self, root: &WlSurface) -> Option<(i32, i32, i32, i32)> {
        self.space.elements().find_map(|element| {
            let HutSpaceElement::Window(window) = element else {
                return None;
            };
            if !crate::main_window::window_matches(window, root) {
                return None;
            }
            // `self.space.element_geometry(element)`, not
            // `SpaceElement::geometry(element)` called on the element
            // directly — a real bug caught in review: `Window`'s own
            // `SpaceElement::geometry` impl is just the window's own
            // local xdg-surface geometry (origin near `(0, 0)`,
            // independent of any `Space` it happens to be mapped into),
            // while `Space::element_geometry` is the one that actually
            // substitutes in this element's real mapped location (the
            // `pos`/`area_origin` `sync_main_window_space` mapped it at).
            // Reading the former left `.loc` silently wrong for any
            // Floating Window/Alert not sitting exactly at its owning
            // Hut's own origin — invisible so far only because this
            // function's one live caller happened to discard `.loc` and
            // keep just `.size`.
            let logical = self.space.element_geometry(element)?;
            let scale = self.space_output.current_scale().fractional_scale();
            let physical: Rectangle<i32, Physical> = logical.to_physical_precise_round(scale);
            Some((physical.loc.x, physical.loc.y, physical.size.w, physical.size.h))
        })
    }

    /// This Hut's own `space`, for direct mutation *without* syncing
    /// first — exists only for `grabs.rs`'s `MoveSurfaceGrab::motion`/
    /// `docks.rs`'s `advance_drag` (a live, in-progress drag position,
    /// written on every pointer-motion sample, deliberately not yet
    /// reflected in the logical window model until the drag actually
    /// ends) and `input.rs`'s click-to-focus `raise_element` call (a
    /// z-order change that isn't part of the logical model at all).
    /// Calling [`Self::space_mut`] at either call site instead would
    /// rebuild from the model and discard exactly the thing being
    /// written — see its own doc comment.
    pub fn space_raw_mut(&mut self) -> &mut Space<HutSpaceElement> {
        &mut self.space
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
            *self.active_main_window = self.main_windows.len() - 1;
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
        *self.active_main_window =
            crate::hut::shift_active_index_on_removal(*self.active_main_window, idx, self.main_windows.len());
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
            *self.active_main_window =
                crate::hut::shift_active_index_on_removal(*self.active_main_window, idx, self.main_windows.len());
            if self.main_windows.is_empty() {
                *self.showing_terminal = true;
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
        *self.active_main_window = if forward {
            (*self.active_main_window + 1) % len
        } else {
            (*self.active_main_window + len - 1) % len
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

        let Some((cells, damage)) = self.terminal.take_dirty_cells() else {
            return self.last_texture.clone();
        };

        let cell_w = self.glyphs.cell_width();
        let cell_h = self.glyphs.cell_height();
        let baseline = self.glyphs.baseline();
        match gpu.redraw(
            renderer,
            &mut self.glyphs,
            &cells,
            &damage,
            cell_w,
            cell_h,
            baseline,
            width,
            height,
        ) {
            Ok((texture, touched)) => {
                // `touched` — the real region `gpu.redraw` actually
                // repainted (see its own doc comment) — `None` for a
                // full redraw (first frame, a resize, or `Damage::Full`
                // upstream), matching `Rectangle::from_size(texture.size())`'s
                // old always-whole-texture behavior exactly for that
                // case; `Some(rect)` reports the real, usually much
                // smaller, damaged region instead, so the outer
                // compositor-facing damage tracker (and everything
                // downstream of `element_damage_snapshot`) sees real
                // per-redraw damage rather than "the whole terminal
                // changed" on every single keystroke.
                self.damage_tracker
                    .add([touched.unwrap_or_else(|| Rectangle::from_size(texture.size()))]);
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

impl Redrawable for ConsoleHut {
    /// The leaf case `Hut::attach_redraw_handle` recurses into — see that
    /// method's doc comment. Reaches every one of this ConsoleHut's own
    /// `Signal` fields; a future one just needs adding here too.
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.showing_terminal.attach_redraw_handle(handle.clone());
        self.active_main_window.attach_redraw_handle(handle);
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::Point;

    use super::*;
    use crate::main_window::{Alert, Dock, Edge, FloatingWindow};
    use crate::test_support::spawn_test_windows;

    fn new_hut() -> ConsoleHut {
        ConsoleHut::spawn(std::iter::empty(), 1.0).unwrap().0
    }

    fn surface_of(window: &Window) -> WlSurface {
        window.toplevel().unwrap().wl_surface().clone()
    }

    #[test]
    fn a_second_main_window_arriving_in_the_background_does_not_steal_the_active_tab() {
        let mut hut = new_hut();
        let [(a, a_fth), (b, b_fth)] = spawn_test_windows(2).try_into().ok().unwrap();
        hut.push_main_window(a, true, a_fth);
        hut.push_main_window(b, false, b_fth);
        assert_eq!(hut.active_main_window_index(), 0, "the 2nd window joined the tab strip without activating");
        assert_eq!(hut.main_window_count(), 2);
    }

    #[test]
    fn take_bare_main_window_shifts_active_index_to_keep_pointing_at_the_same_survivor() {
        let mut hut = new_hut();
        let [(a, a_fth), (b, b_fth), (c, c_fth)] = spawn_test_windows(3).try_into().ok().unwrap();
        let c_surface = surface_of(&c);
        hut.push_main_window(a.clone(), true, a_fth);
        hut.push_main_window(b, false, b_fth);
        hut.push_main_window(c, false, c_fth);
        hut.set_active_main_window(2); // active tab is now C

        let taken = hut.take_bare_main_window(&surface_of(&a));
        assert!(taken.is_some(), "A should have been found and removed");
        assert_eq!(hut.main_window_count(), 2);
        assert_eq!(hut.active_main_window_index(), 1, "C shifted left into A's old slot");
        assert_eq!(surface_of(hut.active_window().unwrap()), c_surface, "active tab is still C, not B");
    }

    #[test]
    fn remove_window_falls_back_to_the_terminal_once_the_last_main_window_is_gone() {
        let mut hut = new_hut();
        let (a, a_fth) = spawn_test_windows(1).into_iter().next().unwrap();
        let a_surface = surface_of(&a);
        hut.push_main_window(a, true, a_fth);
        *hut.showing_terminal = false;

        assert!(hut.remove_window(&a_surface));
        assert_eq!(hut.main_window_count(), 0);
        assert!(*hut.showing_terminal, "no Main Windows left, so the terminal should show again");
    }

    #[test]
    fn remove_window_also_finds_a_floating_window_or_alert_nested_under_a_main_window() {
        let mut hut = new_hut();
        let [(a, a_fth), (b, _)] = spawn_test_windows(2).try_into().ok().unwrap();
        let b_surface = surface_of(&b);
        hut.push_main_window(a.clone(), true, a_fth);
        hut.find_main_window_mut(&surface_of(&a)).unwrap().floating_windows.push(FloatingWindow::new(b));

        assert!(hut.remove_window(&b_surface), "should be found nested under A, not just at the top level");
        assert_eq!(hut.main_window_count(), 1, "removing a nested Floating Window must not remove its owning Main Window");
        assert!(hut.find_main_window_mut(&surface_of(&a)).unwrap().floating_windows.is_empty());
    }

    #[test]
    fn cycle_tab_wraps_in_both_directions() {
        let mut hut = new_hut();
        let [(a, a_fth), (b, b_fth), (c, c_fth)] = spawn_test_windows(3).try_into().ok().unwrap();
        hut.push_main_window(a, true, a_fth);
        hut.push_main_window(b, false, b_fth);
        hut.push_main_window(c, false, c_fth);

        assert_eq!(hut.active_main_window_index(), 0);
        hut.cycle_tab(true);
        assert_eq!(hut.active_main_window_index(), 1);
        hut.cycle_tab(true);
        assert_eq!(hut.active_main_window_index(), 2);
        hut.cycle_tab(true);
        assert_eq!(hut.active_main_window_index(), 0, "forward from the last tab wraps to the first");
        hut.cycle_tab(false);
        assert_eq!(hut.active_main_window_index(), 2, "backward from the first tab wraps to the last");
    }

    /// Regression test for a bug caught in review (see `floating_or_alert_absolute_rect`'s
    /// own doc comment): it used to read a mapped element's *local* xdg-surface
    /// geometry (`SpaceElement::geometry`) instead of its real mapped position
    /// (`Space::element_geometry`), which happened to go unnoticed only because
    /// its one live caller at the time discarded `.loc` and kept just `.size`.
    /// Only the Main Window itself maps at `area_origin`; a Floating Window's
    /// and an Alert's own tracked positions are already absolute — this pins
    /// down all three concretely rather than relying on `.size` alone ever
    /// again masking a `.loc` regression.
    #[test]
    fn sync_main_window_space_maps_everything_at_its_real_documented_position() {
        let mut hut = new_hut();
        let [(main, main_fth), (floating, _), (alert, _)] = spawn_test_windows(3).try_into().ok().unwrap();
        let main_surface = surface_of(&main);
        let floating_surface = surface_of(&floating);
        let alert_surface = surface_of(&alert);

        hut.push_main_window(main, true, main_fth);
        *hut.showing_terminal = false;
        let entry = hut.find_main_window_mut(&main_surface).unwrap();
        let mut fw = FloatingWindow::new(floating);
        fw.dock = Dock::Floating(Point::from((40, 50)));
        entry.floating_windows.push(fw);
        entry.alerts.push(Alert::new(alert));

        let area_origin = Point::from((10, 20));
        hut.sync_main_window_space(area_origin);

        let (mx, my, ..) = hut.floating_or_alert_absolute_rect(&main_surface).expect("main window should be mapped");
        assert_eq!((mx, my), (10, 20), "the Main Window maps at the usable area's own origin");

        let (fx, fy, ..) =
            hut.floating_or_alert_absolute_rect(&floating_surface).expect("floating window should be mapped");
        assert_eq!((fx, fy), (40, 50), "a Floating Window's tracked position is already absolute, like an Alert's — only the Main Window itself maps at area_origin");

        let (ax, ay, ..) = hut.floating_or_alert_absolute_rect(&alert_surface).expect("alert should be mapped");
        assert_eq!((ax, ay), (100, 100), "an Alert's tracked position is already absolute, not origin-relative");
    }

    #[test]
    fn sync_main_window_space_leaves_a_docked_floating_window_unmapped() {
        let mut hut = new_hut();
        let [(main, main_fth), (docked, _)] = spawn_test_windows(2).try_into().ok().unwrap();
        let main_surface = surface_of(&main);
        let docked_surface = surface_of(&docked);
        hut.push_main_window(main, true, main_fth);
        *hut.showing_terminal = false;
        hut.find_main_window_mut(&main_surface)
            .unwrap()
            .floating_windows
            .push(FloatingWindow::new(docked)); // starts Docked(Right) — see FloatingWindow::new

        hut.sync_main_window_space(Point::from((0, 0)));

        assert!(hut.floating_or_alert_absolute_rect(&docked_surface).is_none(), "docks.rs draws a handle for a docked window instead of mapping it");
        let _ = Edge::Right; // documents the default this test relies on
    }
}
