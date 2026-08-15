//! The typed port-graph core — migration step 1 of
//! `docs/rfcs/typed-graph-hut.md`. Generic `Node`/`Graph`/`PortValue`
//! machinery only; no real Hut integration yet (that's steps 2+). See the
//! RFC for the motivating use cases and the "why not generics" section on
//! why ports are a closed runtime enum rather than a `Node<In, Out>` type
//! parameter.
//!
//! Deliberately has no dependency on `GlesRenderer` itself, or on doing
//! any actual rendering — [`RenderedContent`] names `GlesTexture`/
//! `DamageSnapshot` as plain (usually `None`) field types, the same way
//! `ConsoleHut::last_texture` already does, but nothing here ever needs a
//! live GL context to construct or test against one: every test in this
//! module and `graph_nodes.rs` runs with `RenderedContent::default()`
//! (`None`/`None`), matching how `stack.rs`'s own tests already avoid
//! needing a real renderer at all.
//!
//! ## Two kinds of link, not one
//!
//! An earlier version of this module tried to route every port kind
//! (`Content`/`Control` *and* `Hut`/`HutList`) through one `Node::resolve`
//! call — caught before it shipped (the first attempt at writing real
//! tests immediately hit `PortKindMismatch` failures that made the
//! confusion obvious): a "Hut" isn't a *value* some node computes and
//! hands back the way a texture or a control scalar is — it's a
//! *reference* to another node already sitting in the graph. Linking a
//! Tab-Hut's `children` input to a child doesn't ask that child to
//! "resolve a Hut-typed output"; it just records "this id is one of my
//! children," full stop — the child's own actual content is resolved
//! separately, later, exactly the two-step "which child, then what does
//! it show" shape `Hut::focused_hut` already has today. So:
//!
//! - **Value ports** (`Content`/`Control`) go through [`Graph::link`] /
//!   [`Graph::resolve_input`] / [`Graph::resolve_output`] — kind-checked
//!   on both ends, resolved by calling into [`Node::resolve`].
//! - **Reference ports** (`Hut`/`HutList`) go through [`Graph::link_hut`] /
//!   [`Graph::set_hut_list`] / [`Graph::hut_input`] / [`Graph::hut_list_input`]
//!   — plain topology, no `Node::resolve` call involved, no kind-checking
//!   needed on the source side (a Hut reference can point at *any* node
//!   in the graph, unlike a value link, which must match a specific
//!   declared output kind).
//!
//! ## Why `Graph<Env>`
//!
//! A real render-shaped node (Console/Terminal, migration step 3) needs
//! an actual `&mut GlesRenderer` inside its own `Node::resolve` to
//! produce a real [`RenderedContent`] — but this module has no
//! `GlesRenderer` dependency at all (see above), and `Node::resolve`'s
//! signature has no renderer parameter. Adding one directly would force
//! *every* node (including every purely-structural one like `TabNode`,
//! and every synthetic test node in this module) to thread a renderer
//! through regardless of whether it needs one, and would force this
//! module itself to import `GlesRenderer` just to name the parameter
//! type — breaking the "runs with zero GL context" property every test
//! here and in `graph_nodes.rs` relies on.
//!
//! Instead, `Graph` is generic over an environment type (`Env`,
//! defaulting to `()`), held as a field a node's own `resolve` reaches
//! via `graph.env` only if it actually needs to. `Graph<()>` (what every
//! test in this module and `graph_nodes.rs` uses, via the plain
//! `Graph::new()`) needs nothing. The real compositor's graph is
//! `Graph<RenderEnv<'_>>`, defined in `graph_nodes.rs` alongside the
//! first node that actually reaches into it. A node that never touches
//! `graph.env` can stay fully generic over `Env` (`impl<Env> Node<Env>
//! for TabNode`, see `graph_nodes.rs`) — it works unchanged whether it's
//! ever resolved inside a test's `Graph<()>` or the real `Graph<
//! RenderEnv<'_>>`.

use std::collections::HashMap;

use crate::redraw::RedrawHandle;

/// A value a node computes for one of its own `Content`/`Control`-kind
/// output ports — see this module's doc comment for why `Hut`/`HutList`
/// aren't part of this (they're references, not computed values).
#[derive(Clone)]
pub enum PortValue {
    /// A single composited frame's worth of content — what a leaf
    /// (Console/Terminal, a Wayland client's own surface) or a
    /// compositing node (Tab/Tile/Main-Window/Layer-Shell) produces.
    Content(RenderedContent),
    /// A plain control/scalar value (a target refresh rate, a scale
    /// factor, ...) — see the RFC for why this stays a bare `f64` until
    /// a second real shape needs more.
    Control(f64),
}

/// The kind of thing a port carries — checked at link time (`Graph::link`
/// for value ports, `Graph::link_hut`/`set_hut_list` for reference ports),
/// not per-frame: a node's declared ports don't change after
/// construction (only *what's linked to them* does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
    /// A computed value port — see this module's doc comment.
    Content,
    Control,
    /// A reference to a single other node in the graph.
    Hut,
    /// An ordered list of references to other nodes in the graph.
    HutList,
}

