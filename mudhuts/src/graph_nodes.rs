//! Real Hut node types built on the graph core (`graph.rs`) — migration
//! steps 2, 3, 5, and 6 of `docs/rfcs/typed-graph-hut.md`:
//! `TabNode`/`TileNode` (Tab/Tile), `ConsoleNode`/`WaylandClientNode`
//! (Console/Terminal + the real content-flow port), `MainWindowNode`,
//! `LayerShellNode`, and `OutputHut`. `TabNode`/`TileNode`'s cycling is
//! proven equivalent to `hut.rs`'s existing `TabbedHut`/`TileHut`
//! cycling by literally sharing [`crate::hut::wrapping_step`], not a
//! separately-reimplemented copy of it.
//!
//! Still not wired into `main.rs`'s real render/input call paths
//! (migration step 4) — these are real, usable node types, just not live
//! yet.
//!
//! ## What's unit-tested here, and what can't be
//!
//! `ConsoleNode` (needs a real `&mut GlesRenderer`, exactly like
//! `ConsoleHut::redraw` itself, never unit-tested either) and
//! `WaylandClientNode` (needs a real `Window`, which needs a real live
//! `WlSurface`/client connection to construct at all) can't be exercised
//! by a plain unit test at all — Smithay's own API gives no way to
//! construct a fake `GlesTexture` or `Window` without a live GL/Wayland
//! context. Where that's blocked a *decision* the node's own `resolve`
//! makes (e.g. `WaylandClientNode`'s terminal-toggle choice) is pulled
//! out into its own pure function and tested directly instead (see
//! `terminal_pass_through_target`) — real logic coverage without
//! needing the untestable parts. Every purely structural node
//! (`TabNode`/`TileNode`/`MainWindowNode`/`LayerShellNode`/`OutputHut`)
//! stays fully unit-tested, since none of them need a live context to
//! construct or resolve.

// Nothing in `main.rs`/`render.rs`/`input.rs` constructs these node
// types yet — the module's own unit tests already exercise everything
// that can be (see above). Remove once migration step 4 cuts the real
// call paths over to the graph.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::Window;
use smithay::utils::Point;

use crate::console_hut::ConsoleHut;
use crate::graph::{ContentPiece, Graph, InputPort, Node, NodeId, OutputPort, PortKind, PortValue, RenderedContent};
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

    fn focused_child(&self, graph: &Graph<Env>, self_id: NodeId) -> Option<NodeId> {
        graph.hut_list_input(self_id, "children").get(*self.active).copied()
    }

    fn resize_to_pixels(&mut self, graph: &mut Graph<Env>, self_id: NodeId, width: i32, height: i32) {
        // Every child gets the full size, not just `active` — mirrors
        // `Hut::resize_to_pixels`'s Tab arm exactly: only `active` is
        // ever shown, but every child needs to track the real size so
        // switching to it doesn't show a stale layout.
        for child in graph.hut_list_input(self_id, "children") {
            graph.with_node_mut(child, |node, graph| node.resize_to_pixels(graph, child, width, height));
        }
    }

    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        Redrawable::attach_redraw_handle(self, handle);
    }
}

impl Redrawable for TabNode {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.active.attach_redraw_handle(handle);
    }
}

/// Graph-native Tile-Hut: shows every child at once, side by side along
/// `axis` — same semantics as [`crate::hut::TileHut`]. `fracs`/`size`
/// parallel `children` the same way `TabbedHut::label_cache`/`tab_ids`/
/// `bg_tracker` already parallel *its* `children` today (kept in sync by
/// whatever manipulates the list); `size` is this node's own current
/// pixel size, set by whatever propagates a resize down the tree
/// (mirroring `Hut::resize_to_pixels`'s existing top-down cascade).
pub struct TileNode {
    pub axis: Axis,
    pub active: Signal<usize>,
    pub fracs: Vec<f64>,
    pub size: (i32, i32),
}

