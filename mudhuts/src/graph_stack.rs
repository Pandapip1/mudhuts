//! [`GraphStack`] — migration step 4's real replacement for
//! `stack::MruStackHut`: the same MRU-ordered top-level entries,
//! touched/discard rules, preview-session semantics, and "reach the
//! focused leaf, wrap just that" `wrap_tab`/`wrap_tile` behavior, now
//! backed by a real `Graph<RenderEnv>` instead of a `Vec<Hut>` enum tree
//! — plus real per-output state (step 7), per the user's resolved
//! policy: focus follows the mouse across outputs, Alt+Tab cycles within
//! the current output's own stack only, window migration between
//! outputs is deferred (not built here). See
//! `docs/rfcs/typed-graph-hut.md` for the full design and the two real
//! design walls hit building this (`ContentPiece`'s local-vs-absolute
//! position split, `RenderEnv`'s renderer-slot timing).
//!
//! `stack.rs`'s own `Hut`-tree machinery is untouched — this is a
//! parallel, independently-verified implementation, not a rewrite of
//! it, matching the RFC's own "delete the old enum only once every call
//! site has moved" migration rule.

use std::cell::RefCell;
use std::rc::Rc;

use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Channel};
use smithay::utils::{Logical, Point, Rectangle};

use mudhuts_term::TermEvent;

use crate::State;
use crate::console_hut::ConsoleHut;
use crate::graph::{Graph, Node, NodeId};
use crate::graph_nodes::{ConsoleNode, RenderEnv, TabNode, TileNode};
use crate::hut::{Axis, Direction};
use crate::redraw::{Redrawable, RedrawHandle};

/// One physical output's own independent MRU stack — see this module's
/// doc comment on the user's resolved multi-monitor policy. A
/// single-output session (today's only real case) always has exactly
/// one `OutputSlot`.
pub struct OutputSlot {
    pub output: Output,
    /// This output's own position in one shared global compositor
    /// space — real side-by-side layout support (see the RFC's Step 7
    /// scoping section). A single-output session's one slot sits at
    /// `(0, 0)`.
    pub position: Point<i32, Logical>,
    huts: Vec<NodeId>,
    current: usize,
    /// Mirrors `MruStackHut::preview`'s own doc comment exactly — set
    /// while a preview session (the Alt-Tab-style popup) is open.
    preview: Option<usize>,
    panel_id: Id,
}

impl OutputSlot {
    pub fn len(&self) -> usize {
        self.huts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.huts.is_empty()
    }
    pub fn is_previewing(&self) -> bool {
        self.preview.is_some()
    }
    pub fn preview_index(&self) -> usize {
        self.preview.unwrap_or(self.current)
    }
    pub fn panel_id(&self) -> Id {
        self.panel_id.clone()
    }
    /// The raw top-level node id at `current`, unresolved — see
    /// `MruStackHut::focused_top_level`'s identical doc comment.
    pub fn focused_top_level(&self) -> NodeId {
        self.huts[self.current]
    }
    pub fn top_level_entries(&self) -> impl Iterator<Item = &NodeId> {
        self.huts.iter()
    }
}

pub struct GraphStack {
    graph: Graph<RenderEnv>,
    outputs: Vec<OutputSlot>,
    /// Which `outputs` entry currently has input focus — per the user's
    /// resolved policy, follows the mouse across outputs. Updated by
    /// `input.rs`'s pointer-motion handling (`set_focused_output`), not
    /// by this type on its own.
    focused_output: usize,
    loop_handle: LoopHandle<'static, State>,
    extra_env: Vec<(String, String)>,
    /// Shared across every output for now — see the RFC's Step 7 scoping
    /// section: real per-output *scale* (as opposed to per-output
    /// *content*, which this module does support) is a deliberately
    /// deferred simplification.
    scale: f64,
    redraw: RedrawHandle,
}

/// Which shape to wrap the focused leaf into — see [`GraphStack::wrap`].
enum WrapKind {
    Tab,
    Tile(Axis),
}

impl GraphStack {
    /// `first`/`first_events` must come from a single [`ConsoleHut::spawn`]
    /// call using the same `extra_env` given here. `output` is the real
    /// physical output this first (and, at construction time, only)
    /// `OutputSlot` shows on.
    /// `output` starts as a harmless synthetic placeholder (see
    /// `space_element::synthetic_output`'s own doc comment on why one is
    /// always safe to construct with no hardware coupling at all) — like
    /// `RenderEnv`'s renderer slot, `main.rs` constructs the stack before
    /// either backend creates the real `Output`, so there's a real, if
    /// brief, window where none exists yet. Whichever backend creates
    /// the real one calls [`Self::set_output`] once it does.
    pub fn new(
        first: ConsoleHut,
        first_events: Channel<TermEvent>,
        loop_handle: LoopHandle<'static, State>,
        extra_env: Vec<(String, String)>,
        redraw: RedrawHandle,
    ) -> Result<Self, String> {
        let mut graph: Graph<RenderEnv> =
            Graph::with_env(RenderEnv { renderer: None });
        let id = first.id;
        let mut node = ConsoleNode::new(first);
        Redrawable::attach_redraw_handle(&mut node, redraw.clone());
        let node_id = graph.add_node(Box::new(node));

        let stack = Self {
            graph,
            outputs: vec![OutputSlot {
                output: crate::space_element::synthetic_output("pending", (0, 0), 1.0),
                position: Point::from((0, 0)),
                huts: vec![node_id],
                current: 0,
                preview: None,
                panel_id: Id::new(),
            }],
            focused_output: 0,
            loop_handle,
            extra_env,
            scale: 1.0,
            redraw,
        };
        stack.insert_channel(id, first_events)?;
        Ok(stack)
    }

