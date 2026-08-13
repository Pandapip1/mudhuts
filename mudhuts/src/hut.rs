//! `Hut` — the general, recursively composable node type in the layout
//! tree ("stuff that can be alt-tabbed to, stuck in a tile, or put in a
//! tab"). Renamed from the original "Village" as part of the composable
//! Hut hierarchy redesign (see `docs/rfcs/composable-hut-hierarchy.md`)
//! — the leaf that used to be called `Hut` is now [`ConsoleHut`]. A
//! `Hut` is either a [`ConsoleHut`] (leaf), a Tab-Hut (tabbed group of
//! child Huts — only its `active` child is ever shown, cycled via
//! Meta+Left/Right), or a Tile-Hut (manually tiled group — every
//! child is shown at once, side by side; `active` instead picks which
//! pane currently has keyboard focus). See the plan's Phase 6 notes and
//! the Nomenclature table.
//!
//! v1 scope, deliberately narrower than the full plan: a Tile-Hut
//! pane always shows its Console Hut's terminal, never a Main Window —
//! the plan's own notes flag "genuine simultaneous multi-Main-Window
//! visibility" as the one place the rest of the compositor's single
//! shared `Space<Window>` assumption breaks down, and that's a
//! substantially bigger, separately-riskier change than the rest of this
//! phase (the composable Hut hierarchy RFC's Q1 works out how that gap
//! eventually closes). Real side-by-side terminals are still fully real
//! and useful on their own; Main-Window-in-a-tile-pane is a tracked
//! follow-up (see `project_known_issues` memory). A Tile/Tab-Hut nested
//! *inside* a tile pane also isn't given its own recursive split in v1 —
//! it resolves through [`Hut::focused_hut`] like a Tab-Hut would, rather
//! than splitting that pane further.

use smithay::backend::renderer::element::Id;

use crate::console_hut::ConsoleHut;
use crate::redraw::{Redrawable, RedrawHandle};
use crate::render::{ChangeTracker, LabelCache};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Next,
    Prev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

pub enum Hut {
    // Boxed: `ConsoleHut` is a large struct (glyph caches, GPU renderer state,
    // ...) and `Hut` gets moved around a lot (`Vec` insert/remove
    // during wrap/collapse) — without this, every `Hut` (including
    // every `Tab`/`Tile` variant) would pay `ConsoleHut`'s full size regardless
    // of which variant it actually is.
    Console(Box<ConsoleHut>),
    Tab(TabbedHut),
    Tile(TileHut),
}

pub struct TabbedHut {
    pub children: Vec<Hut>,
    pub active: usize,
    /// Each child's rendered tab-label texture, cached the same way
    /// `ConsoleHut`'s own tab caches are (see `render::LabelCache`'s doc
    /// comment) — one entry per child, kept in sync with `children`'s
    /// length by `village_chrome::build` (grown lazily) and
    /// [`Hut::remove_child_hut`] (shrunk in lockstep with
    /// `children`) — a bare `Vec` alongside `children` rather than zipped
    /// into it, so `children` itself doesn't need to know anything about
    /// rendering.
    pub(crate) label_cache: Vec<LabelCache<(String, bool)>>,
    /// Each child's tab's stable (text, background) element ids —
    /// matters for the compositor's outer damage tracking, which
    /// compares elements by id between frames (see `ConsoleHut::element_id`'s
    /// doc comment for why a fresh `Id::new()` per frame is a real
    /// correctness bug, not cosmetic).
    pub tab_ids: Vec<(Id, Id)>,
    /// Each child's tab background's real damage tracking, bumped only
    /// when its active/inactive state flips (see `render::ChangeTracker`'s
    /// doc comment).
    pub(crate) bg_tracker: Vec<ChangeTracker<bool>>,
    /// Composable Hut hierarchy RFC migration step 4: set via
    /// [`Hut::attach_redraw_handle`] (recursively, from
    /// `stack::MruStackHut::wrap_tab`/`wrap_tile`) — [`Self::set_active`]
    /// calls `mark_dirty()` on it itself, so nothing that changes which
    /// child tab is active can forget to request a redraw. `None` only
    /// momentarily, for a freshly-constructed node before that attach call
    /// runs (see `Hut::wrap_focused`'s placeholder).
    redraw: Option<RedrawHandle>,
}

