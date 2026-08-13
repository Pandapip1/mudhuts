//! A Main Window and the Sub-Windows/Alerts tagged as belonging to it —
//! see the plan's Phase 5 notes and the Nomenclature table.

use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};

use crate::render::{ChangeTracker, LabelCache};

/// Which screen edge a docked Sub-Window is minimized to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Where a Sub-Window currently is. Docked ones aren't mapped as a real
/// surface at all (there's nothing to composite — see `docks.rs`, which
/// draws a small handle instead); floating ones are mapped normally at
/// the given position.
#[derive(Debug, Clone, Copy)]
pub enum Dock {
    Docked(Edge),
    Floating(Point<i32, Logical>),
}

pub struct SubWindow {
    pub window: Window,
    pub dock: Dock,
    /// Stable identities for this Sub-Window's docked handle in
    /// `docks.rs` (its title-label text and background) — same reasoning
    /// as `MainWindowEntry::tab_text_id`/`tab_bg_id`.
    pub handle_text_id: Id,
    pub handle_bg_id: Id,
    /// Caches the handle's rendered title-label texture, only actually
    /// re-rendering it (real GPU work) when the title changes — see
    /// `render::LabelCache`'s doc comment. The handle's background color
    /// never changes (no active/inactive state, unlike a tab), so it
    /// stays genuinely static and needs no equivalent cache.
    pub(crate) handle_text_cache: LabelCache<String>,
}

impl SubWindow {
    /// A freshly-tagged Sub-Window starts docked — defaulting to the
    /// right edge. Nothing in `mudhuts_window_role_v1` hints at a
    /// preferred edge, and the user can drag it to any of the other 3
    /// immediately anyway.
    pub fn new(window: Window) -> Self {
        Self {
            window,
            dock: Dock::Docked(Edge::Right),
            handle_text_id: Id::new(),
            handle_bg_id: Id::new(),
            handle_text_cache: LabelCache::new(),
        }
    }

    pub fn matches(&self, surface: &WlSurface) -> bool {
        self.window
            .toplevel()
            .is_some_and(|t| t.wl_surface() == surface)
    }
}

/// Always floating, never docked/minimized — see the plan's Phase 5 notes
/// on Alerts. Tracks its own position explicitly (rather than relying on
/// `state.space` to remember it) since `sync_visible_main_window` unmaps
/// and remaps everything on every focus/visibility change, which would
/// otherwise lose it.
pub struct Alert {
    pub window: Window,
    pub position: Point<i32, Logical>,
}

impl Alert {
    /// No protocol hint gives a preferred position, and centering needs
    /// the current output size (not available where Alerts are created,
    /// in the shell protocol handler) — starts at a simple fixed offset;
    /// the same drag mechanic as Sub-Windows (Phase 5's move grab) lets
    /// the user put it wherever from there.
    pub fn new(window: Window) -> Self {
        Self {
            window,
            position: Point::from((100, 100)),
        }
    }

    pub fn matches(&self, surface: &WlSurface) -> bool {
        self.window
            .toplevel()
            .is_some_and(|t| t.wl_surface() == surface)
    }
}

/// One Main Window (a client toplevel presented as a tab within its Hut)
/// plus whatever's been tagged as belonging to it.
pub struct MainWindowEntry {
    pub window: Window,
    pub sub_windows: Vec<SubWindow>,
    pub alerts: Vec<Alert>,
    /// Stable identities for this Main Window's tab in `chrome.rs`'s tab
    /// strip (its text label and background) — created once here and
    /// reused across every frame's `build()` call, matching `Hut::element_id`'s
    /// pattern; see `Hut::terminal_tab_text_id`'s doc comment for why a
    /// fresh `Id::new()` per frame is a real correctness bug, not cosmetic.
    pub tab_text_id: Id,
    pub tab_bg_id: Id,
    /// Caches this tab's rendered text label, only actually re-rendering
    /// it (real GPU work) when the title or active/inactive state
    /// changes — see `render::LabelCache`'s doc comment. Its background
    /// gets real damage tracking the same way, bumped on the same
    /// change — see `render::ChangeTracker`'s doc comment for why a
    /// fixed `CommitCounter` (reused every frame regardless of content)
    /// would mean the outer tracker never sees this tab's color change
    /// again after the first frame it's drawn.
    pub(crate) tab_text_cache: LabelCache<(String, bool)>,
    tab_bg_tracker: ChangeTracker<bool>,
}

impl MainWindowEntry {
    pub fn new(window: Window) -> Self {
        Self {
            window,
            sub_windows: Vec::new(),
            alerts: Vec::new(),
            tab_text_id: Id::new(),
            tab_bg_id: Id::new(),
            tab_text_cache: LabelCache::new(),
            tab_bg_tracker: ChangeTracker::new(),
        }
    }

    pub fn matches(&self, surface: &WlSurface) -> bool {
        self.window
            .toplevel()
            .is_some_and(|t| t.wl_surface() == surface)
    }

    /// A commit counter for this tab's background element, bumped only if
    /// `active` differs from last frame's.
    pub fn tab_bg_commit(&mut self, active: bool) -> CommitCounter {
        self.tab_bg_tracker.commit(active)
    }
}