impl TileNode {
    pub fn new(axis: Axis) -> Self {
        Self { axis, active: Signal::new(0), fracs: Vec::new(), size: (0, 0) }
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
        // Every pane is visible at once (unlike TabNode) — every child's
        // pieces are collected and translated to that pane's own offset
        // within this Tile's local frame, exactly the position
        // `hut::TileHut::absolute_pane_rects` already computes for the
        // enum tree (same `hut::pane_rects` call, same axis/fracs/size
        // inputs) — real per-child pixel positions, kept in the same
        // "local frame, translated by the caller" convention
        // `ConsoleNode`'s own doc comment establishes, not yet the real
        // output's absolute position (that final translation happens
        // once, at the top of the tree).
        let children = graph.hut_list_input(self_id, "children");
        // `self.fracs` is a caller-maintained parallel array (see this
        // struct's doc comment) — if it's ever out of sync with
        // `children`'s current length (a caller updated one and forgot
        // the other), falling back to even fracs here means the real
        // failure mode is "this pane's share of the tile is wrong until
        // whatever forgot to update `fracs` is fixed," not "every pane
        // past the mismatch silently vanishes" (a `zip` against a
        // shorter `rects` would drop them entirely, an easy-to-miss bug
        // since nothing panics or logs).
        let fracs = if self.fracs.len() == children.len() {
            self.fracs.clone()
        } else {
            vec![1.0; children.len()]
        };
        let rects = crate::hut::pane_rects(self.axis, fracs.into_iter(), self.size);
        let mut pieces = Vec::new();
        for (&child, (px, py, _, _)) in children.iter().zip(rects) {
            let Some(PortValue::Content(child_pieces)) = graph.resolve_output(child, "content") else {
                continue;
            };
            for piece in child_pieces {
                pieces.push(translate_piece(piece, px, py));
            }
        }
        PortValue::Content(pieces)
    }

    fn focused_child(&self, graph: &Graph<Env>, self_id: NodeId) -> Option<NodeId> {
        // The *active pane's* own focused leaf, not just itself — mirrors
        // `Hut::focused_hut`'s Tile arm (`tile.children[*tile.active].0.
        // focused_hut()`). Returning this pane's id from `focused_child`
        // is enough on its own: `Graph::focused_leaf`'s walk keeps
        // calling `focused_child` again on whatever this returns, so a
        // pane that's itself a nested Tab/Tile keeps resolving deeper
        // automatically.
        graph.hut_list_input(self_id, "children").get(*self.active).copied()
    }

    fn resize_to_pixels(&mut self, graph: &mut Graph<Env>, self_id: NodeId, width: i32, height: i32) {
        let children = graph.hut_list_input(self_id, "children");
        let fracs = if self.fracs.len() == children.len() { self.fracs.clone() } else { vec![1.0; children.len()] };
        self.size = (width, height);
        let rects = crate::hut::pane_rects(self.axis, fracs.into_iter(), (width, height));
        for (child, (_, _, w, h)) in children.into_iter().zip(rects) {
            graph.with_node_mut(child, |node, graph| node.resize_to_pixels(graph, child, w, h));
        }
    }

    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        Redrawable::attach_redraw_handle(self, handle);
    }
}

/// Shift a [`ContentPiece`]'s position by a pane offset (physical
/// pixels, matching `hut::pane_rects`'s own units) — used by
/// [`TileNode::resolve`] to place each child's content at its own pane's
/// origin within the tile's local frame. `Window`-kind pieces aren't
/// translated (left at their own reported position): a Tile-Hut pane
/// only ever shows its child's *terminal* in v1 scope (see `hut.rs`'s
/// own module doc on why a Main Window never appears in a tile pane
/// today), so a `Window` piece reaching here would already be outside
/// that scope — passed through unchanged rather than guessing at a
/// translation with no established meaning yet, matching this
/// codebase's convention of degrading rather than guessing.
fn translate_piece(piece: ContentPiece, dx: i32, dy: i32) -> ContentPiece {
    match piece {
        ContentPiece::Texture { id, texture, damage, position: (x, y) } => {
            ContentPiece::Texture { id, texture, damage, position: (x + dx as f64, y + dy as f64) }
        }
        window @ ContentPiece::Window { .. } => window,
    }
}

