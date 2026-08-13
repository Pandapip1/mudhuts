//! Phase 6: Villages — the general node type in the layout tree ("stuff
//! that can be alt-tabbed to, stuck in a tile, or put in a tab"). A
//! Village is either a [`Hut`] (leaf), a Tab-Village (tabbed group of
//! child Villages — only its `active` child is ever shown, cycled via
//! Meta+Left/Right), or a Tile-Village (manually tiled group — every
//! child is shown at once, side by side; `active` instead picks which
//! pane currently has keyboard focus). See the plan's Phase 6 notes and
//! the Nomenclature table.
//!
//! v1 scope, deliberately narrower than the full plan: a Tile-Village
//! pane always shows its Village's terminal, never a Main Window — the
//! plan's own notes flag "genuine simultaneous multi-Main-Window
//! visibility" as the one place the rest of the compositor's single
//! shared `Space<Window>` assumption breaks down, and that's a
//! substantially bigger, separately-riskier change than the rest of this
//! phase. Real side-by-side terminals are still fully real and useful on
//! their own; Main-Window-in-a-tile-pane is a tracked follow-up (see
//! `project_known_issues` memory). A Tile/Tab-Village nested *inside* a
//! tile pane also isn't given its own recursive split in v1 — it
//! resolves through [`Village::focused_hut`] like a Tab-Village would,
//! rather than splitting that pane further.

use smithay::backend::renderer::element::Id;

use crate::hut::Hut;
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

pub enum Village {
    // Boxed: `Hut` is a large struct (glyph caches, GPU renderer state,
    // ...) and `Village` gets moved around a lot (`Vec` insert/remove
    // during wrap/collapse) — without this, every `Village` (including
    // every `Tab`/`Tile` variant) would pay `Hut`'s full size regardless
    // of which variant it actually is.
    Hut(Box<Hut>),
    Tab(TabVillage),
    Tile(TileVillage),
}

pub struct TabVillage {
    pub children: Vec<Village>,
    pub active: usize,
    /// Each child's rendered tab-label texture, cached the same way
    /// `Hut`'s own tab caches are (see `render::LabelCache`'s doc
    /// comment) — one entry per child, kept in sync with `children`'s
    /// length by `village_chrome::build` (grown lazily) and
    /// [`Village::remove_child_hut`] (shrunk in lockstep with
    /// `children`) — a bare `Vec` alongside `children` rather than zipped
    /// into it, so `children` itself doesn't need to know anything about
    /// rendering.
    pub(crate) label_cache: Vec<LabelCache<(String, bool)>>,
    /// Each child's tab's stable (text, background) element ids —
    /// matters for the compositor's outer damage tracking, which
    /// compares elements by id between frames (see `Hut::element_id`'s
    /// doc comment for why a fresh `Id::new()` per frame is a real
    /// correctness bug, not cosmetic).
    pub tab_ids: Vec<(Id, Id)>,
    /// Each child's tab background's real damage tracking, bumped only
    /// when its active/inactive state flips (see `render::ChangeTracker`'s
    /// doc comment).
    pub(crate) bg_tracker: Vec<ChangeTracker<bool>>,
}

pub struct TileVillage {
    pub axis: Axis,
    /// Each child alongside its fraction of the tile's total extent along
    /// `axis` — expected to sum to 1.0, though nothing panics if they
    /// don't (see [`pane_rects`], which just uses them as relative
    /// weights).
    pub children: Vec<(Village, f64)>,
    /// Which pane currently has keyboard focus — distinct from a
    /// Tab-Village's `active`, which *also* controls visibility; every
    /// Tile-Village pane is visible regardless of this index.
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
}

fn wrapping_step(len: usize, active: usize, dir: Direction) -> usize {
    match dir {
        Direction::Next => (active + 1) % len,
        Direction::Prev => (active + len - 1) % len,
    }
}

impl Village {
    /// Combine `current` (shown last, so the wrap is visually a no-op)
    /// after `other` into a new Tab-Village.
    pub fn wrap_tab(other: Village, current: Village) -> Village {
        Village::Tab(TabVillage {
            children: vec![other, current],
            active: 1,
            label_cache: vec![LabelCache::new(), LabelCache::new()],
            tab_ids: vec![(Id::new(), Id::new()), (Id::new(), Id::new())],
            bg_tracker: vec![ChangeTracker::new(), ChangeTracker::new()],
        })
    }

