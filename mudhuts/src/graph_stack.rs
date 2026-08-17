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
use crate::graph_nodes::{ConsoleNode, RenderEnv, TabNode, TileNode, fracs_for, is_effectively_tiled};
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
    /// Whether `State::sync_keyboard_focus_to_view` still needs to run
    /// for the current frame — reset alongside `graph.begin_frame`'s own
    /// per-frame resolve cache by [`Self::begin_frame`]. That method
    /// scans every output's own layer map looking for the current
    /// keyboard focus, and (per `render.rs::build_frame_elements`'s own
    /// doc comment) has to be called once for every output that renders
    /// during a real frame, not deduplicated to a fixed output index —
    /// but a real multi-monitor frame typically renders every output
    /// together in one synchronous pass, so without this gate the same
    /// full scan ran once per output, making total per-frame cost
    /// O(outputs^2) instead of O(outputs) (caught in review). One run
    /// per frame already satisfies that invariant, since every output
    /// rendered this frame does so after the same `begin_frame` call.
    ///
    /// A [`FrameGate`], not a bare `bool` folded into `graph.rs`'s own
    /// `Graph::cache` (a keyed per-`(NodeId, port)` *resolve-value*
    /// memoization, a genuinely different kind of thing than "has this
    /// one-shot side effect already run this frame") — but still a
    /// small, named, reusable primitive rather than an ad hoc flag, so
    /// a future second "run at most once per real frame" need (this is
    /// currently the only one) has an obvious precedent to reuse instead
    /// of inventing its own (caught in review).
    keyboard_focus_synced: FrameGate,
}

/// A one-shot "has this already run for the current frame" gate, reset
/// by [`GraphStack::begin_frame`] — see `keyboard_focus_synced`'s own
/// doc comment for why this exists as a small named type rather than a
/// bare `bool`.
#[derive(Default)]
struct FrameGate(bool);

impl FrameGate {
    fn reset(&mut self) {
        self.0 = false;
    }