impl Redrawable for TileNode {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.active.attach_redraw_handle(handle);
    }
}

/// The real compositor's `Graph` environment (migration step 3, per the
/// RFC's "Why `Graph<Env>`" section in `graph.rs`) — whatever a
/// render-shaped node needs beyond the graph itself. Holds the renderer
/// via `Rc<RefCell<_>>`, not a plain `&mut GlesRenderer`: the graph
/// itself is a long-lived structure (`ConsoleNode` owns real `ConsoleHut`
/// state — a live PTY, a GPU glyph atlas — that has to survive across
/// frames, so `Graph<RenderEnv>` has to be storable long-term on
/// whatever replaces `MruStackHut`), but a render pass only ever has a
/// renderer borrowed for the duration of *one* call. A struct field
/// holding `&'a mut GlesRenderer` would tie that one call's lifetime to
/// the whole `Graph`'s own lifetime, which can't work for a value stored
/// across many calls. `Rc<RefCell<_>>` sidesteps this the same way
/// `udev_backend.rs`'s own `Inner::renderer` already does (real
/// precedent in this codebase, not a new pattern) — `RenderEnv` itself
/// becomes a plain `'static`-compatible, `Clone`-able value.
///
/// `renderer` is `Option` (not a bare `Rc<RefCell<GlesRenderer>>`) for a
/// second, unrelated reason: `main.rs` constructs the stack (and so this
/// `RenderEnv`) *before* either backend creates a real `GlesRenderer` —
/// `init_udev`/`init_winit` run after — so there's a real, if brief,
/// window where no renderer exists yet to put here. Rather than
/// restructure `main.rs`'s init order around this one field, the slot
/// starts empty and whichever backend creates the real renderer fills it
/// in once it exists (`GraphStack::set_renderer`, `graph_stack.rs`).
/// `ConsoleNode::resolve` degrades to empty content rather than
/// panicking if it's ever somehow called before that — in practice it
/// never actually is, since nothing renders before a backend exists to
/// drive rendering in the first place.
#[derive(Clone)]
pub struct RenderEnv {
    pub renderer: Rc<RefCell<Option<GlesRenderer>>>,
}

const LEAF_OUTPUTS: &[OutputPort] = &[OutputPort { name: "content", kind: PortKind::Content }];

/// Graph-native Console/Terminal leaf — wraps a real [`ConsoleHut`],
/// replicating *all* of `render.rs::content_elements`'s per-ConsoleHut
/// behavior (not just the terminal branch): while `showing_terminal` (or
/// there are no Main Windows at all), a single `ContentPiece::Texture`
/// from `ConsoleHut::redraw`, exactly as before; otherwise, one
/// `ContentPiece::Window` per element mapped in `ConsoleHut::space`
/// (whatever `ConsoleHut::sync_main_window_space` put there — the active
/// Main Window plus its floating Floating Windows/Alerts), each already
/// positioned by that `Space`'s own bookkeeping. This is why a separate
/// `WaylandClientNode`/`MainWindowNode` per client isn't needed for a
/// faithful cutover: `ConsoleHut` already bundles exactly this
/// responsibility today, and `ConsoleNode` just needs to expose the same
/// bundle through the graph, not redesign it — the real Main-Window-Hut
/// redesign (`WaylandClientNode`'s real content-flow port, `MainWindowNode`'s
/// `main`/`minimized` ports) is tracked as its own separate wishlist item,
/// layered on *after* this cutover exists, not a prerequisite for it.
///
/// Real per-piece render-element construction (`AsRenderElements::
/// render_elements`, which is what actually expands a `Window` into real
/// `HutSpaceRenderElement`s — Smithay's own `Window` impl already walks
/// `PopupManager::popups_for_surface` internally, so this loses no popup
/// fidelity versus today's `space_render_elements` call, which delegates
/// to the exact same `Window` impl under the hood) happens later, at
/// whatever converts a resolved `Vec<ContentPiece>` into real frame
/// elements — deliberately *not* here: `TextureRenderElement`/
/// `WaylandSurfaceRenderElement` don't implement `Clone` (confirmed
/// against Smithay's pinned source — each owns real per-element damage
/// state with its own `Drop`-time bookkeeping), so they can never be the
/// thing `Graph::resolve_output`'s per-frame memoization cache clones.
///
/// Only implements `Node` for `RenderEnv` specifically (not generic over
/// every `Env` the way `TabNode`/`TileNode` are) — it genuinely needs a
/// real renderer, unlike a purely structural node.
pub struct ConsoleNode {
    pub hut: ConsoleHut,
}