impl TabbedHut {
    /// Switch which child is active, requesting a redraw — the one place
    /// this should ever be set from outside `hut.rs`, so the redraw can't
    /// be forgotten (see [`Self::redraw`]'s doc comment). Internal
    /// bookkeeping (`remove_child_hut`'s clamp on removal) still writes
    /// `active` directly — nothing to redraw differently there, since the
    /// element being removed is already forcing a redraw of its own.
    pub fn set_active(&mut self, index: usize) {
        self.active = index;
        if let Some(redraw) = &self.redraw {
            redraw.mark_dirty();
        }
    }
}

pub struct TileHut {
    pub axis: Axis,
    /// Each child alongside its fraction of the tile's total extent along
    /// `axis` — expected to sum to 1.0, though nothing panics if they
    /// don't (see [`pane_rects`], which just uses them as relative
    /// weights).
    pub children: Vec<(Hut, f64)>,
    /// Which pane currently has keyboard focus — distinct from a
    /// Tab-Hut's `active`, which *also* controls visibility; every
    /// Tile-Hut pane is visible regardless of this index.
    pub active: usize,
    /// Stable identities for the 4 border strips (top/bottom/left/right)
    /// drawn around whichever pane is `active` — reused across frames
    /// regardless of *which* pane that is: each strip's *geometry*
    /// simply follows `active` frame to frame, which the compositor's
    /// own per-element damage tracking already handles correctly on its
    /// own (a moving/resizing element is damaged wherever it was and
    /// wherever it now is, same as a moved window) — no `ChangeTracker`
    /// needed the way a fixed-geometry element's *content*-only change
    /// would (see `render::ChangeTracker`'s doc comment for that other
    /// case). 4 *distinct* ids, not 1 reused 4 times — every element in a
    /// single frame needs its own identity, or the damage tracker can't
    /// tell the 4 strips apart.
    pub(crate) highlight_ids: [Id; 4],
    /// See [`TabbedHut::redraw`]'s doc comment — same purpose, for
    /// [`Self::set_active`] (which pane has keyboard focus).
    redraw: Option<RedrawHandle>,
}

impl TileHut {
    /// See [`TabbedHut::set_active`] — same purpose, for which pane has
    /// keyboard focus.
    pub fn set_active(&mut self, index: usize) {
        self.active = index;
        if let Some(redraw) = &self.redraw {
            redraw.mark_dirty();
        }
    }

    /// Every pane's rectangle in absolute physical-pixel space — `area`
    /// is `State::usable_area()`'s `(x, y, w, h)`. The single computation
    /// shared by rendering (`render.rs::build_tile_elements`), pane-click
    /// hit-testing (`input.rs::try_click_chrome`), and the active pane's
    /// offset alone (`state.rs::active_pane_offset`) — composable Hut
    /// hierarchy RFC migration step 4, consolidating Q3's flagged
    /// triplication of "usable_area() + pane_rects() + add the origin
    /// back" into one place, so the three can never disagree about where
    /// a pane actually is.
    pub fn absolute_pane_rects(&self, area: (i32, i32, i32, i32)) -> Vec<(i32, i32, i32, i32)> {
        let (area_x, area_y, area_w, area_h) = area;
        pane_rects(self.axis, self.children.iter().map(|(_, frac)| *frac), (area_w, area_h))
            .into_iter()
            .map(|(x, y, w, h)| (x + area_x, y + area_y, w, h))
            .collect()
    }
}

fn wrapping_step(len: usize, active: usize, dir: Direction) -> usize {
    match dir {
        Direction::Next => (active + 1) % len,
        Direction::Prev => (active + len - 1) % len,
    }
}

impl Hut {
    /// Combine `current` (shown last, so the wrap is visually a no-op)
    /// after `other` into a new Tab-Hut.
    pub fn wrap_tab(other: Hut, current: Hut) -> Hut {
        Hut::Tab(TabbedHut {
            children: vec![other, current],
            active: 1,
            label_cache: vec![LabelCache::new(), LabelCache::new()],
            tab_ids: vec![(Id::new(), Id::new()), (Id::new(), Id::new())],
            bg_tracker: vec![ChangeTracker::new(), ChangeTracker::new()],
            redraw: None,
        })
    }

