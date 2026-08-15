//! What's left of the pre-graph `Hut` tree (migration step 4's cutover —
//! see `docs/rfcs/typed-graph-hut.md`): `Direction`/`Axis` and
//! [`pane_rects`], still used throughout the typed graph (`graph_nodes.rs`'s
//! `TabNode::cycle`/`TileNode`, `graph_stack.rs`'s wrap/cycle logic,
//! `input.rs`'s pane hit-testing, `render.rs`'s Tile-Hut border
//! highlight) since none of these are Hut-tree-shaped concerns on their
//! own — just a wraparound-index helper and a proportional-split-rect
//! helper any container-shaped node can use.
//!
//! The actual `Hut`/`TabbedHut`/`TileHut` types this module used to
//! define are gone — `graph.rs`/`graph_nodes.rs`/`graph_stack.rs` are
//! their real replacement, verified against this module's own old test
//! suite (`stack.rs`'s tests, ported to `graph_stack.rs`) before this
//! file was pruned down to just what's still load-bearing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Next,
    Prev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    // Real and meaningful (`pane_rects`/`TileNode` both handle it
    // correctly) — just never actually constructed anywhere yet, since
    // `GraphStack::wrap_tile` always splits horizontally today, matching
    // the pre-graph `Hut::wrap_tile`'s own identical hardcoding. Nothing
    // currently exposes a way to wrap *vertically* instead (not a
    // regression from this migration — that gap predates it).
    #[allow(dead_code)]
    Vertical,
}

/// `pub(crate)`, not private — reused by `graph_nodes::TabNode::cycle`/
/// `TileNode::cycle` so the graph-native Tab/Tile nodes' own cycling is
/// provably the same wraparound logic this module always had, not a
/// structurally-similar reimplementation.
pub(crate) fn wrapping_step(len: usize, active: usize, dir: Direction) -> usize {
    match dir {
        Direction::Next => (active + 1) % len,
        Direction::Prev => (active + len - 1) % len,
    }
}

/// The new value an "active index into a Vec" should have after removing
/// the item at `removed_index` from it — shifts left by one if the
/// removed item sat *before* `active` (so `active` keeps pointing at the
/// same surviving item, now one slot earlier), otherwise just clamps to
/// the shrunk Vec's new bounds. A plain clamp alone only happens to give
/// the right answer when the removed item was the active one itself or
/// came after it — removing anything *before* `active` needs the shift
/// too, or `active` silently ends up pointing at the *next* item over
/// instead of the one that was actually selected. This exact bug shape
/// was found and fixed independently in two different places
/// (`graph_stack::GraphStack::remove_child`'s `TabNode`/`TileNode`
/// active index, `console_hut::ConsoleHut`'s own `active_main_window`)
/// before being pulled out here — one implementation to keep correct
/// instead of two (or more, later) copies that could silently diverge.
///
/// `new_len` is the Vec's length *after* removal (i.e. `old_len - 1`).
pub(crate) fn shift_active_index_on_removal(active: usize, removed_index: usize, new_len: usize) -> usize {
    let max_index = new_len.saturating_sub(1);
    if removed_index < active { active - 1 } else { active.min(max_index) }
}

#[cfg(test)]
mod shift_active_index_tests {
    use super::shift_active_index_on_removal;

    #[test]
    fn shifts_left_when_the_removed_item_sat_before_active() {
        // 4 items, active at index 2; removing index 0 should leave
        // active at index 1 (still the same surviving item), not clamped
        // (and wrong) at index 2.
        assert_eq!(shift_active_index_on_removal(2, 0, 3), 1);
    }

    #[test]
    fn clamps_when_the_removed_item_was_the_active_one_or_after_it() {
        // Removing the active item itself: clamps into the new bounds.
        assert_eq!(shift_active_index_on_removal(2, 2, 2), 1);
        // Removing something after active: active doesn't move at all.
        assert_eq!(shift_active_index_on_removal(1, 2, 2), 1);
    }

    #[test]
    fn clamps_to_zero_when_the_vec_becomes_empty() {
        assert_eq!(shift_active_index_on_removal(0, 0, 0), 0);
    }
}

/// Compute each of a tiled container's pane rectangles (`x, y, width,
/// height`, in whatever pixel space `size` itself is — physical or
/// logical, the caller's choice) from its axis and fractions, splitting
/// `size` along `axis` proportionally. Shared by every real caller
/// (`graph_nodes::TileNode`'s own `resolve`/`resize_to_pixels`,
/// `graph_stack::GraphStack`'s `leaf_absolute_rect`/`active_pane_offset`,
/// `input.rs`'s pane hit-testing, `render.rs`'s Tile-Hut border
/// highlight) so none of them can ever disagree about where a pane
/// actually is — the same reasoning `docks::Handle`'s shared rect uses.
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

    #[test]
    fn pane_rects_splits_proportionally_and_tiles_with_no_gap_or_overlap() {
        let rects = pane_rects(Axis::Horizontal, [0.5, 0.5].into_iter(), (200, 80));
        assert_eq!(rects, vec![(0, 0, 100, 80), (100, 0, 100, 80)]);
    }

    #[test]
    fn pane_rects_uneven_fracs_gives_the_last_pane_the_rounding_remainder() {
        let rects = pane_rects(Axis::Horizontal, [1.0, 2.0].into_iter(), (300, 80));
        assert_eq!(rects, vec![(0, 0, 100, 80), (100, 0, 200, 80)]);
    }

    #[test]
    fn pane_rects_vertical_axis_splits_by_height() {
        let rects = pane_rects(Axis::Vertical, [1.0, 1.0].into_iter(), (200, 80));
        assert_eq!(rects, vec![(0, 0, 200, 40), (0, 40, 200, 40)]);
    }

    #[test]
    fn wrapping_step_wraps_in_both_directions() {
        assert_eq!(wrapping_step(3, 0, Direction::Prev), 2);
        assert_eq!(wrapping_step(3, 2, Direction::Next), 0);
        assert_eq!(wrapping_step(3, 1, Direction::Next), 2);
    }
}