impl PortKind {
    fn is_value(self) -> bool {
        matches!(self, PortKind::Content | PortKind::Control)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InputPort {
    pub name: &'static str,
    pub kind: PortKind,
}

#[derive(Debug, Clone, Copy)]
pub struct OutputPort {
    pub name: &'static str,
    pub kind: PortKind,
}

/// One already-rendered, already-positioned piece of a frame's content.
/// A node's `content` output is a *list* of these
/// ([`RenderedContent`] = `Vec<ContentPiece>`), never one flattened,
/// pre-composited texture — matching how the pre-graph render pipeline
/// already treats a Tile-Hut's panes, or a Main Window's Floating
/// Windows/Alerts, as *separate* render elements each with their own
/// damage tracking. Collapsing them into a single composited texture
/// (the way `composite_normal_content` does for the comparatively rare,
/// lower-priority Alt-Tab-thumbnail path — see that function's own doc
/// comment) would force a full GPU recomposite every frame *any one*
/// piece changes, a real performance regression on the main render path
/// every frame goes through, not an acceptable tradeoff there the way
/// it is for a popup only redrawn while open.
///
/// `Texture` positions are physical-pixel positions *within the
/// producing node's own local frame* — a leaf like `ConsoleNode` always
/// reports `(0, 0)` for its terminal texture; a compositing node
/// (`TileNode`) translates each child's pieces by that child's own
/// offset within its own local frame before passing them upward. The one
/// absolute translation (by the real output's `focused_usable_area()` origin)
/// happens exactly once, at the very top of the tree — mirroring
/// `hut::TileHut::absolute_pane_rects`'s own "local `pane_rects`,
/// translated by the caller" shape.
///
/// **`Window` positions are the one exception — already absolute, not
/// local-frame-relative.** `ConsoleNode` reads them straight from
/// `ConsoleHut::space`, whose own elements were mapped with
/// `focused_usable_area()`'s origin already baked in (`sync_main_window_space`'s
/// `map_element(window, area_origin, ...)` call) — matching exactly how
/// today's `space_render_elements`-based rendering already positions
/// them. Whatever ultimately converts a resolved `Vec<ContentPiece>`
/// into real frame elements must apply the top-of-tree origin
/// translation to `Texture` pieces only, never `Window` ones, or a
/// Main Window would render offset twice. `TileNode::translate_piece`
/// already reflects this (a `Window` piece passes through untouched) —
/// consistent, since a Tile-Hut pane never shows Main-Window content in
/// v1 scope anyway (see `hut.rs`'s own module doc).
#[derive(Clone)]
pub enum ContentPiece {
    /// A GPU-rendered texture (a Console/Terminal's own content, or —
    /// once migration step 4 builds it — a Tile-Hut pane's border
    /// highlight). `id` is this piece's own stable cross-frame identity
    /// — the same role `ConsoleHut::element_id` already plays — required
    /// for `TextureRenderElement::from_texture_with_damage`'s outer
    /// per-element damage tracking to recognize this as the *same*
    /// element frame to frame rather than a fresh one every time (see
    /// `ConsoleHut::damage_tracker`'s own doc comment on why a fresh
    /// `Id` per frame is a real correctness bug, not cosmetic).
    Texture {
        id: smithay::backend::renderer::element::Id,
        texture: smithay::backend::renderer::gles::GlesTexture,
        damage: smithay::backend::renderer::utils::DamageSnapshot<i32, smithay::utils::Buffer>,
        position: (f64, f64),
    },
    /// A real client `Window`'s own surface(s) — composited by
    /// Smithay's own `space_render_elements`/`AsRenderElements`
    /// machinery, not rendered by this compositor itself (and so
    /// needing no separate stable `Id` of its own — `Window`/its
    /// surfaces already carry their own identity through that
    /// machinery). A Main Window, Floating Window, Alert, or
    /// layer-shell client.
    Window {
        window: smithay::desktop::Window,
        position: smithay::utils::Point<i32, smithay::utils::Logical>,
    },
}

/// A node's rendered frame content this call — empty for a node with
/// nothing to show yet, or before a real renderer has ever run.
pub type RenderedContent = Vec<ContentPiece>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u64);

/// Anything that can sit in the graph — a Console/Terminal leaf, a
/// Tab/Tile/Main-Window/Layer-Shell/Output compositing node, or a
/// non-render control node (e.g. the RFC's refresh-rate example). A pure
/// sink (Output Hut) has `outputs() == &[]`; a pure leaf (Console/
/// Terminal) has `inputs() == &[]`. Generic over `Env` — see this
/// module's "Why `Graph<Env>`" doc section; a node that never needs the
/// environment can implement this for *any* `Env` (`impl<Env> Node<Env>
/// for MyNode`).
pub trait Node<Env> {
    fn inputs(&self) -> &[InputPort];
    fn outputs(&self) -> &[OutputPort];