impl ConsoleNode {
    pub fn new(hut: ConsoleHut) -> Self {
        Self { hut }
    }
}

impl Node<RenderEnv> for ConsoleNode {
    fn inputs(&self) -> &[InputPort] {
        &[]
    }
    fn outputs(&self) -> &[OutputPort] {
        LEAF_OUTPUTS
    }
    fn resolve(&mut self, graph: &mut Graph<RenderEnv>, _self_id: NodeId, _port: &'static str) -> PortValue {
        // Local origin (0, 0) — same convention `TileNode`'s pane
        // translation and `hut::TileHut::absolute_pane_rects` already
        // use: a node's own `content` output is in its *own* local
        // frame, translated by whatever composes it further up the tree
        // (ultimately the real output's `usable_area()` origin, applied
        // once at the very top, not baked in here).
        if *self.hut.showing_terminal || self.hut.main_window_count() == 0 {
            let mut guard = graph.env.renderer.borrow_mut();
            let Some(renderer) = guard.as_mut() else {
                return PortValue::Content(Vec::new());
            };
            let Some(texture) = self.hut.redraw(renderer) else {
                return PortValue::Content(Vec::new());
            };
            let damage = self.hut.element_damage_snapshot();
            return PortValue::Content(vec![ContentPiece::Texture {
                id: self.hut.element_id.clone(),
                texture,
                damage,
                position: (0.0, 0.0),
            }]);
        }

        let pieces = self
            .hut
            .space
            .elements()
            .filter_map(|element| {
                let crate::space_element::HutSpaceElement::Window(window) = element else {
                    // No `Composited` element is ever mapped into a
                    // ConsoleHut's own `space` in v1 — see
                    // `state.rs::surface_under`'s identical comment.
                    return None;
                };
                let mapped_at = self.hut.space.element_location(element)?;
                // Matches `Space::render_elements_for_region`'s own
                // `render_location()` formula exactly (checked against
                // Smithay's pinned source: `self.location -
                // self.element.geometry().loc`) — a window's own
                // `geometry().loc` (its xdg-surface-reported content
                // offset, typically `(0, 0)` but not always, e.g. a
                // client-side-shadow-drawing toolkit) has to be
                // subtracted the same way here, not just wherever
                // `map_element` originally placed it, or a window with a
                // non-zero geometry offset would render subtly
                // mispositioned versus today's `space_render_elements`
                // path.
                let position = mapped_at - smithay::desktop::space::SpaceElement::geometry(window).loc;
                Some(ContentPiece::Window { window: window.clone(), position })
            })
            .collect();
        PortValue::Content(pieces)
    }

    fn touched(&self) -> bool {
        self.hut.touched()
    }

    fn mark_touched(&mut self) {
        self.hut.mark_touched();
    }

    fn resize_to_pixels(&mut self, _graph: &mut Graph<RenderEnv>, _self_id: NodeId, width: i32, height: i32) {
        self.hut.resize_to_pixels(width, height);
    }

    fn rescale(&mut self, scale: f64) -> Result<(), String> {
        self.hut.rescale(scale)
    }

    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        Redrawable::attach_redraw_handle(self, handle);
    }
}

impl Redrawable for ConsoleNode {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.hut.attach_redraw_handle(handle);
    }
}

const CLIENT_INPUTS: &[InputPort] = &[InputPort { name: "terminal", kind: PortKind::Hut }];