    /// Combine `current` and `other` into a new Tile-Village, split evenly
    /// along `axis`. `current`'s pane keeps keyboard focus, matching
    /// `wrap_tab`'s "visually a no-op" property.
    pub fn wrap_tile(other: Village, current: Village, axis: Axis) -> Village {
        Village::Tile(TileVillage {
            axis,
            children: vec![(other, 0.5), (current, 0.5)],
            active: 1,
            highlight_ids: [Id::new(), Id::new(), Id::new(), Id::new()],
        })
    }

    /// Whether this Village should survive being "left behind" when The
    /// Stack moves away from it (see `stack::HutStack`'s `advance_forward`/
    /// `advance_backward`, and the plan's original Phase 3 discard rule).
    /// A bare Hut defers to its own [`Hut::touched`]; anything wrapped in
    /// a Tab/Tile-Village was deliberately composed by the user out of
    /// Villages already in use, so it's never a "never-touched, safe-to-
    /// discard" candidate the way a freshly auto-spawned bare Hut is.
    pub fn touched(&self) -> bool {
        match self {
            Village::Hut(hut) => hut.touched(),
            Village::Tab(_) | Village::Tile(_) => true,
        }
    }

    /// Marks the underlying Hut touched if this *is* one — a no-op for a
    /// Tab/Tile-Village, which is always already considered touched (see
    /// [`Self::touched`]).
    pub fn mark_touched(&mut self) {
        if let Village::Hut(hut) = self {
            hut.mark_touched();
        }
    }

    /// Resize every Hut anywhere under this Village to fill an output of
    /// `width`x`height` physical pixels — a Tab-Village's children all
    /// get the full size (only `active` is ever shown, but every child
    /// needs to track the real size so switching to it doesn't show a
    /// stale layout, matching the pre-Village `HutStack::resize_all`
    /// behavior); a Tile-Village's children each get their own pane's
    /// share instead (see [`pane_rects`]).
    pub fn resize_to_pixels(&mut self, width: i32, height: i32) {
        match self {
            Village::Hut(hut) => hut.resize_to_pixels(width, height),
            Village::Tab(tab) => {
                for child in &mut tab.children {
                    child.resize_to_pixels(width, height);
                }
            }
            Village::Tile(tile) => {
                let rects = pane_rects(tile.axis, tile.children.iter().map(|(_, frac)| *frac), (width, height));
                for ((child, _), (_, _, w, h)) in tile.children.iter_mut().zip(rects) {
                    child.resize_to_pixels(w, h);
                }
            }
        }
    }

    /// The Hut that currently has effective focus, walking down through
    /// whichever child is `active` at each Tab/Tile-Village level.
    pub fn focused_hut(&self) -> &Hut {
        match self {
            Village::Hut(hut) => hut,
            Village::Tab(tab) => tab.children[tab.active].focused_hut(),
            Village::Tile(tile) => tile.children[tile.active].0.focused_hut(),
        }
    }

    pub fn focused_hut_mut(&mut self) -> &mut Hut {
        match self {
            Village::Hut(hut) => hut,
            Village::Tab(tab) => tab.children[tab.active].focused_hut_mut(),
            Village::Tile(tile) => tile.children[tile.active].0.focused_hut_mut(),
        }
    }

    /// Find a Hut anywhere under this Village by id, recursively —
    /// unlike [`Self::focused_hut`], not limited to whichever child is
    /// currently active/visible (a background Main Window's owning Hut
    /// still needs to be reachable, e.g. for `handlers::xdg_shell`'s
    /// PID-ancestry lookup).
    pub fn find_hut_mut(&mut self, id: u64) -> Option<&mut Hut> {
        match self {
            Village::Hut(hut) if hut.id == id => Some(hut),
            Village::Hut(_) => None,
            Village::Tab(tab) => tab.children.iter_mut().find_map(|c| c.find_hut_mut(id)),
            Village::Tile(tile) => tile
                .children
                .iter_mut()
                .find_map(|(c, _)| c.find_hut_mut(id)),
        }
    }