    /// Produce this node's current value for one of its own declared
    /// `Content`/`Control`-kind output ports — never called for a
    /// `Hut`/`HutList`-kind output (there are none; see this module's
    /// doc comment on why references don't flow through here). Called by
    /// [`Graph::resolve_output`], which has already temporarily removed
    /// this node from the graph before calling in (see that method's
    /// doc comment) — `graph` here is safe to recurse into for any
    /// *other* node id without a borrow conflict, but must never be used
    /// to resolve `self`'s own id again (would panic — this node's slot
    /// is empty until this call returns). `self_id` is this node's own
    /// id — needed so a node can look up its own input links (e.g. a
    /// Tab-Hut's `graph.hut_list_input(self_id, "children")`), since
    /// `Node` has no other way to know which id it was constructed under
    /// (`Graph::add_node` assigns it after the fact).
    fn resolve(&mut self, graph: &mut Graph<Env>, self_id: NodeId, port: &'static str) -> PortValue;

    /// This node's own currently-focused child, if it delegates focus
    /// downward (a Tab/Tile-shaped node picking one active child) —
    /// `None` for a genuine leaf (Console/Terminal), whose own id
    /// callers should treat as the focused leaf itself. Mirrors
    /// `Hut::focused_hut`'s recursive walk, generalized so
    /// `Graph::focused_leaf` (below) doesn't need to know which concrete
    /// node type it's looking at — a `GraphStack` walks this repeatedly
    /// until it hits a leaf, the same way `Hut::focused_hut` recurses
    /// through `Tab`/`Tile` until it hits a bare `Console`.
    fn focused_child(&self, _graph: &Graph<Env>, _self_id: NodeId) -> Option<NodeId> {
        None
    }

    /// Whether this leaf has ever been interacted with — see
    /// `ConsoleHut::touched`'s doc comment. Defaults to `true`: matches
    /// `Hut::touched`'s existing rule that anything *other* than a bare
    /// Console leaf (a Tab/Tile a user deliberately composed) is always
    /// considered touched, never a "never-used, safe-to-discard"
    /// candidate.
    fn touched(&self) -> bool {
        true
    }

    /// Marks this leaf touched, if it tracks that at all — a no-op
    /// default, matching `Hut::mark_touched`'s existing no-op for
    /// anything but a bare Console leaf.
    fn mark_touched(&mut self) {}

    /// Resize every leaf reachable from this node to fill an output of
    /// `width`x`height` physical pixels — mirrors
    /// `Hut::resize_to_pixels`. Default no-op (a pure control/sink node,
    /// or any node with nothing pixel-sized of its own, has nothing to
    /// resize).
    fn resize_to_pixels(&mut self, _graph: &mut Graph<Env>, _self_id: NodeId, _width: i32, _height: i32) {}

    /// Rebuild this leaf's glyph cache (or whatever else is scale-
    /// dependent) for a newly-known real output scale — mirrors
    /// `ConsoleHut::rescale`. Default no-op.
    fn rescale(&mut self, _scale: f64) -> Result<(), String> {
        Ok(())
    }

    /// Wire this node (and, for a compositing node, everything under it)
    /// up to the shared redraw handle — see `crate::redraw::Redrawable`'s
    /// own doc comment for why this exists at all. A `Node` trait method
    /// (not just a separate `Redrawable` impl, which every real node type
    /// here still also implements, for anything that constructs one
    /// directly and doesn't need `dyn Node` dispatch) because
    /// `GraphStack::wrap_tab`/`wrap_tile` build a fresh Tab/Tile node
    /// generically, without knowing its concrete type — needs to reach
    /// this through `dyn Node<Env>`, not a concrete `Redrawable` bound.
    fn attach_redraw_handle(&mut self, _handle: RedrawHandle) {}

    /// Downcast to a concrete node type — needed because `Node`'s own
    /// interface deliberately stays narrow (the touched/resize/rescale
    /// methods above cover *generic* tree bookkeeping, but a `ConsoleNode`
    /// specifically has a large surface of `ConsoleHut`-only operations
    /// — terminal input, `main_windows_mut()`, `pixel_to_cell`, ... —
    /// that `GraphStack`'s many callers (`input.rs`, `handlers/*.rs`)
    /// need direct access to, not a generic `Node` method for each one).
    /// Not default-implementable, despite the body being identical
    /// everywhere (`{ self }`) — Rust can't cast a possibly-unsized
    /// `&Self` to `&dyn Any` in a body shared across every impl without
    /// a `Self: Sized` bound, which would make the method uncallable
    /// through `dyn Node<Env>` in the first place, defeating the point.
    /// Standard pattern (matches e.g. the `downcast-rs` crate's own
    /// approach): every concrete node type repeats the one-liner.
    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

#[derive(Debug)]
pub enum GraphError {
    UnknownNode(NodeId),
    UnknownOutputPort { node: NodeId, port: &'static str },
    UnknownInputPort { node: NodeId, port: &'static str },
    PortKindMismatch { input: PortKind, output: PortKind },
    /// [`Graph::link`]/[`Graph::resolve_input`] etc. only ever operate on
    /// `Content`/`Control`-kind ports — use the `_hut` family for a
    /// `Hut`/`HutList`-kind port instead (see this module's doc comment).
    NotAValuePort { node: NodeId, port: &'static str },
    /// [`Graph::link_hut`]/[`Graph::set_hut_list`] only ever operate on
    /// `Hut`/`HutList`-kind *input* ports.
    NotAReferencePort { node: NodeId, port: &'static str },
    WrongReferenceArity { node: NodeId, port: &'static str, expected: PortKind },
}

/// One graph: every node plus its two link tables (value links and Hut-
/// reference links — see this module's doc comment for why they're kept
/// separate). Both are stored keyed by their *downstream* (input) side.
/// Generic over `Env` — see this module's "Why `Graph<Env>`" doc section.
pub struct Graph<Env = ()> {
    nodes: HashMap<NodeId, Box<dyn Node<Env>>>,
    /// Value-port links (`Content`/`Control`) — `(node, port) ->
    /// (upstream node, upstream port)`, at most one source per input
    /// (linking again replaces the previous source).
    links: HashMap<(NodeId, &'static str), (NodeId, &'static str)>,
    /// `Hut`-kind reference links — `(node, port) -> referenced node`.
    hut_refs: HashMap<(NodeId, &'static str), NodeId>,
    /// `HutList`-kind reference links — `(node, port) -> referenced
    /// nodes, in order`. Set atomically via [`Self::set_hut_list`] rather
    /// than one reference at a time, matching how a Tab-Hut's `children:
    /// Vec<Hut>` is actually manipulated today (whole-Vec pushes/
    /// retains/reorders).
    hut_list_refs: HashMap<(NodeId, &'static str), Vec<NodeId>>,
    next_id: u64,
    /// Populated during [`Self::resolve_output`], cleared by
    /// [`Self::begin_frame`] — memoizes each `(node, port)` already
    /// resolved this pass, so a value reachable from two different
    /// downstream links (e.g. two Tab-Hut children sharing an upstream
    /// producer, possible once linking is general rather than a strict
    /// tree) is only actually computed once per frame, not once per
    /// reachable path to it.
    cache: HashMap<(NodeId, &'static str), PortValue>,
    /// Whatever a real node's `resolve` needs beyond the graph itself —
    /// `()` for every test in this module/`graph_nodes.rs`, a
    /// `RenderEnv<'_>` (holding a `&mut GlesRenderer`) for the real
    /// compositor's graph. Reached directly as a field (`graph.env`),
    /// not through a method, since a node's `resolve` already has `&mut
    /// Graph<Env>` in hand.
    pub env: Env,
}

impl<Env: Default> Default for Graph<Env> {
    fn default() -> Self {
        Self::with_env(Env::default())
    }
}

impl Graph<()> {
    /// Convenience constructor for the common `Env = ()` case — every
    /// test in this module and `graph_nodes.rs` uses this, unchanged
    /// from before `Graph` became generic over `Env`.
    pub fn new() -> Self {
        Self::with_env(())
    }
}

impl<Env> Graph<Env> {
    pub fn with_env(env: Env) -> Self {
        Self {
            nodes: HashMap::new(),
            links: HashMap::new(),
            hut_refs: HashMap::new(),
            hut_list_refs: HashMap::new(),
            next_id: 0,
            cache: HashMap::new(),
            env,
        }
    }

    pub fn add_node(&mut self, node: Box<dyn Node<Env>>) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.nodes.insert(id, node);
        id
    }

    /// Drops the node and prunes every link that pointed at or from it —
    /// a dangling reference is a real correctness bug (resolving it would
    /// panic on `self.nodes.remove(&id)?` inside `resolve_output`, or
    /// silently keep a supposedly-closed child alive in a `HutList`), not
    /// something calling code should have to remember to clean up by
    /// hand at every removal site.
    pub fn remove_node(&mut self, id: NodeId) {
        self.nodes.remove(&id);
        self.links.retain(|(node, _), (source, _)| *node != id && *source != id);
        self.hut_refs.retain(|(node, _), target| *node != id && *target != id);
        self.hut_list_refs.retain(|(node, _), _| *node != id);
        for targets in self.hut_list_refs.values_mut() {
            targets.retain(|target| *target != id);
        }
    }

    pub fn node(&self, id: NodeId) -> Option<&dyn Node<Env>> {
        self.nodes.get(&id).map(|b| &**b)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut (dyn Node<Env> + 'static)>
    where
        Env: 'static,
    {
        self.nodes.get_mut(&id).map(|b| &mut **b)
    }

    /// Every node in the graph, mutably, in one pass — unlike repeated
    /// individual [`Self::node_mut`]/[`Self::downcast_mut`] calls (which
    /// each need their own separate borrow of `self`), `HashMap::
    /// values_mut` safely hands out every entry's own distinct `&mut`
    /// simultaneously as a single streaming iterator, which is what lets
    /// callers like `GraphStack::all_huts_mut` return an `Iterator`
    /// instead of an eagerly-collected `Vec`.
    pub fn nodes_mut(&mut self) -> impl Iterator<Item = &mut (dyn Node<Env> + 'static)>
    where
        Env: 'static,
    {
        self.nodes.values_mut().map(|b| &mut **b)
    }

    /// Call `f` with mutable access to node `id` *and* the rest of the
    /// graph simultaneously — the same "temporarily remove, call, put
    /// back" technique [`Self::resolve_output`] already uses (see that
    /// method's doc comment), exposed generically here for any operation
    /// that needs to recurse into other nodes while mutating one (e.g.
    /// [`Node::resize_to_pixels`] cascading a resize down through a
    /// Tab/Tile node's children). `None` if `id` doesn't exist.
    pub fn with_node_mut<R>(&mut self, id: NodeId, f: impl FnOnce(&mut dyn Node<Env>, &mut Self) -> R) -> Option<R> {
        let mut node = self.nodes.remove(&id)?;
        let result = f(&mut *node, self);
        self.nodes.insert(id, node);
        Some(result)
    }

    /// Walk `id`'s [`Node::focused_child`] chain down to the actual
    /// focused leaf — the generic version of `Hut::focused_hut`'s
    /// recursive walk (see that trait method's own doc comment). Returns
    /// `id` itself if it's already a leaf (or unknown — nothing to walk
    /// down to).
    pub fn focused_leaf(&self, id: NodeId) -> NodeId {
        *self.focused_path(id).last().unwrap_or(&id)
    }

    /// Downcast node `id` to a concrete type `T` — a thin wrapper over
    /// [`Node::as_any`]/`downcast_ref`, for callers (`GraphStack`) that
    /// know a given id is (or should be) a specific node type, most
    /// commonly `ConsoleNode` for reaching real `ConsoleHut` state.
    /// `None` if `id` doesn't exist or isn't actually a `T`.
    pub fn downcast<T: 'static>(&self, id: NodeId) -> Option<&T> {
        self.node(id)?.as_any().downcast_ref::<T>()
    }

    pub fn downcast_mut<T: 'static>(&mut self, id: NodeId) -> Option<&mut T>
    where
        Env: 'static,
    {
        self.node_mut(id)?.as_any_mut().downcast_mut::<T>()
    }

    /// [`Self::focused_leaf`]'s full walk, `id` included — `id` itself,
    /// then each successive [`Node::focused_child`] in turn, ending at
    /// the leaf. Needed (not just the final leaf alone) by
    /// `GraphStack::wrap_tab`/`wrap_tile` (`graph_stack.rs`): wrapping
    /// "in place" means finding whichever node's own `children` list
    /// currently references the focused leaf — the second-to-last entry
    /// in this path — and replacing just that one entry, leaving every
    /// sibling/ancestor untouched. Always has at least one entry (`id`
    /// itself), even for an unknown id.
    pub fn focused_path(&self, id: NodeId) -> Vec<NodeId> {
        let mut path = vec![id];
        let mut current = id;
        while let Some(child) = self.node(current).and_then(|n| n.focused_child(self, current)) {
            path.push(child);
            current = child;
        }
        path
    }

    fn port_kind(&self, id: NodeId, name: &'static str, want_output: bool) -> Result<PortKind, GraphError> {
        let node = self.nodes.get(&id).ok_or(GraphError::UnknownNode(id))?;
        if want_output {
            node.outputs()
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.kind)
                .ok_or(GraphError::UnknownOutputPort { node: id, port: name })
        } else {
            node.inputs()
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.kind)
                .ok_or(GraphError::UnknownInputPort { node: id, port: name })
        }
    }

    // ---- Value ports (Content/Control) ----

    /// Link a `Content`/`Control`-kind input port to an upstream output
    /// of the same kind — replaces whatever was previously linked there,
    /// if anything.
    pub fn link(
        &mut self,
        from: NodeId,
        from_port: &'static str,
        to: NodeId,
        to_port: &'static str,
    ) -> Result<(), GraphError> {
        let output_kind = self.port_kind(from, from_port, true)?;
        let input_kind = self.port_kind(to, to_port, false)?;
        if !input_kind.is_value() {
            return Err(GraphError::NotAValuePort { node: to, port: to_port });
        }
        if !output_kind.is_value() {
            return Err(GraphError::NotAValuePort { node: from, port: from_port });
        }
        if input_kind != output_kind {
            return Err(GraphError::PortKindMismatch { input: input_kind, output: output_kind });
        }
        self.links.insert((to, to_port), (from, from_port));
        Ok(())
    }

    pub fn unlink(&mut self, to: NodeId, to_port: &'static str) {
        self.links.remove(&(to, to_port));
    }

    /// Clears the per-frame memoization cache — call once before each
    /// real resolve pass (a render frame, a hit-test pass, ...) so stale
    /// values from a previous pass are never reused across frames.
    pub fn begin_frame(&mut self) {
        self.cache.clear();
    }

    /// Resolve `id`'s output `port`, memoized within the current frame
    /// (see [`Self::begin_frame`]). Temporarily removes the node from
    /// `self.nodes` for the duration of its own `resolve` call — the
    /// same "move it out, need a placeholder in between" technique
    /// `Hut::wrap_focused`'s doc comment already uses elsewhere in this
    /// codebase — so that call can freely recurse into any *other* node
    /// via `graph.resolve_output`/`resolve_input` without a `&mut`
    /// aliasing conflict against its own slot.
    pub fn resolve_output(&mut self, id: NodeId, port: &'static str) -> Option<PortValue> {
        if let Some(value) = self.cache.get(&(id, port)) {
            return Some(value.clone());
        }
        let mut node = self.nodes.remove(&id)?;
        let value = node.resolve(self, id, port);
        self.nodes.insert(id, node);
        self.cache.insert((id, port), value.clone());
        Some(value)
    }

    /// Resolve whatever's linked to a single-valued input port, if
    /// anything — `None` if nothing's linked there (e.g. an optional
    /// port like a Wayland-client Hut's `terminal` input, unlinked for a
    /// client with no owning terminal).
    pub fn resolve_input(&mut self, id: NodeId, port: &'static str) -> Option<PortValue> {
        let &(from, from_port) = self.links.get(&(id, port))?;
        self.resolve_output(from, from_port)
    }

    // ---- Reference ports (Hut/HutList) ----

    /// Link a `Hut`-kind input port to reference another node — replaces
    /// whatever was previously referenced there, if anything (e.g.
    /// re-pointing a Main-Window Hut's `main` input when the active tab
    /// changes). No kind-check against `target` itself: a Hut reference
    /// can point at any node in the graph, unlike a value link.
    pub fn link_hut(&mut self, to: NodeId, to_port: &'static str, target: NodeId) -> Result<(), GraphError> {
        let input_kind = self.port_kind(to, to_port, false)?;
        if input_kind != PortKind::Hut {
            return Err(GraphError::NotAReferencePort { node: to, port: to_port });
        }
        self.hut_refs.insert((to, to_port), target);
        Ok(())
    }

    pub fn unlink_hut(&mut self, to: NodeId, to_port: &'static str) {
        self.hut_refs.remove(&(to, to_port));
    }

    pub fn hut_input(&self, id: NodeId, port: &'static str) -> Option<NodeId> {
        self.hut_refs.get(&(id, port)).copied()
    }

    /// Atomically replace every node referenced by a `HutList`-kind input
    /// port, in order — the list-reference equivalent of
    /// [`Self::link_hut`]. An empty `targets` clears the port entirely
    /// (e.g. a Tab-Hut with no children left, mid-collapse).
    pub fn set_hut_list(
        &mut self,
        to: NodeId,
        to_port: &'static str,
        targets: Vec<NodeId>,
    ) -> Result<(), GraphError> {
        let input_kind = self.port_kind(to, to_port, false)?;
        if input_kind != PortKind::HutList {
            return Err(GraphError::WrongReferenceArity { node: to, port: to_port, expected: input_kind });
        }
        self.hut_list_refs.insert((to, to_port), targets);
        Ok(())
    }

    /// Every node referenced by a `HutList`-kind input port, in order —
    /// empty if nothing's linked.
    pub fn hut_list_input(&self, id: NodeId, port: &'static str) -> Vec<NodeId> {
        self.hut_list_refs.get(&(id, port)).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pure control leaf — no inputs, one `Control` output — for
    /// exercising value linking/resolution without needing any real
    /// render-shaped node. Generic over `Env` (never touches it) — see
    /// this module's "Why `Graph<Env>`" doc section.
    struct ConstNode {
        value: f64,
        resolve_count: std::rc::Rc<std::cell::Cell<u32>>,
    }
    const CONST_OUTPUTS: &[OutputPort] = &[OutputPort { name: "value", kind: PortKind::Control }];
    impl<Env> Node<Env> for ConstNode {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn inputs(&self) -> &[InputPort] {
            &[]
        }
        fn outputs(&self) -> &[OutputPort] {
            CONST_OUTPUTS
        }
        fn resolve(&mut self, _graph: &mut Graph<Env>, _self_id: NodeId, _port: &'static str) -> PortValue {
            self.resolve_count.set(self.resolve_count.get() + 1);
            PortValue::Control(self.value)
        }
    }

    /// Doubles whatever's linked to its single-valued `input` port —
    /// exercises `link`/`unlink`/`resolve_input`.
    struct DoubleNode;
    const DOUBLE_INPUTS: &[InputPort] = &[InputPort { name: "input", kind: PortKind::Control }];
    const DOUBLE_OUTPUTS: &[OutputPort] = &[OutputPort { name: "output", kind: PortKind::Control }];
    impl<Env> Node<Env> for DoubleNode {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn inputs(&self) -> &[InputPort] {
            DOUBLE_INPUTS
        }
        fn outputs(&self) -> &[OutputPort] {
            DOUBLE_OUTPUTS
        }
        fn resolve(&mut self, graph: &mut Graph<Env>, self_id: NodeId, _port: &'static str) -> PortValue {
            match graph.resolve_input(self_id, "input") {
                Some(PortValue::Control(n)) => PortValue::Control(n * 2.0),
                _ => PortValue::Control(0.0),
            }
        }
    }

    /// A minimal Tab-Hut stand-in: a `children: HutList` reference input
    /// plus an `active` index, producing whichever child's own `value`
    /// output is currently active — exercises `set_hut_list`/
    /// `hut_list_input` and the real two-step "which child, then what
    /// does it show" shape `Node::resolve` is meant to support (see this
    /// module's doc comment).
    struct TabLikeNode {
        active: usize,
    }
    const TAB_INPUTS: &[InputPort] = &[InputPort { name: "children", kind: PortKind::HutList }];
    const TAB_OUTPUTS: &[OutputPort] = &[OutputPort { name: "value", kind: PortKind::Control }];
    impl<Env> Node<Env> for TabLikeNode {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn inputs(&self) -> &[InputPort] {
            TAB_INPUTS
        }
        fn outputs(&self) -> &[OutputPort] {
            TAB_OUTPUTS
        }
        fn resolve(&mut self, graph: &mut Graph<Env>, self_id: NodeId, _port: &'static str) -> PortValue {
            let children = graph.hut_list_input(self_id, "children");
            match children.get(self.active) {
                Some(&child) => graph.resolve_output(child, "value").unwrap_or(PortValue::Control(0.0)),
                None => PortValue::Control(0.0),
            }
        }
        fn focused_child(&self, graph: &Graph<Env>, self_id: NodeId) -> Option<NodeId> {
            graph.hut_list_input(self_id, "children").get(self.active).copied()
        }
    }

    #[test]
    fn link_feeds_a_single_valued_input_and_unlink_clears_it() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 3.0, resolve_count: Default::default() }));
        let d = graph.add_node(Box::new(DoubleNode));
        graph.link(a, "value", d, "input").unwrap();

        graph.begin_frame();
        assert!(matches!(graph.resolve_output(d, "output"), Some(PortValue::Control(n)) if n == 6.0));

        graph.unlink(d, "input");
        graph.begin_frame();
        assert!(matches!(graph.resolve_output(d, "output"), Some(PortValue::Control(n)) if n == 0.0));
    }

    #[test]
    fn relinking_a_single_valued_input_replaces_the_previous_source() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 3.0, resolve_count: Default::default() }));
        let b = graph.add_node(Box::new(ConstNode { value: 10.0, resolve_count: Default::default() }));
        let d = graph.add_node(Box::new(DoubleNode));
        graph.link(a, "value", d, "input").unwrap();
        graph.link(b, "value", d, "input").unwrap();

        graph.begin_frame();
        assert!(matches!(graph.resolve_output(d, "output"), Some(PortValue::Control(n)) if n == 20.0));
    }

    #[test]
    fn link_rejects_a_reference_kind_port() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 1.0, resolve_count: Default::default() }));
        let tab = graph.add_node(Box::new(TabLikeNode { active: 0 }));
        assert!(matches!(graph.link(a, "value", tab, "children"), Err(GraphError::NotAValuePort { .. })));
    }

    #[test]
    fn unknown_port_names_are_rejected() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 1.0, resolve_count: Default::default() }));
        let d = graph.add_node(Box::new(DoubleNode));
        assert!(matches!(
            graph.link(a, "nonexistent", d, "input"),
            Err(GraphError::UnknownOutputPort { .. })
        ));
    }

    #[test]
    fn tab_like_node_shows_whichever_child_is_active() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 1.0, resolve_count: Default::default() }));
        let b = graph.add_node(Box::new(ConstNode { value: 2.0, resolve_count: Default::default() }));
        let tab = graph.add_node(Box::new(TabLikeNode { active: 1 }));
        graph.set_hut_list(tab, "children", vec![a, b]).unwrap();

        graph.begin_frame();
        assert!(matches!(graph.resolve_output(tab, "value"), Some(PortValue::Control(n)) if n == 2.0));
    }

    #[test]
    fn downcast_reaches_the_concrete_node_type() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 7.0, resolve_count: Default::default() }));
        assert_eq!(graph.downcast::<ConstNode>(a).map(|n| n.value), Some(7.0));
        assert!(graph.downcast::<DoubleNode>(a).is_none(), "wrong type should downcast to None, not panic");

        graph.downcast_mut::<ConstNode>(a).unwrap().value = 9.0;
        assert_eq!(graph.downcast::<ConstNode>(a).map(|n| n.value), Some(9.0));
    }

    #[test]
    fn focused_leaf_walks_down_through_nested_focused_children() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 1.0, resolve_count: Default::default() }));
        let b = graph.add_node(Box::new(ConstNode { value: 2.0, resolve_count: Default::default() }));
        let inner_tab = graph.add_node(Box::new(TabLikeNode { active: 1 }));
        graph.set_hut_list(inner_tab, "children", vec![a, b]).unwrap();
        let outer_tab = graph.add_node(Box::new(TabLikeNode { active: 0 }));
        graph.set_hut_list(outer_tab, "children", vec![inner_tab]).unwrap();

        assert_eq!(graph.focused_leaf(outer_tab), b, "should walk through both levels to the real leaf");
        assert_eq!(graph.focused_leaf(a), a, "a leaf with no focused_child override is its own focused leaf");
    }

    #[test]
    fn with_node_mut_gives_simultaneous_access_to_the_node_and_the_rest_of_the_graph() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 5.0, resolve_count: Default::default() }));
        let tab = graph.add_node(Box::new(TabLikeNode { active: 0 }));
        graph.set_hut_list(tab, "children", vec![a]).unwrap();

        let children_seen_from_inside = graph.with_node_mut(tab, |_node, graph| {
            graph.hut_list_input(tab, "children")
        });
        assert_eq!(children_seen_from_inside, Some(vec![a]));
    }

    #[test]
    fn link_hut_feeds_a_single_reference_input_and_unlink_hut_clears_it() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 1.0, resolve_count: Default::default() }));
        let b = graph.add_node(Box::new(ConstNode { value: 2.0, resolve_count: Default::default() }));
        let tab = graph.add_node(Box::new(TabLikeNode { active: 0 }));
        // `TabLikeNode` only declares a `children` (HutList) input in
        // this test module — reusing it here as a single-reference `Hut`
        // port isn't representative of a real node's own port shape
        // (Main-Window's `main` port, for instance), just a convenient
        // stand-in so this test doesn't need a third synthetic node type
        // purely to exercise `link_hut`/`unlink_hut`/`hut_input` in
        // isolation from `set_hut_list`.
        graph.set_hut_list(tab, "children", vec![a]).unwrap();
        assert_eq!(graph.hut_list_input(tab, "children"), vec![a]);

        // Directly exercising the singular reference API against a node
        // whose *declared* port happens to be `HutList`-kind would be
        // rejected (kind mismatch) — so this checks `link_hut`/
        // `unlink_hut`/`hut_input` against a node with a real `Hut`-kind
        // input instead.
        struct MainLikeNode;
        const MAIN_INPUTS: &[InputPort] = &[InputPort { name: "main", kind: PortKind::Hut }];
        impl<Env> Node<Env> for MainLikeNode {
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
            fn inputs(&self) -> &[InputPort] {
                MAIN_INPUTS
            }
            fn outputs(&self) -> &[OutputPort] {
                &[]
            }
            fn resolve(&mut self, _graph: &mut Graph<Env>, _self_id: NodeId, _port: &'static str) -> PortValue {
                unreachable!("MainLikeNode has no outputs")
            }
        }
        let main_hut = graph.add_node(Box::new(MainLikeNode));
        graph.link_hut(main_hut, "main", a).unwrap();
        assert_eq!(graph.hut_input(main_hut, "main"), Some(a));
        graph.link_hut(main_hut, "main", b).unwrap();
        assert_eq!(graph.hut_input(main_hut, "main"), Some(b), "relinking replaces the previous reference");
        graph.unlink_hut(main_hut, "main");
        assert_eq!(graph.hut_input(main_hut, "main"), None);
    }

    #[test]
    fn link_hut_rejects_a_non_reference_port() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 1.0, resolve_count: Default::default() }));
        let d = graph.add_node(Box::new(DoubleNode));
        assert!(matches!(graph.link_hut(d, "input", a), Err(GraphError::NotAReferencePort { .. })));
    }

    #[test]
    fn set_hut_list_replaces_the_whole_list_atomically() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 1.0, resolve_count: Default::default() }));
        let tab = graph.add_node(Box::new(TabLikeNode { active: 0 }));
        graph.set_hut_list(tab, "children", vec![a]).unwrap();
        graph.set_hut_list(tab, "children", vec![]).unwrap();

        assert!(graph.hut_list_input(tab, "children").is_empty());
    }

    #[test]
    fn resolve_output_memoizes_within_one_frame() {
        let mut graph = Graph::new();
        let counter: std::rc::Rc<std::cell::Cell<u32>> = Default::default();
        let a = graph.add_node(Box::new(ConstNode { value: 5.0, resolve_count: counter.clone() }));

        graph.begin_frame();
        graph.resolve_output(a, "value");
        graph.resolve_output(a, "value");
        assert_eq!(counter.get(), 1, "second resolve within the same frame should hit the cache");

        graph.begin_frame();
        graph.resolve_output(a, "value");
        assert_eq!(counter.get(), 2, "a new frame should resolve again, not reuse the stale cache");
    }

    #[test]
    fn removing_a_node_prunes_links_and_references_pointing_at_or_from_it() {
        let mut graph = Graph::new();
        let a = graph.add_node(Box::new(ConstNode { value: 1.0, resolve_count: Default::default() }));
        let d = graph.add_node(Box::new(DoubleNode));
        let tab = graph.add_node(Box::new(TabLikeNode { active: 0 }));
        graph.link(a, "value", d, "input").unwrap();
        graph.set_hut_list(tab, "children", vec![a]).unwrap();

        graph.remove_node(a);

        graph.begin_frame();
        assert!(
            matches!(graph.resolve_output(d, "output"), Some(PortValue::Control(n)) if n == 0.0),
            "a value link sourced from a removed node should be pruned, not dangle"
        );
        assert!(
            graph.hut_list_input(tab, "children").is_empty(),
            "a Hut reference to a removed node should be pruned, not dangle"
        );
    }

    #[test]
    fn a_real_env_is_reachable_from_inside_resolve() {
        // Proves the actual mechanism migration step 3 depends on: a
        // node resolved inside a `Graph<Env>` for a non-`()` `Env` can
        // read `graph.env` from within its own `resolve` call. Doesn't
        // need anything renderer-shaped to prove this — a plain `i32`
        // env stands in for `RenderEnv<'_>` just as well.
        struct ReadsEnvNode;
        const ENV_OUTPUTS: &[OutputPort] = &[OutputPort { name: "value", kind: PortKind::Control }];
        impl Node<i32> for ReadsEnvNode {
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
            fn inputs(&self) -> &[InputPort] {
                &[]
            }
            fn outputs(&self) -> &[OutputPort] {
                ENV_OUTPUTS
            }
            fn resolve(&mut self, graph: &mut Graph<i32>, _self_id: NodeId, _port: &'static str) -> PortValue {
                PortValue::Control(graph.env as f64)
            }
        }
        let mut graph: Graph<i32> = Graph::with_env(42);
        let a = graph.add_node(Box::new(ReadsEnvNode));
        graph.begin_frame();
        assert!(matches!(graph.resolve_output(a, "value"), Some(PortValue::Control(n)) if n == 42.0));
    }
}