/// Graph-native Wayland-client Hut: a single client toplevel window,
/// linked to its owning terminal via a `terminal: Hut` reference input
/// — the RFC's chosen **real content flow** design (not a bare ownership
/// tag): while `showing_terminal` is set, this node's own `content`
/// output is a *live* pass-through to whatever its linked terminal is
/// currently showing, re-resolved fresh through the graph every call
/// (via `Graph::resolve_output`, which is itself memoized per frame —
/// see that method's doc comment — so this costs nothing extra within
/// one frame), never a stale cached copy. Otherwise, it's a single
/// `ContentPiece::Window` wrapping its own real `window` at local origin
/// `(0, 0)` — `space_render_elements`/`AsRenderElements` (called by
/// whatever eventually consumes a resolved `ContentPiece::Window`, not
/// by this node itself) already expand a `Window` into its full render-
/// element tree, including subsurfaces/popups, so this node doesn't need
/// to enumerate any of that itself — same "local frame, translated by
/// the caller" convention as `ConsoleNode`/`TileNode`.
pub struct WaylandClientNode {
    pub window: Window,
    pub showing_terminal: Signal<bool>,
}

impl WaylandClientNode {
    pub fn new(window: Window) -> Self {
        Self { window, showing_terminal: Signal::new(false) }
    }
}

/// Whether resolving a Wayland-client Hut's `content` output should pass
/// through to its linked terminal this call, and which node to resolve
/// if so — pulled out as a pure function so the toggle/link-presence
/// decision itself stays unit-testable independent of
/// `WaylandClientNode` needing a real `Window` to even construct (see
/// this module's doc comment on why nothing GPU/protocol-shaped can be
/// unit-tested past that point).
fn terminal_pass_through_target(showing_terminal: bool, terminal: Option<NodeId>) -> Option<NodeId> {
    if showing_terminal { terminal } else { None }
}

impl<Env> Node<Env> for WaylandClientNode {
    fn inputs(&self) -> &[InputPort] {
        CLIENT_INPUTS
    }
    fn outputs(&self) -> &[OutputPort] {
        LEAF_OUTPUTS
    }
    fn resolve(&mut self, graph: &mut Graph<Env>, self_id: NodeId, _port: &'static str) -> PortValue {
        let terminal = graph.hut_input(self_id, "terminal");
        if let Some(target) = terminal_pass_through_target(*self.showing_terminal, terminal)
            && let Some(value) = graph.resolve_output(target, "content")
        {
            return value;
        }
        PortValue::Content(vec![ContentPiece::Window { window: self.window.clone(), position: Point::from((0, 0)) }])
    }
}

impl Redrawable for WaylandClientNode {
    fn attach_redraw_handle(&mut self, handle: RedrawHandle) {
        self.showing_terminal.attach_redraw_handle(handle);
    }
}

const MAIN_WINDOW_INPUTS: &[InputPort] = &[
    InputPort { name: "main", kind: PortKind::Hut },
    InputPort { name: "minimized", kind: PortKind::HutList },
];

/// Migration step 5: Main-Window Hut — the user's own framing, "the
/// main/floating display/minimization... should be done with a hut."
/// Accepts one Hut on `main` (shown full-size) and a separate list on
/// `minimized` (today's `docks.rs`/`FloatingWindow`/`Dock::Docked`
/// concept, replacing a per-struct enum field with a real port).
///
/// `resolve`'s `content` output is `main`'s own content, unmodified —
/// real compositing of `minimized`'s docked-handle chrome
/// (`docks::build`'s current role) on top of it isn't built here yet,
/// same reasoning `WaylandClientNode`'s doc comment gives for deferring
/// real client-surface rendering: entangled with migration step 4's
/// render-pipeline cutover, not separable ahead of it. What's real and
/// tested here: `main` is what's actually shown, and every minimized
/// entry is still a real, reachable graph node (traversed once per
/// resolve, proven the same way `TileNode`'s "touch every child" test
/// proves its own list traversal) even though nothing composites them
/// visually yet.
pub struct MainWindowNode;