    /// Combine `current` and `other` into a new Tile-Hut, split evenly
    /// along `axis`. `current`'s pane keeps keyboard focus, matching
    /// `wrap_tab`'s "visually a no-op" property.
    pub fn wrap_tile(other: Hut, current: Hut, axis: Axis) -> Hut {
        Hut::Tile(TileHut {
            axis,
            children: vec![(other, 0.5), (current, 0.5)],
            active: 1,
            highlight_ids: [Id::new(), Id::new(), Id::new(), Id::new()],
            redraw: None,
        })
    }

    /// Wrap *in place* whichever leaf `focused_hut`/`focused_hut_mut`
    /// would reach from here (following each level's `active` index all
    /// the way down to the actual bare ConsoleHut) — `make(old)` replaces just
    /// that leaf, leaving every sibling and every ancestor container
    /// completely untouched. This is `wrap_tab`/`wrap_tile`'s whole
    /// point: pressing wrap-tab while focused on one pane of an existing
    /// Tile-Hut should turn *that pane* into a small Tab-Hut,
    /// not disturb the Tile-Hut itself or reach for some unrelated
    /// top-level Stack entry to combine with (`stack::MruStackHut::wrap_tab`
    /// always passes a freshly spawned ConsoleHut as one side, never an
    /// existing entry, for exactly the same reason — see its doc
    /// comment).
    ///
    /// Implementation note: reaching the leaf requires temporarily
    /// moving it out of `self` to hand to `make` (which needs it by
    /// value) and then writing the result back — `std::mem::replace`
    /// needs *some* placeholder value in between, so an empty
    /// (harmless, momentary) `Hut::Tab` stands in for the instant
    /// between the two assignments; nothing ever observes it.
    pub fn wrap_focused(&mut self, make: impl FnOnce(Hut) -> Hut) {
        if matches!(self, Hut::Console(_)) {
            let placeholder = Hut::Tab(TabbedHut {
                children: Vec::new(),
                active: 0,
                label_cache: Vec::new(),
                tab_ids: Vec::new(),
                bg_tracker: Vec::new(),
                redraw: None,
            });
            let old = std::mem::replace(self, placeholder);
            *self = make(old);
            return;
        }
        match self {
            Hut::Tab(tab) => tab.children[tab.active].wrap_focused(make),
            Hut::Tile(tile) => tile.children[tile.active].0.wrap_focused(make),
            Hut::Console(_) => unreachable!("checked above"),
        }
    }

    /// Whether this Hut should survive being "left behind" when The
    /// Stack moves away from it (see `stack::MruStackHut`'s `advance_forward`/
    /// `advance_backward`, and the plan's original Phase 3 discard rule).
    /// A bare Console Hut defers to its own [`ConsoleHut::touched`]; anything wrapped in
    /// a Tab/Tile-Hut was deliberately composed by the user out of
    /// Huts already in use, so it's never a "never-touched, safe-to-
    /// discard" candidate the way a freshly auto-spawned bare Console Hut is.
    pub fn touched(&self) -> bool {
        match self {
            Hut::Console(hut) => hut.touched(),
            Hut::Tab(_) | Hut::Tile(_) => true,
        }
    }

    /// Marks the underlying ConsoleHut touched if this *is* one — a no-op for a
    /// Tab/Tile-Hut, which is always already considered touched (see
    /// [`Self::touched`]).
    pub fn mark_touched(&mut self) {
        if let Hut::Console(hut) = self {
            hut.mark_touched();
        }
    }

    /// Resize every Console Hut anywhere under this Hut to fill an output of
    /// `width`x`height` physical pixels — a Tab-Hut's children all
    /// get the full size (only `active` is ever shown, but every child
    /// needs to track the real size so switching to it doesn't show a
    /// stale layout, matching `Stack::resize_all`'s original pre-redesign
    /// behavior); a Tile-Hut's children each get their own pane's
    /// share instead (see [`pane_rects`]).
    pub fn resize_to_pixels(&mut self, width: i32, height: i32) {
        match self {
            Hut::Console(hut) => hut.resize_to_pixels(width, height),
            Hut::Tab(tab) => {
                for child in &mut tab.children {
                    child.resize_to_pixels(width, height);
                }
            }
            Hut::Tile(tile) => {
                let rects = pane_rects(tile.axis, tile.children.iter().map(|(_, frac)| *frac), (width, height));
                for ((child, _), (_, _, w, h)) in tile.children.iter_mut().zip(rects) {
                    child.resize_to_pixels(w, h);
                }
            }
        }
    }

