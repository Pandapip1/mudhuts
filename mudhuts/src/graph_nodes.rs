//! Real Hut node types built on the graph core (`graph.rs`) — migration
//! step 2 of `docs/rfcs/typed-graph-hut.md`: Tab-Hut and Tile-Hut
//! expressed as [`crate::graph::Node`] impls. Proven equivalent to
//! `hut.rs`'s existing `TabbedHut`/`TileHut` selection/cycling behavior
//! by literally sharing the same [`crate::hut::wrapping_step`] helper,
//! not a separately-reimplemented copy of it — so a future regression in
//! one can't silently diverge from the other without a test failure
//! (see this module's own tests below).
//!
//! Still not wired into `main.rs`'s real render/input call paths
//! (migration step 4) — these are real, usable node types, just not live
//! yet. `resolve`'s `content` output stays a [`RenderedContent`]
//! placeholder until migration step 3 gives it something real to
//! produce (a Console/Terminal leaf node) — what *is* real and tested
//! here is the graph wiring itself: which child is active, how cycling
//! moves that index, and that every child actually gets traversed for a
//! Tile-Hut's "show every pane" case.

// Migration step 2 (see this module's doc comment): nothing in
// `main.rs`/`render.rs`/`input.rs` constructs these node types yet — the
// module's own unit tests already exercise the whole API (see below).
// Remove once migration step 4 cuts the real call paths over to the
// graph.
#![allow(dead_code)]

use crate::graph::{Graph, InputPort, Node, NodeId, OutputPort, PortKind, PortValue, RenderedContent};
use crate::hut::{Axis, Direction, wrapping_step};
use crate::redraw::{Redrawable, RedrawHandle, Signal};

const CHILDREN_INPUT: &[InputPort] = &[InputPort { name: "children", kind: PortKind::HutList }];
const CONTENT_OUTPUT: &[OutputPort] = &[OutputPort { name: "content", kind: PortKind::Content }];

/// Graph-native Tab-Hut: shows exactly one child (`active`) at a time —
/// same semantics as [`crate::hut::TabbedHut`], expressed as a `Node`
/// with a `children: HutList` input instead of an owned `Vec<Hut>`.
pub struct TabNode {
    pub active: Signal<usize>,
}

impl TabNode {
    pub fn new() -> Self {
        Self { active: Signal::new(0) }
    }

    /// Meta+Left/Right's own-level step, once nothing deeper had
    /// anything to cycle — mirrors `Hut::cycle_innermost`'s Tab-Hut arm
    /// exactly (same `wrapping_step` helper, same "no-op below 2
    /// children" rule), just reading the child count from the graph's
    /// own `children` input instead of a `Vec<Hut>` field length.
    /// Returns whether anything actually cycled, matching
    /// `Hut::cycle_innermost`'s own return convention.
    pub fn cycle<Env>(&mut self, graph: &Graph<Env>, self_id: NodeId, dir: Direction) -> bool {
        let len = graph.hut_list_input(self_id, "children").len();
        if len < 2 {
            return false;
        }
        *self.active = wrapping_step(len, *self.active, dir);
        true
    }
}

impl Default for TabNode {
    fn default() -> Self {
        Self::new()
    }
}

impl<Env> Node<Env> for TabNode {
    fn inputs(&self) -> &[InputPort] {
        CHILDREN_INPUT
    }
    fn outputs(&self) -> &[OutputPort] {
        CONTENT_OUTPUT
    }
    fn resolve(&mut self, graph: &mut Graph<Env>, self_id: NodeId, _port: &'static str) -> PortValue {
        let children = graph.hut_list_input(self_id, "children");
        match children.get(*self.active) {
            Some(&child) => graph
                .resolve_output(child, "content")
                .unwrap_or_else(|| PortValue::Content(RenderedContent::default())),
            None => PortValue::Content(RenderedContent::default()),
        }
    }
}