impl<Env> Node<Env> for MainWindowNode {
    fn inputs(&self) -> &[InputPort] {
        MAIN_WINDOW_INPUTS
    }
    fn outputs(&self) -> &[OutputPort] {
        LEAF_OUTPUTS
    }
    fn resolve(&mut self, graph: &mut Graph<Env>, self_id: NodeId, _port: &'static str) -> PortValue {
        // Every minimized entry gets touched (proves reachability; see
        // this struct's doc comment) even though the result isn't
        // composited into anything yet.
        for minimized in graph.hut_list_input(self_id, "minimized") {
            graph.resolve_output(minimized, "content");
        }
        match graph.hut_input(self_id, "main") {
            Some(main) => graph
                .resolve_output(main, "content")
                .unwrap_or_else(|| PortValue::Content(RenderedContent::default())),
            None => PortValue::Content(RenderedContent::default()),
        }
    }
}

const LAYER_SHELL_INPUTS: &[InputPort] = &[
    InputPort { name: "display", kind: PortKind::Hut },
    InputPort { name: "layers", kind: PortKind::HutList },
];

/// Migration step 6: Layer-Shell Hut — one per output (see the RFC's
/// Multi-Monitor section), replacing today's global
/// `layer_map_for_output`/`space_render_elements` consolidation with a
/// real graph node. `display` is "whatever's underneath" (today's whole
/// Stack/Tab/Tile content — in the fully cut-over graph, the root of
/// whatever's linked to this output); `layers` is the list of
/// layer-shell client Huts stacked above/below it.
///
/// Same deferred-compositing scope as `MainWindowNode`/
/// `WaylandClientNode`: `content` resolves to `display`'s own content
/// unmodified; real per-layer positioning/exclusive-zone math
/// (`layer.rs`'s existing `arrange`/`non_exclusive_zone`, already
/// verified correct in `project_known_issues`'s "layer-shell rendering
/// bug" write-up — the bug was never in that logic) plugs in at
/// migration step 4's cutover, not here.
pub struct LayerShellNode;

impl<Env> Node<Env> for LayerShellNode {
    fn inputs(&self) -> &[InputPort] {
        LAYER_SHELL_INPUTS
    }
    fn outputs(&self) -> &[OutputPort] {
        LEAF_OUTPUTS
    }
    fn resolve(&mut self, graph: &mut Graph<Env>, self_id: NodeId, _port: &'static str) -> PortValue {
        for layer in graph.hut_list_input(self_id, "layers") {
            graph.resolve_output(layer, "content");
        }
        match graph.hut_input(self_id, "display") {
            Some(display) => graph
                .resolve_output(display, "content")
                .unwrap_or_else(|| PortValue::Content(RenderedContent::default())),
            None => PortValue::Content(RenderedContent::default()),
        }
    }
}

const OUTPUT_INPUTS: &[InputPort] = &[InputPort { name: "display", kind: PortKind::Hut }];

/// Migration step 7: Output Hut — a physical monitor, modeled as a real
/// graph node with an input and *no* output at all (nothing is ever
/// downstream of a real display). This is the insight that makes real
/// multi-monitor support fall out of the graph model almost for free
/// (see the RFC's Multi-Monitor section, and the user's own framing:
/// "make each monitor a hut with just an input and no output") — a
/// second monitor is just a second `OutputHut` linked to whatever Hut
/// subtree should show on it, real mirroring is linking the *same*
/// subtree to two `OutputHut`s, and nothing about the graph core needed
/// to change to support either.
///
/// `present` is deliberately not `Node::resolve`-shaped — an output has
/// no `Content`/`Control` output port for anything to pull a value from
/// (`outputs()` is empty), it's a true sink. Presenting a frame is
/// itself the side effect: resolve `display`'s content, then hand it to
/// whatever real backend (`udev_backend.rs`'s `render_surface`, today;
/// `winit_backend.rs`'s single always-present `OutputHut` under that
/// backend) actually owns the real `DrmOutput`/window this represents.
/// Wiring a real `OutputHut` per connected connector into
/// `udev_backend.rs`'s existing per-crtc `SurfaceData` map (which
/// already loops over every known crtc — see the RFC's Multi-Monitor
/// section on how little of the real DRM scanning path actually assumes
/// single-output) is real, hardware-facing work deliberately left
/// unstarted this session: it touches the exact code path the user's
/// live `mudhuts --tty` session depends on today, and — unlike every
/// other node in this file — isn't verifiable by a unit test at all, so
/// it deserves to be built and reviewed with the user present, not
/// blind while they're away.
pub struct OutputHut;