    /// The ConsoleHut that currently has effective focus, walking down through
    /// whichever child is `active` at each Tab/Tile-Hut level.
    pub fn focused_hut(&self) -> &ConsoleHut {
        match self {
            Hut::Console(hut) => hut,
            Hut::Tab(tab) => tab.children[tab.active].focused_hut(),
            Hut::Tile(tile) => tile.children[tile.active].0.focused_hut(),
        }
    }

    pub fn focused_hut_mut(&mut self) -> &mut ConsoleHut {
        match self {
            Hut::Console(hut) => hut,
            Hut::Tab(tab) => tab.children[tab.active].focused_hut_mut(),
            Hut::Tile(tile) => tile.children[tile.active].0.focused_hut_mut(),
        }
    }

    /// Find a ConsoleHut anywhere under this Hut by id, recursively —
    /// unlike [`Self::focused_hut`], not limited to whichever child is
    /// currently active/visible (a background Main Window's owning ConsoleHut
    /// still needs to be reachable, e.g. for `handlers::xdg_shell`'s
    /// PID-ancestry lookup).
    pub fn find_hut_mut(&mut self, id: u64) -> Option<&mut ConsoleHut> {
        match self {
            Hut::Console(hut) if hut.id == id => Some(hut),
            Hut::Console(_) => None,
            Hut::Tab(tab) => tab.children.iter_mut().find_map(|c| c.find_hut_mut(id)),
            Hut::Tile(tile) => tile
                .children
                .iter_mut()
                .find_map(|(c, _)| c.find_hut_mut(id)),
        }
    }