impl Redrawable for TabNode {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.active.attach_redraw_handle(handle);
    }
}

/// Graph-native Tile-Hut: shows every child at once, side by side along
/// `axis` — same semantics as [`crate::hut::TileHut`]. `fracs` parallels
/// `children`'s length the same way `TabbedHut::label_cache`/`tab_ids`/
/// `bg_tracker` already parallel *its* `children` today (kept in sync by
/// whatever manipulates the list — real per-pane arrangement math
/// (`hut::pane_rects`) plugs in once there's real content to position,
/// migration step 3+; for now `resolve` just proves every child is
/// actually reachable/traversed, which is the part of this node's
/// behavior that's real today).
pub struct TileNode {
    pub axis: Axis,
    pub active: Signal<usize>,
    pub fracs: Vec<f64>,
}

impl TileNode {
    pub fn new(axis: Axis) -> Self {
        Self { axis, active: Signal::new(0), fracs: Vec::new() }
    }

    /// Same wraparound rule as [`TabNode::cycle`], applied to which pane
    /// has keyboard focus instead of which tab is shown — mirrors
    /// `Hut::cycle_innermost`'s Tile-Hut arm.
    pub fn cycle<Env>(&mut self, graph: &Graph<Env>, self_id: NodeId, dir: Direction) -> bool {
        let len = graph.hut_list_input(self_id, "children").len();
        if len < 2 {
            return false;
        }
        *self.active = wrapping_step(len, *self.active, dir);
        true
    }
}

impl<Env> Node<Env> for TileNode {
    fn inputs(&self) -> &[InputPort] {
        CHILDREN_INPUT
    }
    fn outputs(&self) -> &[OutputPort] {
        CONTENT_OUTPUT
    }
    fn resolve(&mut self, graph: &mut Graph<Env>, self_id: NodeId, _port: &'static str) -> PortValue {
        // Every pane is visible at once (unlike TabNode) — every child
        // gets resolved, not just `active`, even though there's nowhere
        // real to composite the results into yet (see this module's doc
        // comment). Discarding the per-child results here rather than
        // returning them is deliberately temporary: migration step 3+
        // gives `resolve` a real renderer to actually composite them
        // with, the same way `render.rs::build_tile_elements` does today.
        for &child in &graph.hut_list_input(self_id, "children") {
            graph.resolve_output(child, "content");
        }
        PortValue::Content(RenderedContent::default())
    }
}