impl OutputHut {
    /// Resolve and return whatever's linked to `display`, if anything —
    /// the one real operation an output sink needs, called once per
    /// render pass by whatever backend owns the actual hardware output
    /// this represents.
    pub fn present<Env>(&self, graph: &mut Graph<Env>, self_id: NodeId) -> Option<PortValue> {
        let display = graph.hut_input(self_id, "display")?;
        graph.resolve_output(display, "content")
    }
}

impl<Env> Node<Env> for OutputHut {
    fn inputs(&self) -> &[InputPort] {
        OUTPUT_INPUTS
    }
    fn outputs(&self) -> &[OutputPort] {
        &[]
    }
    fn resolve(&mut self, _graph: &mut Graph<Env>, _self_id: NodeId, _port: &'static str) -> PortValue {
        unreachable!("OutputHut has no outputs — see Self::present, not Node::resolve")
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
        resized: std::rc::Rc<std::cell::Cell<(i32, i32)>>,
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
        fn resize_to_pixels(&mut self, _graph: &mut Graph<Env>, _self_id: NodeId, width: i32, height: i32) {
            self.resized.set((width, height));
        }
    }

    fn leaf(graph: &mut Graph) -> (NodeId, std::rc::Rc<std::cell::Cell<bool>>) {
        let flag: std::rc::Rc<std::cell::Cell<bool>> = Default::default();
        let id = graph.add_node(Box::new(LeafNode { resolved: flag.clone(), resized: Default::default() }));
        (id, flag)
    }