    /// Every ConsoleHut anywhere under this Hut, recursively — for searches
    /// that need to reach a background/inactive ConsoleHut too (PID-ancestry
    /// ownership, finding a window by surface across every ConsoleHut, resizing
    /// every Main Window on output resize), not just whatever's currently
    /// shown.
    pub fn all_huts(&self) -> Box<dyn Iterator<Item = &ConsoleHut> + '_> {
        match self {
            Hut::Console(hut) => Box::new(std::iter::once(hut.as_ref())),
            Hut::Tab(tab) => Box::new(tab.children.iter().flat_map(|c| c.all_huts())),
            Hut::Tile(tile) => Box::new(tile.children.iter().flat_map(|(c, _)| c.all_huts())),
        }
    }

    pub fn all_huts_mut(&mut self) -> Box<dyn Iterator<Item = &mut ConsoleHut> + '_> {
        match self {
            Hut::Console(hut) => Box::new(std::iter::once(hut.as_mut())),
            Hut::Tab(tab) => Box::new(tab.children.iter_mut().flat_map(|c| c.all_huts_mut())),
            Hut::Tile(tile) => Box::new(
                tile.children
                    .iter_mut()
                    .flat_map(|(c, _)| c.all_huts_mut()),
            ),
        }
    }

    /// Try to remove ConsoleHut `id` from among this Hut's own children,
    /// recursively — only meaningful for a Tab/Tile-Hut (a bare ConsoleHut
    /// has no children to remove from, so always returns `false`). If
    /// removing left exactly one child behind, [`Self::collapse_if_singleton`]
    /// replaces this Hut with that child directly — an emptied-out
    /// wrapper around one survivor is never useful to keep around. See
    /// `stack::MruStackHut::remove_exited`, which calls this for a ConsoleHut
    /// that's exited but isn't a bare top-level Stack entry.
    pub fn remove_child_hut(&mut self, id: u64) -> bool {
        let removed = match self {
            Hut::Console(_) => false,
            Hut::Tab(tab) => {
                let keep: Vec<bool> = tab
                    .children
                    .iter()
                    .map(|c| !matches!(c, Hut::Console(hut) if hut.id == id))
                    .collect();
                if keep.iter().any(|k| !k) {
                    let mut kept = keep.iter();
                    tab.children.retain(|_| *kept.next().unwrap());
                    let mut kept = keep.iter();
                    tab.label_cache
                        .retain(|_| kept.next().copied().unwrap_or(true));
                    let mut kept = keep.iter();
                    tab.tab_ids.retain(|_| kept.next().copied().unwrap_or(true));
                    let mut kept = keep.iter();
                    tab.bg_tracker
                        .retain(|_| kept.next().copied().unwrap_or(true));
                    tab.active = tab.active.min(tab.children.len().saturating_sub(1));
                    true
                } else {
                    tab.children.iter_mut().any(|c| c.remove_child_hut(id))
                }
            }
            Hut::Tile(tile) => {
                let before = tile.children.len();
                tile.children
                    .retain(|(c, _)| !matches!(c, Hut::Console(hut) if hut.id == id));
                if tile.children.len() != before {
                    tile.active = tile.active.min(tile.children.len().saturating_sub(1));
                    true
                } else {
                    tile.children.iter_mut().any(|(c, _)| c.remove_child_hut(id))
                }
            }
        };
        if removed {
            self.collapse_if_singleton();
        }
        removed
    }

    /// If this Hut is a Tab/Tile-Hut with exactly one child left,
    /// replace it with that child directly.
    fn collapse_if_singleton(&mut self) {
        let solo = match self {
            Hut::Console(_) => None,
            Hut::Tab(tab) if tab.children.len() == 1 => tab.children.pop(),
            Hut::Tile(tile) if tile.children.len() == 1 => tile.children.pop().map(|(c, _)| c),
            _ => None,
        };
        if let Some(child) = solo {
            *self = child;
        }
    }

    /// Meta+Left/Right's bubble-up step (see the plan's Meta+Left/Right
    /// resolution notes) — only called once the focused ConsoleHut itself has
    /// fewer than 2 Main Window tabs to cycle (that check happens one
    /// layer below this, in `input.rs`, via `ConsoleHut::cycle_tab`). Recurses
    /// into the active child *first* — "innermost first" — only cycling
    /// *this* level's `active` index if nothing deeper had anything to
    /// cycle. Returns whether anything was actually cycled, so a no-op
    /// call (a lone ConsoleHut, or every container along the active path having
    /// fewer than 2 children) is distinguishable from a real change.
    pub fn cycle_innermost(&mut self, dir: Direction) -> bool {
        match self {
            Hut::Console(_) => false,
            Hut::Tab(tab) => {
                if tab.children[tab.active].cycle_innermost(dir) {
                    return true;
                }
                if tab.children.len() < 2 {
                    return false;
                }
                tab.set_active(wrapping_step(tab.children.len(), tab.active, dir));
                true
            }
            Hut::Tile(tile) => {
                if tile.children[tile.active].0.cycle_innermost(dir) {
                    return true;
                }
                if tile.children.len() < 2 {
                    return false;
                }
                tile.set_active(wrapping_step(tile.children.len(), tile.active, dir));
                true
            }
        }
    }
}

impl Redrawable for Hut {
    /// Recurses into every descendant, not just this node — a Tab/Tile-Hut
    /// might contain other Tab/Tile-Huts nested arbitrarily deep (see the
    /// module doc's v1 scope note), and every one of them needs the same
    /// handle so [`TabbedHut::set_active`]/[`TileHut::set_active`] can mark
    /// a redraw regardless of how deep the click/cycle that changed them
    /// was. A no-op for a bare `Console` leaf — `ConsoleHut` isn't part of
    /// this migration step (see the RFC's step 4 notes).
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        match self {
            Hut::Console(_) => {}
            Hut::Tab(tab) => {
                tab.redraw = Some(handle.clone());
                for child in &mut tab.children {
                    child.attach_redraw_handle(handle.clone());
                }
            }
            Hut::Tile(tile) => {
                tile.redraw = Some(handle.clone());
                for (child, _) in &mut tile.children {
                    child.attach_redraw_handle(handle.clone());
                }
            }
        }
    }
}