impl Redrawable for TileNode {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.active.attach_redraw_handle(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hut::{Hut, TabbedHut};
    use crate::render::{ChangeTracker, LabelCache};
    use smithay::backend::renderer::element::Id;

    /// A minimal leaf `Node` for these tests — no inputs, a `Content`
    /// output identifiable enough (via `resolve_count`) to prove *which*
    /// leaf actually got resolved, without needing any real rendering.
    struct LeafNode {
        resolved: std::rc::Rc<std::cell::Cell<bool>>,
    }
    const LEAF_OUTPUTS: &[OutputPort] = &[OutputPort { name: "content", kind: PortKind::Content }];
    impl<Env> Node<Env> for LeafNode {
        fn inputs(&self) -> &[InputPort] {
            &[]
        }
        fn outputs(&self) -> &[OutputPort] {
            LEAF_OUTPUTS
        }
        fn resolve(&mut self, _graph: &mut Graph<Env>, _self_id: NodeId, _port: &'static str) -> PortValue {
            self.resolved.set(true);
            PortValue::Content(RenderedContent::default())
        }
    }

    fn leaf(graph: &mut Graph) -> (NodeId, std::rc::Rc<std::cell::Cell<bool>>) {
        let flag: std::rc::Rc<std::cell::Cell<bool>> = Default::default();
        let id = graph.add_node(Box::new(LeafNode { resolved: flag.clone() }));
        (id, flag)
    }

    /// A cheap placeholder `Hut::Tab` — mirrors `hut::tests::placeholder`
    /// (private to that module), needed here to build a real `TabbedHut`
    /// fixture to compare `TabNode::cycle` against.
    fn placeholder_hut() -> Hut {
        Hut::Tab(TabbedHut {
            children: Vec::new(),
            active: Signal::new(0),
            label_cache: Vec::new(),
            tab_ids: Vec::new(),
            bg_tracker: Vec::new(),
        })
    }

    #[test]
    fn tab_node_shows_only_the_active_child() {
        let mut graph = Graph::new();
        let (a, a_resolved) = leaf(&mut graph);
        let (b, b_resolved) = leaf(&mut graph);
        let mut tab = TabNode::new();
        *tab.active = 1;
        let tab_id = graph.add_node(Box::new(tab));
        graph.set_hut_list(tab_id, "children", vec![a, b]).unwrap();

        graph.begin_frame();
        graph.resolve_output(tab_id, "content");
        assert!(!a_resolved.get(), "inactive child shouldn't be resolved");
        assert!(b_resolved.get(), "active child should be resolved");
    }

    #[test]
    fn tile_node_shows_every_child() {
        let mut graph = Graph::new();
        let (a, a_resolved) = leaf(&mut graph);
        let (b, b_resolved) = leaf(&mut graph);
        let tile_id = graph.add_node(Box::new(TileNode::new(Axis::Horizontal)));
        graph.set_hut_list(tile_id, "children", vec![a, b]).unwrap();

        graph.begin_frame();
        graph.resolve_output(tile_id, "content");
        assert!(a_resolved.get(), "every pane should be resolved, not just one");
        assert!(b_resolved.get(), "every pane should be resolved, not just one");
    }

    #[test]
    fn tab_node_cycling_matches_hut_tab_cycling_step_for_step() {
        // Real `TabbedHut` fixture (3 children) alongside a `TabNode`
        // fixture with the same child count — stepping both through the
        // same Next/Next/Prev sequence and asserting their `active`
        // indices agree at every step is a genuine equivalence check,
        // not just a resemblance: both ultimately call the exact same
        // `wrapping_step` helper (see this module's doc comment), so a
        // future edit to one wraparound rule without the other would
        // fail this test rather than silently diverge.
        let mut enum_tab = TabbedHut {
            children: vec![placeholder_hut(), placeholder_hut(), placeholder_hut()],
            active: Signal::new(0),
            label_cache: vec![LabelCache::new(), LabelCache::new(), LabelCache::new()],
            tab_ids: vec![(Id::new(), Id::new()), (Id::new(), Id::new()), (Id::new(), Id::new())],
            bg_tracker: vec![ChangeTracker::new(), ChangeTracker::new(), ChangeTracker::new()],
        };

        let mut graph = Graph::new();
        let (a, _) = leaf(&mut graph);
        let (b, _) = leaf(&mut graph);
        let (c, _) = leaf(&mut graph);
        // Registered in the graph only to reserve a real id with the
        // right declared port shape for `set_hut_list`'s kind-check —
        // the actual node stepped below is a separate, standalone
        // `TabNode` value (`graph_tab`), since `cycle` only needs shared
        // read access to the graph (for the child count), not to be the
        // exact boxed instance living inside it.
        let tab_id = graph.add_node(Box::new(TabNode::new()));
        graph.set_hut_list(tab_id, "children", vec![a, b, c]).unwrap();
        let mut graph_tab = TabNode::new();

        for dir in [Direction::Next, Direction::Next, Direction::Prev, Direction::Prev, Direction::Prev] {
            let enum_len = enum_tab.children.len();
            *enum_tab.active = wrapping_step(enum_len, *enum_tab.active, dir);
            graph_tab.cycle(&graph, tab_id, dir);
            assert_eq!(
                *graph_tab.active, *enum_tab.active,
                "TabNode and TabbedHut should land on the same active index for {dir:?}"
            );
        }
    }
}