    /// Fills in the real renderer once a backend creates one — see
    /// `graph_nodes::RenderEnv`'s doc comment for why this can't happen
    /// at construction time.
    /// `renderer` must be the *same* `Rc<RefCell<GlesRenderer>>`
    /// allocation a backend already owns (see `RenderEnv::renderer`'s
    /// own doc comment for why) — pass a `.clone()` of it (a cheap `Rc`
    /// clone, not a duplicated GL context), never a freshly-constructed
    /// one.
    pub fn set_renderer(&mut self, renderer: Rc<RefCell<GlesRenderer>>) {
        self.graph.env.renderer = Some(renderer);
    }

    /// Resolve `top`'s own `content` output — the graph-side half of a
    /// render pass. **Must be called before whatever backend render pass
    /// this feeds into acquires its own borrow of the shared renderer**
    /// (see `RenderEnv::renderer`'s doc comment): this internally
    /// borrows the exact same `Rc<RefCell<GlesRenderer>>` a backend's
    /// own render pass also borrows, and `RefCell` panics on a second
    /// concurrent borrow — the two must be sequential, never nested.
    /// Call [`Self::begin_frame`] once per real frame before this, not
    /// once per call (memoization spans the whole frame, not just one
    /// resolve).
    pub fn resolve_content(&mut self, top: NodeId) -> Vec<crate::graph::ContentPiece> {
        match self.graph.resolve_output(top, "content") {
            Some(crate::graph::PortValue::Content(pieces)) => pieces,
            _ => Vec::new(),
        }
    }

    pub fn begin_frame(&mut self) {
        self.graph.begin_frame();
    }

    /// Fills in the real output for slot `index` once a backend creates
    /// one — see [`Self::new`]'s doc comment for why the first slot
    /// starts with a harmless synthetic placeholder instead.
    pub fn set_output(&mut self, index: usize, output: Output) {
        if let Some(slot) = self.outputs.get_mut(index) {
            slot.output = output;
        }
    }

    /// Add a genuinely new output — real multi-monitor (a second
    /// connector) — with its own independent MRU stack, seeded with one
    /// freshly-spawned `ConsoleHut`, per the user's resolved policy
    /// (each output starts as its own independent workspace, not
    /// mirrored). Returns the new `OutputSlot`'s index.
    pub fn add_output(&mut self, output: Output, position: Point<i32, Logical>) -> Result<usize, String> {
        let (hut, events) = ConsoleHut::spawn(self.extra_env.clone(), self.scale)?;
        let id = hut.id;
        let mut node = ConsoleNode::new(hut);
        Redrawable::attach_redraw_handle(&mut node, self.redraw.clone());
        let node_id = self.graph.add_node(Box::new(node));
        self.insert_channel(id, events)?;
        self.outputs.push(OutputSlot {
            output,
            position,
            huts: vec![node_id],
            current: 0,
            preview: None,
            panel_id: Id::new(),
        });
        Ok(self.outputs.len() - 1)
    }

    /// Remove output `index` (a real connector disconnected) — every
    /// node under its own stack is dropped along with it; nothing
    /// migrates to another output (deferred per the user's resolved
    /// policy — see this module's doc comment). Refuses to remove the
    /// last remaining output (mirrors every other "never end up with
    /// zero entries" rule in this codebase).
    pub fn remove_output(&mut self, index: usize) {
        if self.outputs.len() <= 1 || index >= self.outputs.len() {
            return;
        }
        let removed = self.outputs.remove(index);
        for id in removed.huts {
            for node in self.all_ids_under(id) {
                self.graph.remove_node(node);
            }
        }
        if self.focused_output >= self.outputs.len() {
            self.focused_output = self.outputs.len() - 1;
        } else if self.focused_output > index {
            self.focused_output -= 1;
        }
    }

    fn all_ids_under(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = vec![id];
        for child in self.graph.hut_list_input(id, "children") {
            out.extend(self.all_ids_under(child));
        }
        out
    }

    pub fn outputs(&self) -> &[OutputSlot] {
        &self.outputs
    }

    pub fn focused_output_index(&self) -> usize {
        self.focused_output
    }

    /// Per the user's resolved multi-monitor policy: focus follows the
    /// mouse across outputs. Called from `input.rs`'s pointer-motion
    /// handling with whichever output's real geometry now contains the
    /// pointer — a no-op if it's already the focused one.
    pub fn set_focused_output(&mut self, index: usize) {
        if index < self.outputs.len() {
            self.focused_output = index;
        }
    }

    /// Which output's own real, positioned rectangle (`OutputSlot::position`
    /// plus its current mode, scale-divided into Logical) contains `pos` —
    /// per the user's resolved focus-follows-mouse policy. Falls back to
    /// the currently-focused output if `pos` doesn't land inside any real
    /// output's rect (e.g. before any output has a real mode yet, or the
    /// pointer is briefly outside every known rect) — never a bare
    /// `Option`, mirroring every other "there's always a currently-focused
    /// output" invariant in this module.
    pub fn output_index_at(&self, pos: Point<f64, Logical>) -> usize {
        for (i, slot) in self.outputs.iter().enumerate() {
            let Some(mode) = slot.output.current_mode() else {
                continue;
            };
            let scale = slot.output.current_scale().fractional_scale();
            let size = mode.size.to_f64().to_logical(scale);
            let rect = Rectangle::<f64, Logical>::new(slot.position.to_f64(), size);
            if rect.contains(pos) {
                return i;
            }
        }
        self.focused_output
    }

    fn out(&self) -> &OutputSlot {
        &self.outputs[self.focused_output]
    }

    fn out_mut(&mut self) -> &mut OutputSlot {
        &mut self.outputs[self.focused_output]
    }

    /// Real multi-monitor: every method below this point that implicitly
    /// means "the *focused* output" (`focused()`, `focused_top_level()`,
    /// ...) has an explicit `_for(output_index)` counterpart here,
    /// operating on *any* output's own independent stack regardless of
    /// which one currently has input focus — needed because a
    /// backgrounded (unfocused) monitor still needs to render its own
    /// live content every frame, not a stale copy or the focused
    /// monitor's own. `render.rs`'s per-output render pass uses these;
    /// `input.rs`/chrome/docks (all about whatever the user is currently
    /// interacting with) keep using the plain, focused-output versions
    /// unchanged.
    fn out_at(&self, index: usize) -> &OutputSlot {
        &self.outputs[index]
    }