    fn leaf_with_size_tracking(
        graph: &mut Graph,
    ) -> (NodeId, std::rc::Rc<std::cell::Cell<(i32, i32)>>) {
        let resized: std::rc::Rc<std::cell::Cell<(i32, i32)>> = Default::default();
        let id = graph.add_node(Box::new(LeafNode { resolved: Default::default(), resized: resized.clone() }));
        (id, resized)
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

    // `WaylandClientNode` itself needs a real `Window` to even
    // construct (a live `WlSurface`/client connection, unavailable to a
    // unit test — see this module's doc comment), so its toggle/pass-
    // through *decision* is tested directly against
    // `terminal_pass_through_target` instead of through a constructed
    // node — the same logic `Node::resolve` calls, just reachable
    // without needing a real `Window` to exercise it.

    #[test]
    fn terminal_pass_through_target_passes_through_when_toggled_and_linked() {
        let mut graph = Graph::new();
        let (terminal, _) = leaf(&mut graph);
        assert_eq!(terminal_pass_through_target(true, Some(terminal)), Some(terminal));
    }

    #[test]
    fn terminal_pass_through_target_does_not_pass_through_when_not_toggled() {
        let mut graph = Graph::new();
        let (terminal, _) = leaf(&mut graph);
        assert_eq!(terminal_pass_through_target(false, Some(terminal)), None);
    }

    #[test]
    fn terminal_pass_through_target_toggled_but_unlinked_is_none() {
        assert_eq!(terminal_pass_through_target(true, None), None);
    }

    #[test]
    fn main_window_node_shows_main_and_still_touches_every_minimized_entry() {
        let mut graph = Graph::new();
        let (main, main_resolved) = leaf(&mut graph);
        let (min_a, min_a_resolved) = leaf(&mut graph);
        let (min_b, min_b_resolved) = leaf(&mut graph);
        let node_id = graph.add_node(Box::new(MainWindowNode));
        graph.link_hut(node_id, "main", main).unwrap();
        graph.set_hut_list(node_id, "minimized", vec![min_a, min_b]).unwrap();

        graph.begin_frame();
        graph.resolve_output(node_id, "content");
        assert!(main_resolved.get(), "main should be resolved — it's what's actually shown");
        assert!(min_a_resolved.get(), "every minimized entry should still be reachable/traversed");
        assert!(min_b_resolved.get(), "every minimized entry should still be reachable/traversed");
    }

    #[test]
    fn main_window_node_with_no_main_linked_falls_back_to_placeholder() {
        let mut graph = Graph::new();
        let node_id = graph.add_node(Box::new(MainWindowNode));
        graph.begin_frame();
        assert!(matches!(graph.resolve_output(node_id, "content"), Some(PortValue::Content(_))));
    }

    #[test]
    fn layer_shell_node_shows_display_and_touches_every_layer() {
        let mut graph = Graph::new();
        let (display, display_resolved) = leaf(&mut graph);
        let (layer, layer_resolved) = leaf(&mut graph);
        let node_id = graph.add_node(Box::new(LayerShellNode));
        graph.link_hut(node_id, "display", display).unwrap();
        graph.set_hut_list(node_id, "layers", vec![layer]).unwrap();

        graph.begin_frame();
        graph.resolve_output(node_id, "content");
        assert!(display_resolved.get());
        assert!(layer_resolved.get(), "layer-shell clients should still be reachable/traversed");
    }

    #[test]
    fn output_hut_present_resolves_whatever_is_linked_to_display() {
        let mut graph = Graph::new();
        let (display, display_resolved) = leaf(&mut graph);
        let output_id = graph.add_node(Box::new(OutputHut));
        graph.link_hut(output_id, "display", display).unwrap();

        graph.begin_frame();
        let output = OutputHut;
        assert!(output.present(&mut graph, output_id).is_some());
        assert!(display_resolved.get());
    }

    #[test]
    fn output_hut_present_with_nothing_linked_is_none_not_a_panic() {
        let mut graph = Graph::new();
        let output_id = graph.add_node(Box::new(OutputHut));
        graph.begin_frame();
        let output = OutputHut;
        assert!(output.present(&mut graph, output_id).is_none());
    }

    #[test]
    fn tab_node_focused_child_is_the_active_one() {
        let mut graph = Graph::new();
        let (a, _) = leaf(&mut graph);
        let (b, _) = leaf(&mut graph);
        let mut tab = TabNode::new();
        *tab.active = 1;
        let tab_id = graph.add_node(Box::new(tab));
        graph.set_hut_list(tab_id, "children", vec![a, b]).unwrap();

        assert_eq!(graph.focused_leaf(tab_id), b);
    }

    #[test]
    fn tab_node_resize_to_pixels_resizes_every_child_not_just_active() {
        let mut graph = Graph::new();
        let (a, a_resized) = leaf_with_size_tracking(&mut graph);
        let (b, b_resized) = leaf_with_size_tracking(&mut graph);
        let mut tab = TabNode::new();
        *tab.active = 1;
        let tab_id = graph.add_node(Box::new(tab));
        graph.set_hut_list(tab_id, "children", vec![a, b]).unwrap();

        graph.with_node_mut(tab_id, |node, graph| node.resize_to_pixels(graph, tab_id, 800, 600));

        assert_eq!(a_resized.get(), (800, 600), "inactive children still need to track the real size");
        assert_eq!(b_resized.get(), (800, 600));
    }

    #[test]
    fn tile_node_resize_to_pixels_splits_by_fracs() {
        let mut graph = Graph::new();
        let (a, a_resized) = leaf_with_size_tracking(&mut graph);
        let (b, b_resized) = leaf_with_size_tracking(&mut graph);
        let mut tile = TileNode::new(Axis::Horizontal);
        tile.fracs = vec![1.0, 1.0];
        let tile_id = graph.add_node(Box::new(tile));
        graph.set_hut_list(tile_id, "children", vec![a, b]).unwrap();

        graph.with_node_mut(tile_id, |node, graph| node.resize_to_pixels(graph, tile_id, 800, 600));

        assert_eq!(a_resized.get(), (400, 600), "even fracs should split the width evenly");
        assert_eq!(b_resized.get(), (400, 600));
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