    /// `true` (and marks it done) only the first call since the last
    /// [`Self::reset`].
    fn take(&mut self) -> bool {
        let needed = !self.0;
        self.0 = true;
        needed
    }
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
            keyboard_focus_synced: FrameGate::default(),
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
        self.keyboard_focus_synced.reset();
    }

    /// `true` (and marks it done) only the first time this is called
    /// since the last [`Self::begin_frame`] — see `keyboard_focus_synced`'s
    /// own doc comment. `render.rs::build_frame_elements` is this
    /// field's only real gate; the other, mutation-time call sites of
    /// `sync_keyboard_focus_to_view` (`sync_hut_space`/
    /// `sync_visible_main_window`) deliberately stay ungated — they run
    /// once per mutation event, not once per output per frame, so
    /// they're not the O(outputs) pattern this exists to fix, and
    /// calling `sync_keyboard_focus_to_view` again redundantly is always
    /// safe regardless (see its own doc comment).
    pub(crate) fn take_needs_keyboard_focus_sync(&mut self) -> bool {
        self.keyboard_focus_synced.take()
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

    /// Which `OutputSlot` (if any) wraps this real `Output` — `Output`'s
    /// own `PartialEq` is `Arc::ptr_eq` (real handle identity, not mode/
    /// name comparison), so this correctly picks out one specific
    /// connector even if two outputs briefly share a mode/scale. Needed
    /// anywhere a real `Output` handle is the only thing on hand (a
    /// layer-shell surface's own `wl_output`, resolved via
    /// `Output::from_resource`) and the caller needs to reach that
    /// output's own `_for(output_index)` accessors instead of always the
    /// focused one.
    pub fn output_index_for(&self, output: &Output) -> Option<usize> {
        self.outputs.iter().position(|slot| &slot.output == output)
    }

    /// Which output's own subtree contains the `ConsoleHut` with this id
    /// — needed wherever only a stable Hut id is on hand
    /// (`grabs.rs`'s `MoveSurfaceGrab`, which captures an id at grab-
    /// start rather than an output index precisely so it stays correct
    /// even if focus moves to a different output mid-drag). `None` if
    /// no Hut with this id exists anywhere (its shell already exited).
    pub fn output_index_for_hut(&self, hut_id: u64) -> Option<usize> {
        (0..self.outputs.len()).find(|&i| self.all_huts_for(i).any(|h| h.id == hut_id))
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

    /// Output `index`'s own real, positioned rectangle — `OutputSlot::position`
    /// plus its current mode, scale-divided into Logical. `None` if
    /// `index` is out of range or that output has no real mode yet.
    /// Shared by [`Self::output_index_at`]/[`Self::virtual_bounding_box`],
    /// and used directly by `input.rs`'s relative-motion clamp to keep a
    /// real pointer's position genuinely *inside* whichever output it
    /// resolves to — see that call site's own comment: two side-by-side
    /// outputs of different heights leave a "dead zone" region inside
    /// [`Self::virtual_bounding_box`]'s own bounding hull but outside
    /// every real output's rect, which this lets a caller clamp out of.
    pub fn output_rect(&self, index: usize) -> Option<Rectangle<f64, Logical>> {
        let slot = self.outputs.get(index)?;
        let mode = slot.output.current_mode()?;
        let scale = slot.output.current_scale().fractional_scale();
        let size = mode.size.to_f64().to_logical(scale);
        Some(Rectangle::<f64, Logical>::new(slot.position.to_f64(), size))
    }

    /// Output `index`'s own real position in the shared global compositor
    /// space — `(0, 0)` for an out-of-range index, the same "never end up
    /// with nothing to add/subtract" convention every caller of this was
    /// independently re-deriving by hand before this existed
    /// (`grabs.rs`'s `MoveSurfaceGrab`, `docks.rs`'s `DockDrag`,
    /// `input.rs`'s pointer-motion rebasing, `handlers/xdg_shell.rs`'s
    /// `move_request`) — one place to change if that fallback policy
    /// (e.g. it should propagate `None` instead) ever needs to.
    pub fn output_position(&self, index: usize) -> Point<i32, Logical> {
        self.outputs.get(index).map(|slot| slot.position).unwrap_or_default()
    }

    /// Which output's own real, positioned rectangle contains `pos` —
    /// per the user's resolved focus-follows-mouse policy. Falls back to
    /// the currently-focused output if `pos` doesn't land inside any real
    /// output's rect (e.g. before any output has a real mode yet, or the
    /// pointer is briefly outside every known rect) — never a bare
    /// `Option`, mirroring every other "there's always a currently-focused
    /// output" invariant in this module.
    pub fn output_index_at(&self, pos: Point<f64, Logical>) -> usize {
        for i in 0..self.outputs.len() {
            if self.output_rect(i).is_some_and(|rect| rect.contains(pos)) {
                return i;
            }
        }
        self.focused_output
    }

    /// The bounding hull of every real output's own positioned rect — the
    /// whole virtual desktop a pointer can actually be somewhere within,
    /// not just the focused output's own local `(0, 0)..size` bounds.
    /// Used by `input.rs`'s relative-motion (real mouse/touchpad) clamp:
    /// clamping against the focused output alone (this method's
    /// predecessor) meant a real pointer device could never actually
    /// cross onto a second monitor at all — `pointer_location` just
    /// pinned at the focused output's own edge — even though every other
    /// multi-monitor piece (`output_index_at`, focus-follows-mouse) was
    /// already built to handle it. Falls back to a zero-sized rect at the
    /// origin if no output has a real mode yet (matches `output_index_at`'s
    /// own "there's always a fallback" shape).
    ///
    /// A bounding *hull*, not a true union: two side-by-side outputs of
    /// different heights (mudhuts' own connector-connect layout always
    /// places new outputs to the right at the same `y = 0`, so this only
    /// ever bites on mismatched heights, never widths) leave a "dead
    /// zone" region inside this hull that belongs to neither real
    /// output. Clamping only against this alone would let a fast-enough
    /// diagonal motion land there, where `output_index_at` falls back to
    /// whichever output is already focused without that output's rect
    /// actually containing the point — `input.rs`'s caller clamps a
    /// second time, into [`Self::output_rect`] for whichever index that
    /// resolves to, specifically to close that gap.
    pub fn virtual_bounding_box(&self) -> Rectangle<f64, Logical> {
        (0..self.outputs.len())
            .filter_map(|i| self.output_rect(i))
            .reduce(|a, b| a.merge(b))
            .unwrap_or_else(|| Rectangle::new((0.0, 0.0).into(), (0.0, 0.0).into()))
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
        self.shows_terminal_effective_given_tiled(top, is_effectively_tiled(&self.graph, top))
    }

    /// [`Self::shows_terminal_effective`], given the caller already knows
    /// whether `top` is tiled — `render.rs::build_frame_elements` needs
    /// that same `is_effectively_tiled` result for its own separate
    /// purposes on every frame anyway, so calling the plain version right
    /// next to it recomputed it (and re-cloned the `Vec<NodeId>`
    /// `hut_list_input` builds just to read its length) a second time in
    /// a row for no reason (caught in review).
    pub(crate) fn shows_terminal_effective_given_tiled(&self, top: NodeId, is_tiled: bool) -> bool {
        if is_tiled {
            return true;
        }
        let leaf = self.graph.focused_leaf(top);
        let Some(console) = self.graph.downcast::<ConsoleNode>(leaf) else {
            return true;
        };
        *console.hut.showing_terminal || console.hut.main_window_count() == 0
    }

    /// `root`'s absolute physical-pixel rect right now, if it's a Main
    /// Window, Floating Window, or Alert currently on screen under `top`
    /// — mirrors `Hut::leaf_absolute_rect` exactly (same real-output-
    /// absolute coordinates convention, computed via the same
    /// `hut::pane_rects` call `TileNode`'s own `resolve`/`resize_to_pixels`
    /// already use, so this can never disagree with what's actually
    /// rendered/sized, for the Main-Window case). A Floating Window/Alert
    /// isn't fullscreen like a Main Window, so it can't just reuse `area`
    /// — resolved instead via
    /// [`crate::console_hut::ConsoleHut::floating_or_alert_absolute_rect`],
    /// which already returns this same physical-pixel convention. Takes
    /// `top`/`area` explicitly (not implicitly the focused output) —
    /// `State::focused_leaf_absolute_rect`/`State::leaf_absolute_rect_for`
    /// are the "for a particular output" wrappers around this.
    pub fn leaf_absolute_rect(
        &self,
        top: NodeId,
        root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        area: (i32, i32, i32, i32),
    ) -> Option<(i32, i32, i32, i32)> {
        if let Some(console) = self.graph.downcast::<ConsoleNode>(top) {
            if console.hut.main_windows().iter().any(|e| e.matches(root)) {
                return Some(area);
            }
            // Not a bare Main Window (always fullscreen, so `area` alone
            // is correct for one) — check whether it's a Floating Window
            // or Alert instead, which floats at its own tracked position
            // and needs its own real rect, not `area`. See
            // `ConsoleHut::floating_or_alert_absolute_rect`'s own doc
            // comment for why this wasn't handled before it existed.
            return console.hut.floating_or_alert_absolute_rect(root);
        }
        if let Some(tab) = self.graph.downcast::<TabNode>(top) {
            let children = self.graph.hut_list_input(top, "children");
            let next = *children.get(*tab.active)?;
            return self.leaf_absolute_rect(next, root, area);
        }
        if let Some(tile) = self.graph.downcast::<TileNode>(top) {
            let children = self.graph.hut_list_input(top, "children");
            let fracs = fracs_for(&children, &tile.fracs);
            let rects: Vec<_> = crate::hut::pane_rects(tile.axis, fracs.into_iter(), (area.2, area.3))
                .into_iter()
                .map(|(x, y, w, h)| (x + area.0, y + area.1, w, h))
                .collect();
            for (&child, rect) in children.iter().zip(rects) {
                let leaf = self.graph.focused_leaf(child);
                if let Some(console) = self.graph.downcast::<ConsoleNode>(leaf) {
                    if console.hut.main_windows().iter().any(|e| e.matches(root)) {
                        return Some(rect);
                    }
                    // Same Floating Window/Alert fallback as the bare
                    // ConsoleNode branch above — a pane's own ConsoleHut
                    // can still tag one even though Tile-Hut panes only
                    // ever *render* their terminal (see this pane's
                    // `rect`'s own doc comment on `Hut::leaf_absolute_rect`'s
                    // real-output-absolute convention, which this fallback
                    // already returns in, same as `floating_or_alert_absolute_rect`
                    // itself).
                    if let Some(floating) = console.hut.floating_or_alert_absolute_rect(root) {
                        return Some(floating);
                    }
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
        let fracs = fracs_for(&children, &tile.fracs);
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

    /// Depth-first search for a `ConsoleHut` with this id, rooted at
    /// `start` — short-circuits on the first match instead of collecting
    /// every id in the subtree first (unlike `all_node_ids`/
    /// `all_node_ids_for`), since [`Self::find_mut`]/[`Self::find_mut_for`]
    /// only ever want one specific node, often via a hot loop (see their
    /// own doc comments).
    fn find_console_node(&self, start: NodeId, id: u64) -> Option<NodeId> {
        if self.graph.downcast::<ConsoleNode>(start).is_some_and(|c| c.hut.id == id) {
            return Some(start);
        }
        self.graph.hut_list_input(start, "children").into_iter().find_map(|child| self.find_console_node(child, id))
    }

    /// Find a `ConsoleHut` by id anywhere in the whole graph, across
    /// every output — mirrors `MruStackHut::find_mut`.
    pub fn find_mut(&mut self, id: u64) -> Option<&mut ConsoleHut> {
        let node_id = self.all_top_level_entries().copied().find_map(|top| self.find_console_node(top, id))?;
        Some(&mut self.graph.downcast_mut::<ConsoleNode>(node_id)?.hut)
    }

    /// [`Self::find_mut`], scoped to one output's own subtree — a cheap
    /// fast path for hot loops (`grabs.rs`'s `MoveSurfaceGrab::motion`,
    /// `docks.rs`'s `advance_drag`, both driven by every pointer-motion
    /// sample during a drag) that already know which output a captured
    /// `hut_id` last resolved to, instead of paying `find_mut`'s full
    /// every-output walk on every single sample. `None` scoped just to
    /// `output_index` doesn't necessarily mean the Hut is gone —
    /// `output_index` can go stale if an output was unplugged/renumbered
    /// mid-drag — so callers on a hot path should fall back to
    /// [`Self::find_mut`] before concluding the Hut actually exited.
    pub fn find_mut_for(&mut self, output_index: usize, id: u64) -> Option<&mut ConsoleHut> {
        // Bounds-checked, not `top_level_entries_for`'s own unchecked
        // `out_at` — every other `_for(output_index)` accessor in this
        // file is called with an index resolved fresh within the same
        // call, but this one's hot-path caller `grabs.rs`'s
        // `MoveSurfaceGrab` (via `find_mut_for_hint`) caches
        // `output_index` on itself *across* multiple event-loop turns —
        // an output unplugged mid-drag can shift later indices down
        // (`GraphStack::remove_output`'s own doc comment) and leave that
        // cached index pointing past the end of `self.outputs`, which
        // `out_at`'s unchecked indexing would panic on. `docks.rs`'s
        // `advance_drag` is the other `find_mut_for_hint` caller, but no
        // longer caches an index on `DockDrag` itself the same way — see
        // `DockDrag::output`'s own doc comment — it just resolves one
        // fresh each call and passes it straight through as the hint.
        if output_index >= self.outputs.len() {
            return None;
        }
        let node_id = self
            .top_level_entries_for(output_index)
            .copied()
            .find_map(|top| self.find_console_node(top, id))?;
        Some(&mut self.graph.downcast_mut::<ConsoleNode>(node_id)?.hut)
    }

    /// [`Self::find_mut_for`], falling back to the full [`Self::find_mut`]
    /// search on a miss — the actual pattern every hot-loop caller wants
    /// (`grabs.rs`'s `MoveSurfaceGrab::motion`, `docks.rs`'s
    /// `advance_drag`, both call sites this consolidates): try the cheap
    /// per-output hint first, but never mistake a stale/miss-scoped
    /// `output_index` for the Hut itself having exited. Living here once,
    /// instead of copy-pasted at each call site, means a future change to
    /// this fallback policy (a retry limit, logging on fallback, ...)
    /// only has to happen in one place.
    pub fn find_mut_for_hint(&mut self, output_index: usize, id: u64) -> Option<&mut ConsoleHut> {
        // Resolves a plain `NodeId` first (an `&self` search — free to
        // try the scoped hint, then fall back to the full search, with
        // no borrow-checker fight at all, unlike attempting the same
        // fallback directly against `find_mut`/`find_mut_for`'s own
        // `&mut self` results across `match` arms), then does exactly
        // one `&mut self` resolution at the very end — so the common
        // (found-via-hint) case pays for the scoped search exactly once,
        // matching the cost of the 3 call sites this consolidates.
        //
        // Bounds-checked before touching `top_level_entries_for` (whose
        // own `out_at` indexes unchecked) — see `find_mut_for`'s
        // identical guard/doc comment: `output_index` is cached across
        // event-loop turns by every hot-path caller here, and can go
        // stale/out-of-bounds if an output is unplugged mid-drag.
        let hint = self.outputs.get(output_index).into_iter().flat_map(|slot| slot.top_level_entries());
        let node_id = hint
            .copied()
            .find_map(|top| self.find_console_node(top, id))
            .or_else(|| self.all_top_level_entries().copied().find_map(|top| self.find_console_node(top, id)))?;
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

    /// Every `ConsoleHut` reachable from a single output's own top-level
    /// entries — the per-output counterpart of [`Self::all_huts`], for
    /// anything that must only touch what's actually on one specific
    /// output's own screen (`handlers/layer_shell.rs`'s
    /// `reconfigure_main_windows`, which must not resize windows on an
    /// unrelated monitor just because *this* output's exclusive zone
    /// changed).
    pub fn all_huts_for(&self, output_index: usize) -> impl Iterator<Item = &ConsoleHut> {
        self.all_node_ids_for(output_index)
            .into_iter()
            .filter_map(|id| self.graph.downcast::<ConsoleNode>(id))
            .map(|n| &n.hut)
    }

    /// Every node id reachable from any output's top-level entries —
    /// the graph-native walk `MruStackHut::all_huts`'s `Hut::all_huts`
    /// recursion used to do directly on owned `Vec<Hut>` structure, done
    /// here against `hut_list_input` links instead.
    fn all_node_ids(&self) -> Vec<NodeId> {
        self.all_node_ids_from(self.all_top_level_entries().copied())
    }

    /// [`Self::all_node_ids`], scoped to one output's own top-level
    /// entries only.
    fn all_node_ids_for(&self, output_index: usize) -> Vec<NodeId> {
        self.all_node_ids_from(self.top_level_entries_for(output_index).copied())
    }

    fn all_node_ids_from(&self, tops: impl Iterator<Item = NodeId>) -> Vec<NodeId> {
        fn walk(graph: &Graph<RenderEnv>, id: NodeId, out: &mut Vec<NodeId>) {
            out.push(id);
            for child in graph.hut_list_input(id, "children") {
                walk(graph, child, out);
            }
        }
        let mut out = Vec::new();
        for top in tops {
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

        // Every fallible step from here on is provably unreachable today
        // (see `swap_child_in_place`'s own doc comment on why) — but if
        // one ever did fail, `new_id`'s `ConsoleHut` is a live PTY/shell
        // process (`ConsoleHut::spawn`, above), not just a graph node;
        // leaving it (and whatever wrapper node got created before the
        // failure) orphaned in `self.graph` on the way out would leak
        // both for the rest of the session. `wrap_link` rolls back
        // everything it created itself via `or_rollback`; this only has
        // to additionally remove `new_id`, the one node it created
        // before delegating (caught in review).
        let result = self.wrap_link(kind, new_id);
        self.or_rollback(new_id, result)?;

        self.redraw.mark_dirty();
        self.insert_channel(id, events)
    }

    /// Removes `id` from the graph if `result` is `Err`, then returns
    /// `result` unchanged — [`Self::wrap_link`]'s shared "roll back the
    /// node this step just created if the next thing about it fails"
    /// step, so a future fallible step added there can't omit the
    /// rollback by copy-pasting the pattern wrong (caught in review on
    /// an earlier version that hand-rolled this identically 4 times).
    fn or_rollback<T>(&mut self, id: NodeId, result: Result<T, String>) -> Result<T, String> {
        if result.is_err() {
            self.graph.remove_node(id);
        }
        result
    }

    /// [`Self::wrap`]'s own linking step (build the Tab/Tile wrapper
    /// around `new_id` and the current focused leaf, then splice it into
    /// the tree) — split out only so its error path can roll back
    /// whatever it already created on the way to a later failure,
    /// uniformly, regardless of which of the several fallible steps
    /// below actually failed.
    fn wrap_link(&mut self, kind: WrapKind, new_id: NodeId) -> Result<(), String> {
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
                let result =
                    self.graph.set_hut_list(wrapped_id, "children", vec![new_id, focused_leaf]).map_err(|err| format!("{err:?}"));
                self.or_rollback(wrapped_id, result)?;
                wrapped_id
            }
            WrapKind::Tile(axis) => {
                let mut tile = TileNode::new(axis);
                tile.fracs.insert(new_id, 0.5);
                tile.fracs.insert(focused_leaf, 0.5);
                *tile.active = 1;
                Redrawable::attach_redraw_handle(&mut tile, self.redraw.clone());
                let wrapped_id = self.graph.add_node(Box::new(tile));
                let result =
                    self.graph.set_hut_list(wrapped_id, "children", vec![new_id, focused_leaf]).map_err(|err| format!("{err:?}"));
                self.or_rollback(wrapped_id, result)?;
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
            // correct automatically, no adjustment needed. See
            // `swap_child_in_place`'s own doc comment for the
            // `TileNode::fracs`/`TabNode::child_chrome` bookkeeping this
            // also has to carry along.
            let parent = path[path.len() - 2];
            let result = self.swap_child_in_place(parent, focused_leaf, wrapped_id);
            self.or_rollback(wrapped_id, result)?;
        }
        Ok(())
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
        self.spawn_and_insert_for(self.focused_output)
    }

    /// [`Self::spawn_and_insert`], targeting a specific output rather
    /// than always the focused one — needed by `remove_exited`, which
    /// can need to refill a *background* output that just lost its last
    /// top-level entry (see its own doc comment for why `spawn_and_insert`
    /// itself, always pushing to `self.out_mut()`, would refill the
    /// wrong output there).
    fn spawn_and_insert_for(&mut self, output_index: usize) -> Result<u64, String> {
        let (hut, events) = ConsoleHut::spawn(self.extra_env.clone(), self.scale)?;
        let id = hut.id;
        self.insert_channel(id, events)?;
        let mut node = ConsoleNode::new(hut);
        Redrawable::attach_redraw_handle(&mut node, self.redraw.clone());
        let node_id = self.graph.add_node(Box::new(node));
        self.outputs[output_index].huts.push(node_id);
        // A newly-inserted top-level entry always needs a redraw
        // somewhere (at minimum the Alt-Tab popup/tab strip now has an
        // extra entry, and it can become the immediately-visible one if
        // `advance_forward` just pushed `pos` onto it) — pinged here
        // rather than left for every caller to remember, the same
        // "structural, not hand-followed" fix already applied to
        // keyboard-focus resync this session: every *internal* caller
        // today already happens to ping afterward too (`next`/`prev`) —
        // `wrap_tab`/`wrap_tile` do NOT go through this function at all
        // (`wrap()` calls `ConsoleHut::spawn` directly and has its own
        // separate `mark_dirty()` call — don't remove that one thinking
        // this covers it) — but `autostart.rs`'s direct call to
        // `spawn_and_insert_with_command` (this method's sibling, same
        // fix applied there too) did not — invisible only because it
        // runs before the backend's own first render pass, not because
        // anything guaranteed it. Idempotent/cheap to call redundantly —
        // `RedrawHandle::mark_dirty`'s own doc comment.
        self.redraw.mark_dirty();
        Ok(id)
    }

    /// [`Self::spawn_and_insert`], running `program`/`args` as the new
    /// ConsoleHut's own PTY child instead of a shell — `autostart.rs`'s
    /// own caller, one dedicated Hut per autostart entry. Immediately
    /// marked `touched` (unlike every other freshly-spawned ConsoleHut):
    /// `touched` tracks "has a keystroke ever been sent to this
    /// terminal" (see `ConsoleHut::touched`'s own doc comment), which an
    /// autostart entry's Hut never gets — nobody's meant to type into it
    /// — so leaving it `false` would make `advance_forward`'s "discard an
    /// untouched entry rather than grow the stack" rule silently destroy
    /// it (and every window it owns) the moment the user's own `next()`
    /// navigation happened to land on it, exactly the bug this method
    /// exists to fix. It's running something real, not sitting empty as
    /// a disposable scratch console, so it isn't disposable the same way.
    pub fn spawn_and_insert_with_command(&mut self, program: String, args: Vec<String>) -> Result<u64, String> {
        let (mut hut, events) =
            ConsoleHut::spawn_with_command(self.extra_env.clone(), self.scale, Some((program, args)))?;
        hut.mark_touched();
        let id = hut.id;
        self.insert_channel(id, events)?;
        let mut node = ConsoleNode::new(hut);
        Redrawable::attach_redraw_handle(&mut node, self.redraw.clone());
        let node_id = self.graph.add_node(Box::new(node));
        self.out_mut().huts.push(node_id);
        // See `spawn_and_insert_for`'s identical call/doc comment — this
        // is that fix's actual motivating caller: `autostart.rs` calls
        // this directly and, before this, never triggered a redraw of
        // its own, relying entirely on the coincidence that it runs
        // before the backend's first render pass.
        self.redraw.mark_dirty();
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
    ///
    /// Returns the touched output's index (`Some`) so the caller can
    /// resync keyboard focus there — this can shift which entry is
    /// focused (a bare top-level removal shifts/clamps `current`, a
    /// nested removal can collapse a Tab/Tile node onto a sibling pane),
    /// but nothing in this method touches `KeyboardHandle::set_focus`
    /// itself (no `&mut State`/renderer available here), so a caller that
    /// skips the resync leaves real Wayland keyboard focus pointed at
    /// whatever surface it was on before the exit — silently routing
    /// every subsequent keystroke to the wrong Hut/pane whenever the
    /// newly-focused entry differs from the old one. `None` for the
    /// unknown-id no-op case, where nothing actually changed.
    pub fn remove_exited(&mut self, id: u64) -> Result<Option<usize>, String> {
        let Some(node_id) = self.all_node_ids().into_iter().find(|&node_id| {
            self.graph.downcast::<ConsoleNode>(node_id).is_some_and(|c| c.hut.id == id)
        }) else {
            return Ok(None);
        };
        // Resolved *before* any removal below, while `id`'s own Hut still
        // exists to find — the nested branch's `remove_child` search is
        // global (`self.all_node_ids()`, not scoped to any one output),
        // so `self.focused_output` is the wrong fallback whenever the
        // collapsed Tab/Tile pane lives on a *background* output: it
        // would report the unrelated focused output as touched, so the
        // caller's keyboard-focus resync (see this method's own doc
        // comment) would resync the wrong output and leave the actually-
        // changed background output's `Space` — and its keyboard focus —
        // stale.
        let owning_output = self.output_index_for_hut(id);

        // Search every output's own top-level slots, not just the
        // focused one — real multi-monitor: `node_id` can be a bare
        // top-level entry on *any* output. Using `self.focused_output`
        // unconditionally here missed a background output's own exiting
        // Hut, so the "bare top-level entry" branch below never ran for
        // it; it instead fell into the "nested" branch (a no-op, since
        // nothing actually references it as a child), yet the node was
        // still deleted from the graph regardless — leaving a dangling
        // `NodeId` in that output's own `huts`, which panicked the next
        // time `focused_for`/`focused_mut_for` resolved it there.
        let found_output = self
            .outputs
            .iter()
            .enumerate()
            .find_map(|(i, out)| out.huts.iter().position(|&h| h == node_id).map(|top_index| (i, top_index)));

        let touched_output = if let Some((output_index, top_index)) = found_output {
            // A bare top-level entry — drop it outright.
            self.graph.remove_node(node_id);
            let out = &mut self.outputs[output_index];
            out.huts.remove(top_index);
            let new_len = out.huts.len();
            // Not the final answer for either field — the unconditional
            // clamp a few lines below this whole `if`/`else` still has to
            // run regardless, since `spawn_and_insert_for` can also
            // change `touched_output`'s length in between. Using the
            // shared helper here anyway keeps this in sync with
            // `remove_child`'s identical-shaped `TabNode`/`TileNode`
            // logic and `ConsoleHut`'s own `active_main_window` handling
            // — see [`crate::hut::shift_active_index_on_removal`]'s own
            // doc comment for why a plain clamp alone isn't enough.
            out.current = crate::hut::shift_active_index_on_removal(out.current, top_index, new_len);
            if let Some(preview) = out.preview {
                out.preview = Some(crate::hut::shift_active_index_on_removal(preview, top_index, new_len));
            }
            output_index
        } else {
            // Nested inside some top-level entry's own Tab/Tile chain —
            // remove it from whichever node's `children` list references
            // it, collapsing that node back to a bare child if only one
            // survives (mirrors `Hut::remove_child_hut`). Doesn't change
            // any output's own top-level `huts` list, so there's no
            // specific *other* output to re-clamp below — but `owning_output`
            // (resolved above, before removal) is still needed as the
            // caller's keyboard-focus resync target: `remove_child`'s own
            // search is global, so the collapsed pane can just as easily
            // be on a background output as the focused one.
            self.remove_child(node_id);
            self.graph.remove_node(node_id);
            owning_output.unwrap_or(self.focused_output)
        };

        // `touched_output`, not unconditionally the focused one — the
        // "bare top-level entry" branch above can empty out a
        // *background* output, and `spawn_and_insert` (which only ever
        // pushes to the *focused* output) would otherwise both fail to
        // refill the output that actually went empty (leaving it with a
        // `huts.len() == 0`, which the very next line's `self.huts[self.current]`-
        // style indexing elsewhere in this module assumes never happens)
        // and spawn an extra, unwanted Hut on the focused one instead.
        if self.outputs[touched_output].is_empty() {
            self.spawn_and_insert_for(touched_output)?;
        }
        let out = &mut self.outputs[touched_output];
        let max_index = out.len().saturating_sub(1);
        out.current = out.current.min(max_index);
        if let Some(preview) = &mut out.preview {
            *preview = (*preview).min(max_index);
        }
        Ok(Some(touched_output))
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
            let Some(removed_index) = children.iter().position(|&c| c == target) else {
                continue;
            };
            children.remove(removed_index);
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
                // `parent` survives with a shorter `children` list —
                // clamp its own `active` index to match. Without this, a
                // removal of anything but the *last* child left `active`
                // pointing past the end of the shrunk list whenever it
                // had been pointing at (or past) the removed slot:
                // `TabNode`/`TileNode::focused_child` degrade gracefully
                // (`.get()`), but `Graph::focused_path` then stops at
                // `parent` itself instead of reaching a real leaf, and
                // the next `GraphStack::focused()`/`focused_mut()` (etc.)
                // call panics trying to downcast that non-`ConsoleNode`
                // "leaf" — see this codebase's standing no-panics rule.
                let new_len = children.len();
                let _ = self.graph.set_hut_list(parent, "children", children);
                // Shift/clamp `active` — see `hut::shift_active_index_on_removal`'s
                // own doc comment for why a plain clamp alone isn't
                // enough (it would leave `active` pointing at the *next*
                // child over whenever the removed one was before it,
                // silently changing which tab/pane reads as focused).
                // Pruning `target`'s own `child_chrome`/`fracs` entry
                // isn't load-bearing (see `TabNode::child_chrome`'s doc
                // comment — a `NodeId`-keyed cache can't desync from a
                // shorter/reordered `children` the way the old positional
                // `Vec`s could), just cheap hygiene against unbounded
                // growth across a long session of repeated tab/pane
                // open-close.
                if let Some(tab) = self.graph.downcast_mut::<TabNode>(parent) {
                    tab.child_chrome.remove(&target);
                    *tab.active = crate::hut::shift_active_index_on_removal(*tab.active, removed_index, new_len);
                } else if let Some(tile) = self.graph.downcast_mut::<TileNode>(parent) {
                    tile.fracs.remove(&target);
                    *tile.active = crate::hut::shift_active_index_on_removal(*tile.active, removed_index, new_len);
                }
            }
            return;
        }
    }

    /// Replace every reference to `old` (a top-level slot, or an entry
    /// in some other node's `children` list) with `new` — the
    /// "collapse" half of [`Self::remove_child`]. `swap_child_in_place`
    /// (below) is what actually finds and rewrites each node's own
    /// `children` list — this just tries every node as a candidate
    /// `parent`, relying on that helper's own no-op-if-`old`-isn't-there
    /// behavior for the ones that don't reference it.
    fn repoint(&mut self, old: NodeId, new: NodeId) {
        for out in &mut self.outputs {
            for hut in &mut out.huts {
                if *hut == old {
                    *hut = new;
                }
            }
        }
        for parent in self.all_node_ids() {
            if let Err(err) = self.swap_child_in_place(parent, old, new) {
                tracing::warn!("repoint: {err}");
            }
        }
    }

    /// Replace `old` with `new` at whatever position it currently
    /// occupies in `parent`'s own `children` list — a no-op if `old`
    /// isn't actually one of `parent`'s children. Shared by [`Self::wrap`]
    /// (a pane getting wrapped in a new Tab/Tile, in place) and
    /// [`Self::repoint`] (a Tab/Tile collapsing to its one surviving
    /// child) — both are "this child's own identity changed but it's
    /// still logically the same slot" operations, previously each
    /// hand-rolling the same `children[pos] = new` swap plus its own
    /// copy of the `TileNode::fracs`/`TabNode::child_chrome` bookkeeping
    /// below. Caught in review as its own risk once duplicated: the
    /// bookkeeping itself is a hand-followed convention no different in
    /// shape from the parallel-array bug this `NodeId`-keyed design was
    /// introduced to eliminate — any *third* future in-place-swap call
    /// site copy-pasting the pattern instead of calling this would be
    /// just as easy to get wrong as the original bug. Consolidating to
    /// one call site removes that risk instead of just documenting it.
    ///
    /// `TileNode::fracs`'s entry migrates from `old` to `new` (a user-
    /// chosen pane-size ratio would otherwise be silently lost — see
    /// that field's own doc comment). `TabNode::child_chrome`'s entry is
    /// only pruned, not migrated — a render-cache entry regenerating
    /// fresh under `new`'s id on the next frame is correct, cheap
    /// behavior, not data loss; leaving `old`'s entry behind instead
    /// (the original, review-caught gap) would leak a `LabelCache`/two
    /// `Id`s/a `ChangeTracker` per swap, unboundedly, across a long-lived
    /// daily-driver session.
    ///
    /// Returns the underlying `set_hut_list` failure rather than
    /// swallowing it — not reachable today (`parent` only ever arrives
    /// here already proven to have a valid `children` `HutList` port,
    /// via `old`'s own presence in it a few lines above, which only ever
    /// resolves through real Tab/Tile nodes), but `wrap`'s own call site
    /// used to propagate this exact failure via `?` before it was folded
    /// into this shared helper — silently discarding it here would be a
    /// real, if currently-unreachable, narrowing of that contract:
    /// `wrap_tab`/`wrap_tile` could report success while leaving a
    /// freshly-spawned ConsoleHut and wrapper node fully configured but
    /// orphaned, unreachable from any parent (caught in review). Callers
    /// with no `Result` of their own to propagate through (`repoint`)
    /// log it instead.
    fn swap_child_in_place(&mut self, parent: NodeId, old: NodeId, new: NodeId) -> Result<(), String> {
        let mut children = self.graph.hut_list_input(parent, "children");
        let Some(pos) = children.iter().position(|&c| c == old) else {
            return Ok(());
        };
        children[pos] = new;
        self.graph
            .set_hut_list(parent, "children", children)
            .map_err(|err| format!("failed to relink {parent:?}'s children: {err:?}"))?;
        if let Some(tile) = self.graph.downcast_mut::<TileNode>(parent) {
            if let Some(frac) = tile.fracs.remove(&old) {
                tile.fracs.insert(new, frac);
            }
        } else if let Some(tab) = self.graph.downcast_mut::<TabNode>(parent) {
            tab.child_chrome.remove(&old);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Shared with `ownership.rs`/`handlers/shell.rs`'s own test modules —
    // aliased to this file's pre-existing local names rather than
    // renaming every one of this file's own call sites.
    use crate::test_support::test_stack as new_stack;

    #[test]
    fn starts_with_a_single_focused_untouched_hut() {
        let stack = new_stack();
        assert_eq!(stack.len(), 1);
        assert!(!stack.focused().touched());
    }

    #[test]
    fn keyboard_focus_sync_is_needed_once_then_not_again_until_the_next_frame() {
        let mut stack = new_stack();
        assert!(stack.take_needs_keyboard_focus_sync(), "first call this frame should need a sync");
        assert!(!stack.take_needs_keyboard_focus_sync(), "a second call in the same frame is redundant");
        assert!(!stack.take_needs_keyboard_focus_sync(), "still redundant on a third call");
        stack.begin_frame();
        assert!(stack.take_needs_keyboard_focus_sync(), "a new frame needs its own sync again");
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
    fn wrap_focused_migrates_the_tile_pane_fraction_to_the_new_wrapper() {
        // Regression case caught in review: `TileNode::fracs` is keyed
        // by `NodeId` (see its own doc comment's "trap for a new call
        // site" section), and `wrap`'s in-place `children[pos] =
        // wrapped_id` swap doesn't automatically carry a pane's custom
        // fraction over to the new id the way the old positional
        // `Vec<f64>` did for free.
        let mut stack = new_stack();
        stack.wrap_tile().unwrap(); // Tile[new_hut, orig], fracs = {new_hut: 0.5, orig: 0.5}
        let tile_id = stack.focused_top_level();
        let before = stack.graph().hut_list_input(tile_id, "children");
        assert_eq!(before.len(), 2);
        let orig_pane_id = before[1];

        stack.wrap_tab().unwrap(); // wraps the focused (orig) pane into a new Tab node, in place

        let after = stack.graph().hut_list_input(tile_id, "children");
        assert_eq!(after.len(), 2, "still just the 2 original panes");
        assert_ne!(after[1], orig_pane_id, "the wrapped pane's own id changed (it's now the new Tab node)");
        let tile = stack.graph().downcast::<TileNode>(tile_id).unwrap();
        let fracs = crate::graph_nodes::fracs_for(&after, &tile.fracs);
        assert_eq!(
            fracs,
            vec![0.5, 0.5],
            "the wrapped pane's custom 50/50 split survived the in-place identity swap, not silently reset to the 1.0 default"
        );
    }

    #[test]
    fn collapsing_a_tab_inside_a_tile_migrates_the_tile_pane_fraction() {
        // Regression case caught in review, same root cause as `wrap`'s
        // identical fix: `repoint` (called when a Tab/Tile collapses to
        // its one surviving child — `remove_child`'s collapse branch)
        // does the same in-place "swap this child's own id for a
        // different one, same position" substitution `wrap` does, so an
        // ancestor `TileNode`'s own `fracs` entry needs the same
        // explicit migration.
        let mut stack = new_stack();
        let b_id = stack.spawn_and_insert().unwrap();
        stack.spawn_and_insert().unwrap();
        let node_ids: Vec<NodeId> = stack.outputs[0].huts.drain(..).collect();
        assert_eq!(node_ids.len(), 3);
        let (a_node, b_node, c_node) = (node_ids[0], node_ids[1], node_ids[2]);

        // TabNode(b, c)
        let mut tab = TabNode::new();
        Redrawable::attach_redraw_handle(&mut tab, stack.redraw.clone());
        let tab_id = stack.graph.add_node(Box::new(tab));
        stack.graph.set_hut_list(tab_id, "children", vec![b_node, c_node]).unwrap();

        // TileNode[a, TabNode(b, c)], with a custom 30/70 split.
        let mut tile = TileNode::new(Axis::Horizontal);
        tile.fracs.insert(a_node, 0.3);
        tile.fracs.insert(tab_id, 0.7);
        Redrawable::attach_redraw_handle(&mut tile, stack.redraw.clone());
        let tile_id = stack.graph.add_node(Box::new(tile));
        stack.graph.set_hut_list(tile_id, "children", vec![a_node, tab_id]).unwrap();
        stack.outputs[0].huts.push(tile_id);
        stack.outputs[0].current = 0;

        stack.remove_exited(b_id).unwrap(); // b leaves TabNode(b,c), which collapses to c via repoint(tab_id, c_node)

        let children = stack.graph.hut_list_input(tile_id, "children");
        assert_eq!(children, vec![a_node, c_node], "the collapsed Tab node was replaced by its survivor c");
        let tile = stack.graph.downcast::<TileNode>(tile_id).unwrap();
        let fracs = crate::graph_nodes::fracs_for(&children, &tile.fracs);
        assert_eq!(
            fracs,
            vec![0.3, 0.7],
            "the collapsing Tab's own 0.7 share migrated to its survivor c, not reset to the 1.0 default"
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
    fn remove_exited_finds_a_bare_top_level_entry_on_a_non_focused_output() {
        // Regression case: `remove_exited` used to only ever search the
        // *focused* output's own top-level slots for the exiting node,
        // so a background output's own exiting Hut fell through to the
        // "nested" branch (a no-op there) while the node was still
        // deleted from the graph regardless — leaving a dangling
        // `NodeId` in that output's `huts` that panicked the next time
        // it was resolved.
        let mut stack = new_stack();
        stack.add_output(crate::space_element::synthetic_output("b", (800, 600), 1.0), Point::from((1920, 0))).unwrap();
        assert_eq!(stack.focused_output_index(), 0, "focus stays on output 0");
        let background_id = stack.focused_for(1).id;

        stack.remove_exited(background_id).unwrap();

        assert_eq!(stack.len_for(1), 1, "output 1 should have been refilled, never left with zero entries");
        assert_ne!(stack.focused_for(1).id, background_id, "the exited Hut is really gone");
        assert_eq!(stack.len_for(0), 1, "output 0 (unrelated) is untouched");
    }

    #[test]
    fn remove_child_clamps_active_and_prunes_tab_node_cache() {
        // Regression case: closing a non-last tabbed pane used to leave
        // `TabNode::active` pointing past the end of the shrunk
        // `children` list (eventually panicking via `focused()`'s
        // `ConsoleNode` downcast `.expect()`). `child_chrome` being
        // `NodeId`-keyed means it can't desync from `children` the way
        // the old positional `Vec`s could — this now also checks that
        // the removed child's own cache entry is actually pruned (cheap
        // hygiene, not load-bearing correctness) and that the survivors'
        // entries are untouched, not just "the right count remains".
        let mut stack = new_stack();
        let b_id = stack.spawn_and_insert().unwrap();
        stack.spawn_and_insert().unwrap();
        // `wrap_tab` only ever builds a flat 2-child node — assemble a
        // flat 3-child `TabNode` directly instead, out of the 3
        // top-level entries `new_stack`/`spawn_and_insert` just built.
        let node_ids: Vec<NodeId> = stack.outputs[0].huts.drain(..).collect();
        assert_eq!(node_ids.len(), 3);

        let mut tab = TabNode::new();
        *tab.active = 2; // pointing at the last child
        Redrawable::attach_redraw_handle(&mut tab, stack.redraw.clone());
        let tab_id = stack.graph.add_node(Box::new(tab));
        stack.graph.set_hut_list(tab_id, "children", node_ids.clone()).unwrap();
        stack.outputs[0].huts.push(tab_id);
        stack.outputs[0].current = 0;

        // Seed the per-child cache for all 3, the same way a real render
        // pass would (village_chrome.rs's own `.entry(...).or_insert_with(...)`).
        stack.graph.with_node_mut(tab_id, |node, _graph| {
            let tab = node.as_any_mut().downcast_mut::<TabNode>().unwrap();
            for &id in &node_ids {
                tab.child_chrome.entry(id).or_insert_with(crate::graph_nodes::TabChildChrome::new);
            }
        });

        stack.remove_exited(b_id).unwrap(); // removes the middle child

        let children = stack.graph.hut_list_input(tab_id, "children");
        assert_eq!(children, vec![node_ids[0], node_ids[2]], "the middle child (b) is gone, the rest kept its order");
        let tab = stack.graph.downcast::<TabNode>(tab_id).unwrap();
        assert_eq!(*tab.active, 1, "clamped from 2 down to the new last valid index");
        assert_eq!(tab.child_chrome.len(), 2, "the removed child's own cache entry was pruned");
        assert!(tab.child_chrome.contains_key(&node_ids[0]));
        assert!(tab.child_chrome.contains_key(&node_ids[2]));
        assert!(!tab.child_chrome.contains_key(&node_ids[1]), "the removed middle child (b)'s own entry is gone");
    }

    #[test]
    fn remove_child_shifts_active_left_when_the_removed_child_sat_before_it() {
        // Regression case: a plain `.min(max_index)` clamp alone leaves
        // `active` pointing at the *next* child over whenever the
        // removed child sat before it, since clamping only catches the
        // "removed the last child" case — it doesn't shift `active` to
        // keep tracking the same surviving child. 4 children, active
        // pointing at the 3rd (index 2); removing the 1st (index 0)
        // should leave `active` at index 1 (still the same child, now
        // shifted down), not clamped-but-wrong at index 2.
        let mut stack = new_stack();
        let a_id = stack.focused().id;
        stack.spawn_and_insert().unwrap();
        stack.spawn_and_insert().unwrap();
        stack.spawn_and_insert().unwrap();
        let node_ids: Vec<NodeId> = stack.outputs[0].huts.drain(..).collect();
        assert_eq!(node_ids.len(), 4);

        let mut tab = TabNode::new();
        *tab.active = 2; // pointing at the 3rd child
        Redrawable::attach_redraw_handle(&mut tab, stack.redraw.clone());
        let tab_id = stack.graph.add_node(Box::new(tab));
        stack.graph.set_hut_list(tab_id, "children", node_ids.clone()).unwrap();
        stack.outputs[0].huts.push(tab_id);
        stack.outputs[0].current = 0;

        let expected_survivor = node_ids[2];
        stack.remove_exited(a_id).unwrap(); // removes the 1st child, before `active`

        let children = stack.graph.hut_list_input(tab_id, "children");
        assert_eq!(children.len(), 3);
        let tab = stack.graph.downcast::<TabNode>(tab_id).unwrap();
        assert_eq!(*tab.active, 1, "shifted left to keep pointing at the same surviving child");
        assert_eq!(children[*tab.active], expected_survivor, "active still resolves to the child that was focused before the removal");
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
    fn find_mut_for_and_find_mut_for_hint_never_panic_on_a_stale_output_index() {
        // Regression case: `MoveSurfaceGrab`/`DockDrag` cache an
        // output_index across multiple event-loop turns (a drag hot-path
        // fast path); an output unplugged mid-drag can shift later
        // indices down or shrink the outputs list entirely, leaving that
        // cached index pointing past the end of `self.outputs`. Both
        // methods used to index unchecked via `out_at`, which would
        // panic instead of gracefully treating an out-of-range index as
        // "not found there."
        let mut stack = new_stack();
        let id = stack.focused().id;
        let out_of_bounds = 5;
        assert!(stack.find_mut_for(out_of_bounds, id).is_none());
        // `find_mut_for_hint` still finds it via its full-search fallback.
        assert_eq!(stack.find_mut_for_hint(out_of_bounds, id).map(|h| h.id), Some(id));
        assert!(stack.find_mut_for_hint(out_of_bounds, 999_999).is_none());
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

        // A point in the "dead zone" `virtual_bounding_box`'s own doc
        // comment describes — inside the hull, but outside both real
        // outputs (second output is only 600 tall, first is 1080) —
        // falls back the same way, rather than claiming to be inside
        // either output's own rect.
        let dead_zone = Point::from((2000.0, 800.0));
        assert!(stack.output_rect(0).is_some_and(|r| !r.contains(dead_zone)));
        assert!(stack.output_rect(1).is_some_and(|r| !r.contains(dead_zone)));
        assert_eq!(stack.output_index_at(dead_zone), stack.focused_output_index());
    }

    #[test]
    fn output_position_falls_back_to_the_origin_for_an_out_of_range_index() {
        let mut stack = new_stack();
        stack.add_output(crate::space_element::synthetic_output("second", (800, 600), 1.0), Point::from((1920, 0))).unwrap();
        assert_eq!(stack.output_position(0), Point::from((0, 0)));
        assert_eq!(stack.output_position(1), Point::from((1920, 0)));
        assert_eq!(stack.output_position(5), Point::from((0, 0)), "out-of-range falls back to the origin, not a panic");
    }

    #[test]
    fn virtual_bounding_box_unions_every_positioned_output_rect() {
        // Regression case: `input.rs`'s real-mouse relative-motion clamp
        // used to clamp against just the focused output's own local
        // bounds, which pinned a real pointer device at that output's
        // edge forever — it could never actually reach a second monitor.
        let mut stack = new_stack();
        stack.set_output(0, crate::space_element::synthetic_output("first", (1920, 1080), 1.0));
        stack.add_output(crate::space_element::synthetic_output("second", (800, 600), 1.0), Point::from((1920, 0))).unwrap();

        let bounds = stack.virtual_bounding_box();
        assert_eq!(bounds.loc, Point::from((0.0, 0.0)));
        // The union should reach past the first output's own 1920 width,
        // out to the second output's far edge at 1920 + 800.
        assert_eq!(bounds.size.w, 2720.0);
        assert_eq!(bounds.size.h, 1080.0, "height is the union's max, not the first output's own");
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