    pub fn len(&self) -> usize {
        self.out().len()
    }

    pub fn len_for(&self, output_index: usize) -> usize {
        self.out_at(output_index).len()
    }

    pub fn is_empty(&self) -> bool {
        self.out().is_empty()
    }

    pub fn is_previewing(&self) -> bool {
        self.out().is_previewing()
    }

    pub fn is_previewing_for(&self, output_index: usize) -> bool {
        self.out_at(output_index).is_previewing()
    }

    pub fn preview_index(&self) -> usize {
        self.out().preview_index()
    }

    pub fn preview_index_for(&self, output_index: usize) -> usize {
        self.out_at(output_index).preview_index()
    }

    pub fn panel_id(&self) -> Id {
        self.out().panel_id()
    }

    pub fn panel_id_for(&self, output_index: usize) -> Id {
        self.out_at(output_index).panel_id()
    }

    pub fn focused_top_level(&self) -> NodeId {
        self.out().focused_top_level()
    }

    pub fn focused_top_level_for(&self, output_index: usize) -> NodeId {
        self.out_at(output_index).focused_top_level()
    }

    pub fn top_level_entries(&self) -> impl Iterator<Item = &NodeId> {
        self.out().top_level_entries()
    }

    pub fn top_level_entries_for(&self, output_index: usize) -> impl Iterator<Item = &NodeId> {
        self.out_at(output_index).top_level_entries()
    }

    /// Every top-level entry across *every* output — for searches that
    /// need to reach a backgrounded output's own entries too (mirrors
    /// `MruStackHut::all_huts`'s "not just what's currently visible"
    /// scope, generalized across outputs).
    pub fn all_top_level_entries(&self) -> impl Iterator<Item = &NodeId> {
        self.outputs.iter().flat_map(|o| o.huts.iter())
    }

    /// Whether `top`'s own terminal (vs. its focused ConsoleHut's active
    /// Main Window) is what's currently effectively shown — mirrors
    /// `Hut::shows_terminal_effective` exactly.
    pub fn shows_terminal_effective(&self, top: NodeId) -> bool {
        if self.graph.downcast::<TileNode>(top).is_some() && self.graph.hut_list_input(top, "children").len() >= 2 {
            return true;
        }
        let leaf = self.graph.focused_leaf(top);
        let Some(console) = self.graph.downcast::<ConsoleNode>(leaf) else {
            return true;
        };
        *console.hut.showing_terminal || console.hut.main_window_count() == 0
    }

    /// `root`'s absolute physical-pixel rect right now, if it's a Main
    /// Window currently on screen under `top` — mirrors
    /// `Hut::leaf_absolute_rect` exactly (same real-output-absolute
    /// coordinates convention, computed via the same `hut::pane_rects`
    /// call `TileNode`'s own `resolve`/`resize_to_pixels` already use,
    /// so this can never disagree with what's actually rendered/sized).
    pub fn leaf_absolute_rect(
        &self,
        top: NodeId,
        root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        area: (i32, i32, i32, i32),
    ) -> Option<(i32, i32, i32, i32)> {
        if let Some(console) = self.graph.downcast::<ConsoleNode>(top) {
            return console.hut.main_windows().iter().any(|e| e.matches(root)).then_some(area);
        }
        if let Some(tab) = self.graph.downcast::<TabNode>(top) {
            let children = self.graph.hut_list_input(top, "children");
            let next = *children.get(*tab.active)?;
            return self.leaf_absolute_rect(next, root, area);
        }
        if let Some(tile) = self.graph.downcast::<TileNode>(top) {
            let children = self.graph.hut_list_input(top, "children");
            let fracs = if tile.fracs.len() == children.len() { tile.fracs.clone() } else { vec![1.0; children.len()] };
            let rects: Vec<_> = crate::hut::pane_rects(tile.axis, fracs.into_iter(), (area.2, area.3))
                .into_iter()
                .map(|(x, y, w, h)| (x + area.0, y + area.1, w, h))
                .collect();
            for (&child, rect) in children.iter().zip(rects) {
                let leaf = self.graph.focused_leaf(child);
                if let Some(console) = self.graph.downcast::<ConsoleNode>(leaf)
                    && console.hut.main_windows().iter().any(|e| e.matches(root))
                {
                    return Some(rect);
                }
            }
            return None;
        }
        None
    }

    /// This top-level entry's active pane's own physical-pixel offset
    /// from `area`'s origin — mirrors `TileHut::absolute_pane_rects`'s
    /// per-pane offset for the focused pane, generalized to "no Tile at
    /// all" (just `area`'s own origin unchanged).
    pub fn active_pane_offset(&self, top: NodeId, area: (i32, i32, i32, i32)) -> (f64, f64) {
        let Some(tile) = self.graph.downcast::<TileNode>(top) else {
            return (area.0 as f64, area.1 as f64);
        };
        let children = self.graph.hut_list_input(top, "children");
        if children.len() < 2 {
            return (area.0 as f64, area.1 as f64);
        }
        let fracs = if tile.fracs.len() == children.len() { tile.fracs.clone() } else { vec![1.0; children.len()] };
        let rects = crate::hut::pane_rects(tile.axis, fracs.into_iter(), (area.2, area.3));
        let (x, y, _, _) = rects[(*tile.active).min(rects.len().saturating_sub(1))];
        ((area.0 + x) as f64, (area.1 + y) as f64)
    }