    /// Every Hut anywhere under this Village, recursively — for searches
    /// that need to reach a background/inactive Hut too (PID-ancestry
    /// ownership, finding a window by surface across every Hut, resizing
    /// every Main Window on output resize), not just whatever's currently
    /// shown.
    pub fn all_huts(&self) -> Box<dyn Iterator<Item = &Hut> + '_> {
        match self {
            Village::Hut(hut) => Box::new(std::iter::once(hut.as_ref())),
            Village::Tab(tab) => Box::new(tab.children.iter().flat_map(|c| c.all_huts())),
            Village::Tile(tile) => Box::new(tile.children.iter().flat_map(|(c, _)| c.all_huts())),
        }
    }

    pub fn all_huts_mut(&mut self) -> Box<dyn Iterator<Item = &mut Hut> + '_> {
        match self {
            Village::Hut(hut) => Box::new(std::iter::once(hut.as_mut())),
            Village::Tab(tab) => Box::new(tab.children.iter_mut().flat_map(|c| c.all_huts_mut())),
            Village::Tile(tile) => Box::new(
                tile.children
                    .iter_mut()
                    .flat_map(|(c, _)| c.all_huts_mut()),
            ),
        }
    }

    /// Try to remove Hut `id` from among this Village's own children,
    /// recursively — only meaningful for a Tab/Tile-Village (a bare Hut
    /// has no children to remove from, so always returns `false`). If
    /// removing left exactly one child behind, [`Self::collapse_if_singleton`]
    /// replaces this Village with that child directly — an emptied-out
    /// wrapper around one survivor is never useful to keep around. See
    /// `stack::HutStack::remove_exited`, which calls this for a Hut
    /// that's exited but isn't a bare top-level Stack entry.
    pub fn remove_child_hut(&mut self, id: u64) -> bool {
        let removed = match self {
            Village::Hut(_) => false,
            Village::Tab(tab) => {
                let keep: Vec<bool> = tab
                    .children
                    .iter()
                    .map(|c| !matches!(c, Village::Hut(hut) if hut.id == id))
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
            Village::Tile(tile) => {
                let before = tile.children.len();
                tile.children
                    .retain(|(c, _)| !matches!(c, Village::Hut(hut) if hut.id == id));
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

    /// If this Village is a Tab/Tile-Village with exactly one child left,
    /// replace it with that child directly.
    fn collapse_if_singleton(&mut self) {
        let solo = match self {
            Village::Hut(_) => None,
            Village::Tab(tab) if tab.children.len() == 1 => tab.children.pop(),
            Village::Tile(tile) if tile.children.len() == 1 => tile.children.pop().map(|(c, _)| c),
            _ => None,
        };
        if let Some(child) = solo {
            *self = child;
        }
    }

    /// Meta+Left/Right's bubble-up step (see the plan's Meta+Left/Right
    /// resolution notes) — only called once the focused Hut itself has
    /// fewer than 2 Main Window tabs to cycle (that check happens one
    /// layer below this, in `input.rs`, via `Hut::cycle_tab`). Recurses
    /// into the active child *first* — "innermost first" — only cycling
    /// *this* level's `active` index if nothing deeper had anything to
    /// cycle. Returns whether anything was actually cycled, so a no-op
    /// call (a lone Hut, or every container along the active path having
    /// fewer than 2 children) is distinguishable from a real change.
    pub fn cycle_innermost(&mut self, dir: Direction) -> bool {
        match self {
            Village::Hut(_) => false,
            Village::Tab(tab) => {
                if tab.children[tab.active].cycle_innermost(dir) {
                    return true;
                }
                if tab.children.len() < 2 {
                    return false;
                }
                tab.active = wrapping_step(tab.children.len(), tab.active, dir);
                true
            }
            Village::Tile(tile) => {
                if tile.children[tile.active].0.cycle_innermost(dir) {
                    return true;
                }
                if tile.children.len() < 2 {
                    return false;
                }
                tile.active = wrapping_step(tile.children.len(), tile.active, dir);
                true
            }
        }
    }
}

/// Compute each of a Tile-Village's pane rectangles (`x, y, width,
/// height`, in whatever pixel space `size` itself is — physical or
/// logical, the caller's choice) from its axis and fractions, splitting
/// `size` along `axis` proportionally. Shared between
/// [`Village::resize_to_pixels`] (sizing each pane's terminal grid) and
/// `render.rs`'s Tile-Village compositing (positioning each pane's
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