/// Compute each of a Tile-Hut's pane rectangles (`x, y, width,
/// height`, in whatever pixel space `size` itself is — physical or
/// logical, the caller's choice) from its axis and fractions, splitting
/// `size` along `axis` proportionally. Shared between
/// [`Hut::resize_to_pixels`] (sizing each pane's terminal grid) and
/// `render.rs`'s Tile-Hut compositing (positioning each pane's
/// texture) so the two can never disagree about where a pane actually is
/// — the same reasoning as `docks::Handle`'s shared rect.
pub fn pane_rects(
    axis: Axis,
    fracs: impl Iterator<Item = f64> + Clone,
    size: (i32, i32),
) -> Vec<(i32, i32, i32, i32)> {
    let (width, height) = size;
    let total = fracs.clone().sum::<f64>().max(f64::EPSILON);
    let extent = match axis {
        Axis::Horizontal => width,
        Axis::Vertical => height,
    };

    let mut rects = Vec::new();
    let mut offset = 0;
    let fracs: Vec<f64> = fracs.collect();
    for (i, frac) in fracs.iter().enumerate() {
        // The last pane absorbs any leftover pixel from rounding, so the
        // panes always exactly tile the full extent with no gap/overlap.
        let this_extent = if i + 1 == fracs.len() {
            extent - offset
        } else {
            ((frac / total) * extent as f64).round() as i32
        };
        let rect = match axis {
            Axis::Horizontal => (offset, 0, this_extent, height),
            Axis::Vertical => (0, offset, width, this_extent),
        };
        rects.push(rect);
        offset += this_extent;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cheap placeholder child — an empty Tab-Hut, not a real
    /// `ConsoleHut` (which would need a spawned shell process, unlike
    /// `stack.rs`'s tests) — good enough for tests that only care about
    /// `TileHut`'s own rect math, never resolve to a leaf.
    fn placeholder() -> Hut {
        Hut::Tab(TabbedHut {
            children: Vec::new(),
            active: 0,
            label_cache: Vec::new(),
            tab_ids: Vec::new(),
            bg_tracker: Vec::new(),
            redraw: None,
        })
    }

    fn tile(axis: Axis, fracs: [f64; 2]) -> TileHut {
        TileHut {
            axis,
            children: vec![(placeholder(), fracs[0]), (placeholder(), fracs[1])],
            active: 0,
            highlight_ids: [Id::new(), Id::new(), Id::new(), Id::new()],
            redraw: None,
        }
    }

    #[test]
    fn absolute_pane_rects_offsets_every_pane_by_the_areas_origin() {
        let t = tile(Axis::Horizontal, [0.5, 0.5]);
        let area = (100, 50, 200, 80); // x, y, w, h
        let rects = t.absolute_pane_rects(area);
        assert_eq!(rects.len(), 2);
        // Same split `pane_rects` alone would produce, shifted by the
        // area's origin — this is the whole point of the shared method
        // (see its doc comment): every caller gets exactly this, never a
        // hand-rolled re-derivation that could drift from it.
        let bare = pane_rects(Axis::Horizontal, [0.5, 0.5].into_iter(), (200, 80));
        for ((x, y, w, h), (bx, by, bw, bh)) in rects.iter().zip(bare) {
            assert_eq!(*x, bx + 100);
            assert_eq!(*y, by + 50);
            assert_eq!(*w, bw);
            assert_eq!(*h, bh);
        }
    }

    #[test]
    fn set_active_marks_the_attached_redraw_handle_dirty() {
        let mut t = tile(Axis::Horizontal, [0.5, 0.5]);
        let (ping, source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        t.redraw = Some(RedrawHandle::new(ping));

        t.set_active(1);
        assert_eq!(t.active, 1);

        let mut event_loop: smithay::reexports::calloop::EventLoop<'static, ()> =
            smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        let fired_clone = fired.clone();
        event_loop
            .handle()
            .insert_source(source, move |_, _, _| fired_clone.set(true))
            .unwrap();
        event_loop
            .dispatch(std::time::Duration::from_millis(0), &mut ())
            .unwrap();
        assert!(fired.get(), "set_active should have pinged the attached RedrawHandle");
    }
}