    pub fn graph(&self) -> &Graph<RenderEnv> {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut Graph<RenderEnv> {
        &mut self.graph
    }

    /// The `ConsoleNode` currently holding effective focus — see
    /// `MruStackHut::focused`'s identical doc comment. `.expect()`s
    /// that the focused leaf really is a `ConsoleNode`: unlike most of
    /// this codebase (see the standing "no panics in compositor code"
    /// guidance), there's no meaningful fallback `&ConsoleHut` to
    /// degrade to, and this is a genuine internal invariant of
    /// `GraphStack`'s own construction (every leaf `wrap_tab`/`wrap_tile`
    /// can ever reach is a `ConsoleNode` — nothing else has no
    /// `focused_child`), not an external condition that can fail on its
    /// own — the same class of provably-safe `.expect()` `udev_backend.rs`'s
    /// own `drm_lease_state` already uses, for the same reason.
    pub fn focused(&self) -> &ConsoleHut {
        let leaf = self.graph.focused_leaf(self.focused_top_level());
        &self
            .graph
            .downcast::<ConsoleNode>(leaf)
            .expect("a focused leaf with no focused_child of its own must be a ConsoleNode")
            .hut
    }

    pub fn focused_mut(&mut self) -> &mut ConsoleHut {
        let leaf = self.graph.focused_leaf(self.focused_top_level());
        &mut self
            .graph
            .downcast_mut::<ConsoleNode>(leaf)
            .expect("a focused leaf with no focused_child of its own must be a ConsoleNode")
            .hut
    }

    /// [`Self::focused`], for a specific output rather than whichever one
    /// currently has input focus — see the "real multi-monitor" doc
    /// comment on [`Self::out_at`].
    pub fn focused_for(&self, output_index: usize) -> &ConsoleHut {
        let leaf = self.graph.focused_leaf(self.focused_top_level_for(output_index));
        &self
            .graph
            .downcast::<ConsoleNode>(leaf)
            .expect("a focused leaf with no focused_child of its own must be a ConsoleNode")
            .hut
    }

    pub fn focused_mut_for(&mut self, output_index: usize) -> &mut ConsoleHut {
        let leaf = self.graph.focused_leaf(self.focused_top_level_for(output_index));
        &mut self
            .graph
            .downcast_mut::<ConsoleNode>(leaf)
            .expect("a focused leaf with no focused_child of its own must be a ConsoleNode")
            .hut
    }

    /// Find a `ConsoleHut` by id anywhere in the whole graph, across
    /// every output — mirrors `MruStackHut::find_mut`.
    pub fn find_mut(&mut self, id: u64) -> Option<&mut ConsoleHut> {
        // No generic "every node id" iterator on `Graph` (nothing else
        // has needed one) — walking every output's own top-level entries
        // down through every reachable child is the same reach
        // `MruStackHut::all_huts`/`find_mut` already have, just phrased
        // against the graph's own reference tables instead of a `Hut`
        // tree's owned `Vec`s.
        let node_id = self
            .all_node_ids()
            .into_iter()
            .find(|&node_id| self.graph.downcast::<ConsoleNode>(node_id).is_some_and(|c| c.hut.id == id))?;
        Some(&mut self.graph.downcast_mut::<ConsoleNode>(node_id)?.hut)
    }

    /// Every `ConsoleHut` anywhere in the graph, across every output —
    /// mirrors `MruStackHut::all_huts`.
    pub fn all_huts(&self) -> impl Iterator<Item = &ConsoleHut> {
        self.all_node_ids()
            .into_iter()
            .filter_map(|id| self.graph.downcast::<ConsoleNode>(id))
            .map(|n| &n.hut)
    }

    pub fn all_huts_mut(&mut self) -> impl Iterator<Item = &mut ConsoleHut> {
        self.graph
            .nodes_mut()
            .filter_map(|n| n.as_any_mut().downcast_mut::<ConsoleNode>())
            .map(|n| &mut n.hut)
    }

    /// One `ConsoleHut` per top-level entry across every output —
    /// whichever is currently active/visible within that entry, if it's
    /// a Tab/Tile node. Mirrors `MruStackHut::top_level_huts`.
    pub fn top_level_huts(&self) -> impl Iterator<Item = &ConsoleHut> + '_ {
        self.all_top_level_entries()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|id| self.graph.downcast::<ConsoleNode>(self.graph.focused_leaf(id)))
            .map(|n| &n.hut)
    }

    /// Every node id reachable from any output's top-level entries —
    /// the graph-native walk `MruStackHut::all_huts`'s `Hut::all_huts`
    /// recursion used to do directly on owned `Vec<Hut>` structure, done
    /// here against `hut_list_input` links instead.
    fn all_node_ids(&self) -> Vec<NodeId> {
        fn walk(graph: &Graph<RenderEnv>, id: NodeId, out: &mut Vec<NodeId>) {
            out.push(id);
            for child in graph.hut_list_input(id, "children") {
                walk(graph, child, out);
            }
        }
        let mut out = Vec::new();
        for &top in self.all_top_level_entries().collect::<Vec<_>>() {
            walk(&self.graph, top, &mut out);
        }
        out
    }

    /// Every `ConsoleHut` anywhere in the graph needs to track the real
    /// output size even while not focused/visible — mirrors
    /// `MruStackHut::resize_all`. `width`/`height` apply to *every*
    /// output's own top-level entries — real distinct per-output sizes
    /// are a real backend's own concern (each output's actual mode),
    /// not this method's.
    pub fn resize_all(&mut self, width: i32, height: i32) {
        let tops: Vec<NodeId> = self.all_top_level_entries().copied().collect();
        for id in tops {
            self.graph.with_node_mut(id, |node, graph| node.resize_to_pixels(graph, id, width, height));
        }
    }

    /// Real multi-monitor: resizes only `output_index`'s own top-level
    /// entries, for the case `resize_all`'s own doc comment flags as out
    /// of its scope — two outputs with genuinely different real modes.
    pub fn resize_output(&mut self, output_index: usize, width: i32, height: i32) {
        let tops: Vec<NodeId> = self.top_level_entries_for(output_index).copied().collect();
        for id in tops {
            self.graph.with_node_mut(id, |node, graph| node.resize_to_pixels(graph, id, width, height));
        }
    }

    /// Catch every already-spawned `ConsoleHut` up to the real output
    /// scale — mirrors `MruStackHut::rescale_all`.
    pub fn rescale_all(&mut self, scale: f64) -> Result<(), String> {
        self.scale = scale;
        for id in self.all_node_ids() {
            if let Some(console) = self.graph.downcast_mut::<ConsoleNode>(id) {
                console.hut.rescale(scale)?;
            }
        }
        Ok(())
    }

    /// Meta+Left/Right's bubble-up step — mirrors
    /// `MruStackHut::cycle_innermost`/`Hut::cycle_innermost`'s "innermost
    /// first" recursion: walks the focused path from the top-level entry
    /// down, cycling the *deepest* Tab/Tile level that actually has 2+
    /// children, not the shallowest.
    pub fn cycle_innermost(&mut self, dir: Direction) -> bool {
        let path = self.graph.focused_path(self.focused_top_level());
        // Innermost first: try every Tab/Tile level starting from the
        // one closest to the leaf, same order `Hut::cycle_innermost`'s
        // own recursion visits them in (it recurses into the active
        // child *before* trying to cycle its own level).
        for &id in path.iter().rev() {
            let cycled = self.graph.with_node_mut(id, |node, graph| {
                if let Some(tab) = node.as_any_mut().downcast_mut::<TabNode>() {
                    return tab.cycle(graph, id, dir);
                }
                if let Some(tile) = node.as_any_mut().downcast_mut::<TileNode>() {
                    return tile.cycle(graph, id, dir);
                }
                false
            });
            if cycled == Some(true) {
                self.redraw.mark_dirty();
                return true;
            }
        }
        false
    }

    /// Spawn a fresh `ConsoleHut` and wrap it together with whatever's
    /// currently focused into a new Tab node, *in place* — mirrors
    /// `MruStackHut::wrap_tab`/`Hut::wrap_focused`'s doc comment on why
    /// this always reaches all the way down to the actual focused leaf
    /// (whatever container it's already inside, if any) rather than
    /// operating on top-level entries: wrapping a specific pane of an
    /// existing Tile node needs to replace *just that pane*, leaving the
    /// Tile node itself and every other pane completely untouched.
    pub fn wrap_tab(&mut self) -> Result<(), String> {
        self.wrap(WrapKind::Tab)
    }

    /// Same as [`Self::wrap_tab`], but into a new (horizontally split)
    /// Tile node instead.
    pub fn wrap_tile(&mut self) -> Result<(), String> {
        self.wrap(WrapKind::Tile(Axis::Horizontal))
    }

    fn wrap(&mut self, kind: WrapKind) -> Result<(), String> {
        self.commit_preview();
        let (hut, events) = ConsoleHut::spawn(self.extra_env.clone(), self.scale)?;
        let id = hut.id;
        let mut new_console = ConsoleNode::new(hut);
        Redrawable::attach_redraw_handle(&mut new_console, self.redraw.clone());
        let new_id = self.graph.add_node(Box::new(new_console));

        let top_id = self.focused_top_level();
        let path = self.graph.focused_path(top_id);
        // Always has >= 1 entry (`top_id` itself) — see
        // `Graph::focused_path`'s own doc comment.
        let focused_leaf = *path.last().expect("focused_path always returns at least one entry");

        // `other` (freshly spawned) first, `current` (the pre-existing
        // focused leaf) second — matches `Hut::wrap_tab`/`wrap_tile`'s
        // exact ordering ("current kept last, so the wrap is visually a
        // no-op") and `active`/`fracs` below accordingly.
        let wrapped_id = match kind {
            WrapKind::Tab => {
                let mut tab = TabNode::new();
                *tab.active = 1;
                Redrawable::attach_redraw_handle(&mut tab, self.redraw.clone());
                let wrapped_id = self.graph.add_node(Box::new(tab));
                self.graph
                    .set_hut_list(wrapped_id, "children", vec![new_id, focused_leaf])
                    .map_err(|err| format!("{err:?}"))?;
                wrapped_id
            }
            WrapKind::Tile(axis) => {
                let mut tile = TileNode::new(axis);
                tile.fracs = vec![0.5, 0.5];
                *tile.active = 1;
                Redrawable::attach_redraw_handle(&mut tile, self.redraw.clone());
                let wrapped_id = self.graph.add_node(Box::new(tile));
                self.graph
                    .set_hut_list(wrapped_id, "children", vec![new_id, focused_leaf])
                    .map_err(|err| format!("{err:?}"))?;
                wrapped_id
            }
        };

        if path.len() == 1 {
            // The top-level entry itself *is* the focused leaf — replace
            // that top-level slot directly.
            let current = self.out().current;
            self.out_mut().huts[current] = wrapped_id;
        } else {
            // Repoint whichever ancestor's own `children` list currently
            // references the focused leaf — the second-to-last entry in
            // the path — at the new wrapper instead. Replacing *in
            // place* at the same list index means the parent's own
            // `active` index (still pointing at that position) stays
            // correct automatically, no adjustment needed.
            let parent = path[path.len() - 2];
            let mut children = self.graph.hut_list_input(parent, "children");
            if let Some(pos) = children.iter().position(|&c| c == focused_leaf) {
                children[pos] = wrapped_id;
            }
            self.graph.set_hut_list(parent, "children", children).map_err(|err| format!("{err:?}"))?;
        }

        self.redraw.mark_dirty();
        self.insert_channel(id, events)
    }

    fn insert_channel(&self, id: u64, events: Channel<TermEvent>) -> Result<(), String> {
        self.loop_handle
            .insert_source(events, move |event, _, state| {
                if let channel::Event::Msg(event) = event {
                    state.handle_term_event(id, event);
                }
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Spawns a genuinely new background `ConsoleHut` and appends it to
    /// the end of the *focused output's* own stack, without touching
    /// `current` — mirrors `MruStackHut::spawn_and_insert`.
    pub fn spawn_and_insert(&mut self) -> Result<u64, String> {
        let (hut, events) = ConsoleHut::spawn(self.extra_env.clone(), self.scale)?;
        let id = hut.id;
        self.insert_channel(id, events)?;
        let mut node = ConsoleNode::new(hut);
        Redrawable::attach_redraw_handle(&mut node, self.redraw.clone());
        let node_id = self.graph.add_node(Box::new(node));
        self.out_mut().huts.push(node_id);
        Ok(id)
    }

    /// Move `pos` forward by one step, applying the discard/spawn rules
    /// — mirrors `MruStackHut::advance_forward` exactly, just against
    /// `NodeId`s/`Graph::node`'s `touched()` instead of `Hut::touched()`.
    fn advance_forward(&mut self, pos: &mut usize, protect: Option<usize>) -> Result<(), String> {
        if self.out().is_empty() {
            self.spawn_and_insert()?;
            *pos = 0;
            return Ok(());
        }
        let id = self.out().huts[*pos];
        let keep = protect == Some(*pos) || self.graph.node(id).map(Node::touched).unwrap_or(true);
        if keep {
            *pos += 1;
        } else {
            self.graph.remove_node(id);
            self.out_mut().huts.remove(*pos);
        }
        if *pos >= self.out().len() {
            self.spawn_and_insert()?;
        }
        Ok(())
    }

    fn advance_backward(&mut self, pos: &mut usize, protect: Option<usize>) {
        if *pos == 0 || self.out().is_empty() {
            return;
        }
        let id = self.out().huts[*pos];
        let keep = protect == Some(*pos) || self.graph.node(id).map(Node::touched).unwrap_or(true);
        if !keep {
            self.graph.remove_node(id);
            self.out_mut().huts.remove(*pos);
        }
        *pos -= 1;
    }

    /// Alt+Tab, instant-commit fallback — mirrors `MruStackHut::next`.
    pub fn next(&mut self) -> Result<(), String> {
        let mut pos = self.out().current;
        self.advance_forward(&mut pos, None)?;
        self.out_mut().current = pos;
        self.redraw.mark_dirty();
        Ok(())
    }

    pub fn prev(&mut self) {
        let mut pos = self.out().current;
        self.advance_backward(&mut pos, None);
        self.out_mut().current = pos;
        self.redraw.mark_dirty();
    }

    pub fn preview_next(&mut self) -> Result<(), String> {
        let current = self.out().current;
        let mut pos = self.out().preview.unwrap_or(current);
        self.advance_forward(&mut pos, Some(current))?;
        self.out_mut().preview = Some(pos);
        self.redraw.mark_dirty();
        Ok(())
    }

    pub fn preview_prev(&mut self) {
        let current = self.out().current;
        let pos = match self.out().preview {
            Some(mut pos) => {
                self.advance_backward(&mut pos, Some(current));
                pos
            }
            None => self.out().len().saturating_sub(1),
        };
        self.out_mut().preview = Some(pos);
        self.redraw.mark_dirty();
    }

    /// Commit the open preview session — mirrors
    /// `MruStackHut::commit_preview`.
    pub fn commit_preview(&mut self) {
        let Some(pos) = self.out_mut().preview.take() else {
            return;
        };
        let out = self.out_mut();
        if pos < out.huts.len() {
            let id = out.huts.remove(pos);
            if let Some(node) = self.graph.node_mut(id) {
                node.mark_touched();
            }
            self.out_mut().huts.insert(0, id);
        }
        self.out_mut().current = 0;
        self.redraw.mark_dirty();
    }

    /// A `ConsoleHut`'s shell exited — mirrors
    /// `MruStackHut::remove_exited` exactly, including the "collapse a
    /// Tab/Tile node left with one child" rule.
    pub fn remove_exited(&mut self, id: u64) -> Result<(), String> {
        let Some(node_id) = self.all_node_ids().into_iter().find(|&node_id| {
            self.graph.downcast::<ConsoleNode>(node_id).is_some_and(|c| c.hut.id == id)
        }) else {
            return Ok(());
        };

        let output_index = self.focused_output;
        if let Some(top_index) = self.outputs[output_index].huts.iter().position(|&h| h == node_id) {
            // A bare top-level entry — drop it outright.
            self.graph.remove_node(node_id);
            let out = &mut self.outputs[output_index];
            out.huts.remove(top_index);
            if top_index < out.current {
                out.current -= 1;
            }
            if let Some(preview) = &mut out.preview
                && top_index < *preview
            {
                *preview -= 1;
            }
        } else {
            // Nested inside some top-level entry's own Tab/Tile chain —
            // remove it from whichever node's `children` list references
            // it, collapsing that node back to a bare child if only one
            // survives (mirrors `Hut::remove_child_hut`).
            self.remove_child(node_id);
            self.graph.remove_node(node_id);
        }

        if self.out().is_empty() {
            self.spawn_and_insert()?;
        }
        let max_index = self.out().len().saturating_sub(1);
        let out = self.out_mut();
        out.current = out.current.min(max_index);
        if let Some(preview) = &mut out.preview {
            *preview = (*preview).min(max_index);
        }
        Ok(())
    }

    /// Remove `target` from whichever node's own `children` list
    /// currently references it, anywhere in the graph — mirrors
    /// `Hut::remove_child_hut`'s recursive search, phrased against
    /// `hut_list_input`/`set_hut_list` instead of an owned `Vec<Hut>`.
    /// Collapses the parent to its one surviving child if that leaves
    /// exactly one.
    fn remove_child(&mut self, target: NodeId) {
        for parent in self.all_node_ids() {
            let mut children = self.graph.hut_list_input(parent, "children");
            let before = children.len();
            children.retain(|&c| c != target);
            if children.len() == before {
                continue;
            }
            if children.len() == 1 {
                // Collapse: whatever pointed at `parent` should now
                // point at `children[0]` directly instead — same
                // "an emptied-out wrapper around one survivor is never
                // useful to keep" rule `Hut::collapse_if_singleton`
                // already applies.
                let survivor = children[0];
                self.repoint(parent, survivor);
                self.graph.remove_node(parent);
            } else {
                let _ = self.graph.set_hut_list(parent, "children", children);
            }
            return;
        }
    }

    /// Replace every reference to `old` (a top-level slot, or an entry
    /// in some other node's `children` list) with `new` — the
    /// "collapse" half of [`Self::remove_child`].
    fn repoint(&mut self, old: NodeId, new: NodeId) {
        for out in &mut self.outputs {
            for hut in &mut out.huts {
                if *hut == old {
                    *hut = new;
                }
            }
        }
        for parent in self.all_node_ids() {
            let mut children = self.graph.hut_list_input(parent, "children");
            let mut changed = false;
            for child in &mut children {
                if *child == old {
                    *child = new;
                    changed = true;
                }
            }
            if changed {
                let _ = self.graph.set_hut_list(parent, "children", children);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use smithay::reexports::calloop::EventLoop;

    use super::*;
    use crate::console_hut::ConsoleHut;

    /// Real `LoopHandle` (from a real, never-run `EventLoop`) — mirrors
    /// `stack.rs`'s own test helper exactly.
    fn loop_handle() -> LoopHandle<'static, State> {
        let event_loop: EventLoop<'static, State> = EventLoop::try_new().unwrap();
        Box::leak(Box::new(event_loop)).handle()
    }

    fn new_stack() -> GraphStack {
        let (hut, events) = ConsoleHut::spawn(std::iter::empty(), 1.0).unwrap();
        let (ping, _source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        GraphStack::new(hut, events, loop_handle(), Vec::new(), RedrawHandle::new(ping)).unwrap()
    }

    #[test]
    fn starts_with_a_single_focused_untouched_hut() {
        let stack = new_stack();
        assert_eq!(stack.len(), 1);
        assert!(!stack.focused().touched());
    }

    #[test]
    fn next_past_an_untouched_tail_replaces_it_rather_than_growing() {
        let mut stack = new_stack();
        let original_id = stack.focused().id;
        stack.next().unwrap();
        assert_eq!(stack.len(), 1, "untouched ConsoleHut should be replaced, not kept alongside a new one");
        assert_ne!(stack.focused().id, original_id, "should be a fresh ConsoleHut");
    }

    #[test]
    fn next_past_a_touched_tail_grows_the_stack() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.focused_mut().mark_touched();
        stack.next().unwrap();
        assert_eq!(stack.len(), 2);
        assert_ne!(stack.focused().id, first_id, "should have moved on to a new ConsoleHut");
        assert!(!stack.focused().touched());
    }

    #[test]
    fn prev_discards_an_untouched_hut_left_behind() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.focused_mut().mark_touched();
        stack.next().unwrap();
        stack.prev();
        assert_eq!(stack.len(), 1, "the never-touched 2nd ConsoleHut should be discarded, not kept");
        assert_eq!(stack.focused().id, first_id);
    }

    #[test]
    fn preview_session_never_discards_the_untouched_but_currently_focused_hut() {
        let mut stack = new_stack();
        assert!(!stack.focused().touched());
        let first_id = stack.focused().id;
        stack.preview_next().unwrap();
        assert_eq!(stack.focused().id, first_id, "current survives untouched");
        stack.preview_prev();
        assert_eq!(stack.preview_index(), 0);
        assert_eq!(stack.len(), 1, "current wasn't discarded by landing back on it");
        assert_eq!(stack.focused().id, first_id);
    }

    #[test]
    fn commit_preview_moves_the_selection_to_the_front_and_marks_it_touched() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.preview_next().unwrap(); // spawns #2, previews it (untouched)

        stack.commit_preview();

        assert!(!stack.is_previewing());
        assert_ne!(stack.focused().id, first_id, "committed selection becomes focused");
        assert!(stack.focused().touched(), "committing counts as using it");
        assert_eq!(stack.len(), 2, "the ConsoleHut left behind (first_id) is kept, not discarded");
    }

    #[test]
    fn wrap_tab_spawns_a_new_hut_and_wraps_the_focused_one_in_place() {
        let mut stack = new_stack();
        let focused_id = stack.focused().id;
        assert_eq!(stack.len(), 1);

        stack.wrap_tab().unwrap();

        assert_eq!(stack.len(), 1, "wrapping the only entry doesn't add a 2nd top-level entry");
        assert_eq!(
            stack.focused().id,
            focused_id,
            "wrapping shouldn't change what's on screen — the pre-existing ConsoleHut stays active"
        );
        assert!(
            stack.graph().downcast::<TabNode>(stack.focused_top_level()).is_some(),
            "the wrapped entry is a Tab node"
        );

        stack.cycle_innermost(Direction::Prev);
        assert_ne!(stack.focused().id, focused_id, "cycling should reach the freshly spawned ConsoleHut");
    }

    #[test]
    fn wrap_tile_spawns_a_new_hut_and_wraps_the_focused_one_in_place() {
        let mut stack = new_stack();
        let focused_id = stack.focused().id;

        stack.wrap_tile().unwrap();

        assert_eq!(stack.len(), 1);
        assert_eq!(stack.focused().id, focused_id);
        assert!(stack.graph().downcast::<TileNode>(stack.focused_top_level()).is_some());
    }

    #[test]
    fn wrap_focused_only_touches_the_specific_pane_its_called_on() {
        // Regression case (mirrors stack.rs's own identically-named
        // test): wrapping a Tab node "inside" one pane of an existing
        // Tile node must replace *that pane's* content only, leaving the
        // Tile node and its other pane completely untouched.
        let mut stack = new_stack();
        stack.wrap_tile().unwrap(); // Tile[new_hut, orig], active = 1 (orig — unchanged focus)
        let tile_id = stack.focused_top_level();
        let other_pane_id = stack.graph().hut_list_input(tile_id, "children")[0];
        let active_pane_id = stack.focused().id;

        stack.wrap_tab().unwrap();

        assert_eq!(stack.len(), 1, "still just the one Tile node overall");
        let tile_id = stack.focused_top_level();
        assert!(stack.graph().downcast::<TileNode>(tile_id).is_some(), "still a Tile node");
        let children = stack.graph().hut_list_input(tile_id, "children");
        assert_eq!(children.len(), 2, "the other pane wasn't touched");
        assert_eq!(children[0], other_pane_id, "the untouched pane is still exactly the same node");
        assert!(
            stack.graph().downcast::<TabNode>(children[1]).is_some(),
            "only the active pane became a Tab node"
        );
        assert_eq!(
            stack.focused().id,
            active_pane_id,
            "wrapping the active pane in place doesn't change what's on screen"
        );
    }

    #[test]
    fn cycle_innermost_bubbles_up_and_wraps() {
        let mut stack = new_stack();
        let original_id = stack.focused().id;
        stack.wrap_tab().unwrap();
        assert_eq!(stack.focused().id, original_id, "wrap doesn't change what's focused");

        assert!(stack.cycle_innermost(Direction::Next), "moves to the freshly spawned ConsoleHut");
        let new_id = stack.focused().id;
        assert_ne!(new_id, original_id);

        assert!(stack.cycle_innermost(Direction::Next), "wraps back to the original");
        assert_eq!(stack.focused().id, original_id);
    }

    #[test]
    fn cycle_innermost_is_a_no_op_for_a_lone_hut() {
        let mut stack = new_stack();
        assert!(!stack.cycle_innermost(Direction::Next));
        assert!(!stack.cycle_innermost(Direction::Prev));
    }

    #[test]
    fn remove_exited_respawns_when_it_was_the_only_hut() {
        let mut stack = new_stack();
        let original_id = stack.focused().id;
        stack.remove_exited(original_id).unwrap();
        assert_eq!(stack.len(), 1, "should never end up with zero entries");
        assert_ne!(stack.focused().id, original_id);
    }

    #[test]
    fn remove_exited_collapses_a_tab_node_left_with_one_child() {
        let mut stack = new_stack();
        stack.wrap_tab().unwrap(); // Tab[original, new], active = 1 (new)
        let new_id = stack.focused().id;

        stack.remove_exited(new_id).unwrap();

        assert_eq!(stack.len(), 1, "still one top-level entry");
        let top = stack.focused_top_level();
        assert!(
            stack.graph().downcast::<ConsoleNode>(top).is_some(),
            "collapsed back down to a bare ConsoleNode instead of a 1-child Tab node"
        );
    }

    #[test]
    fn remove_exited_is_a_no_op_for_an_unknown_id() {
        let mut stack = new_stack();
        let id = stack.focused().id;
        stack.remove_exited(999_999).unwrap();
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.focused().id, id);
    }

    #[test]
    fn spawn_and_insert_appends_a_background_hut_without_changing_focus() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        let new_id = stack.spawn_and_insert().unwrap();
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.focused().id, first_id, "focus shouldn't move to the new background entry");
        assert!(stack.all_huts().any(|h| h.id == new_id));
    }

    #[test]
    fn find_mut_reaches_a_hut_nested_inside_a_tab_node() {
        let mut stack = new_stack();
        stack.wrap_tab().unwrap();
        let nested_id = stack.top_level_huts().next().map(|h| h.id);
        // `top_level_huts` resolves through `focused_leaf`, so it should
        // find the currently-active pane's own id — either way, walk
        // `all_huts` to get a real nested id to search for.
        let some_id = stack.all_huts().map(|h| h.id).find(|&id| Some(id) != nested_id).unwrap();
        assert!(stack.find_mut(some_id).is_some());
        assert!(stack.find_mut(999_999).is_none());
    }

    #[test]
    fn add_output_creates_an_independent_stack_that_does_not_disturb_the_first() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        let second_output = crate::space_element::synthetic_output("second", (800, 600), 1.0);
        let index = stack.add_output(second_output, Point::from((1920, 0))).unwrap();
        assert_eq!(index, 1);
        assert_eq!(stack.outputs().len(), 2);
        // Focus hasn't moved — `add_output` only ever adds a slot, never
        // reassigns `focused_output` on its own (that's `set_focused_output`'s
        // job, driven by real pointer motion).
        assert_eq!(stack.focused_output_index(), 0);
        assert_eq!(stack.focused().id, first_id);
        assert_ne!(stack.focused_for(1).id, first_id, "the new output's own ConsoleHut should be a fresh one");
        assert_eq!(stack.len_for(0), 1);
        assert_eq!(stack.len_for(1), 1);
    }

    #[test]
    fn output_index_at_finds_the_output_whose_positioned_rect_contains_the_point() {
        let mut stack = new_stack();
        // Slot 0's placeholder starts at size (0, 0) — give it a real mode
        // so it has a real rect to test against, mirroring what
        // `udev_backend.rs::connector_connected` does for a real first
        // connector.
        stack.set_output(0, crate::space_element::synthetic_output("first", (1920, 1080), 1.0));
        let second_output = crate::space_element::synthetic_output("second", (800, 600), 1.0);
        stack.add_output(second_output, Point::from((1920, 0))).unwrap();

        assert_eq!(stack.output_index_at(Point::from((100.0, 100.0))), 0);
        assert_eq!(stack.output_index_at(Point::from((2000.0, 100.0))), 1);
        // Outside every known rect — falls back to whichever is currently
        // focused, never panics/returns an invalid index.
        assert_eq!(stack.output_index_at(Point::from((-50.0, -50.0))), stack.focused_output_index());
    }

    #[test]
    fn remove_output_drops_its_own_huts_and_shifts_later_indices_down() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.add_output(crate::space_element::synthetic_output("b", (800, 600), 1.0), Point::from((1920, 0))).unwrap();
        stack.add_output(crate::space_element::synthetic_output("c", (800, 600), 1.0), Point::from((2720, 0))).unwrap();
        assert_eq!(stack.outputs().len(), 3);

        stack.remove_output(1);
        assert_eq!(stack.outputs().len(), 2, "should have dropped exactly one slot");
        assert_eq!(stack.focused().id, first_id, "removing a non-focused output shouldn't disturb focus");
        // What used to be index 2 ("c") should now be at index 1.
        assert_eq!(stack.outputs()[1].output.name(), "c");
    }

    #[test]
    fn remove_output_refuses_to_drop_the_last_remaining_slot() {
        let mut stack = new_stack();
        stack.remove_output(0);
        assert_eq!(stack.outputs().len(), 1, "the last slot must never be removed");
    }
}
