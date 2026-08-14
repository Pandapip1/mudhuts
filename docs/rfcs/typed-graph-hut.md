# Typed graph Hut model

## Status

Drafted 2026-08-14, implementation starting the same day (user going AFK,
explicit "full build-out" scope — see the [[project_wishlist]] memory
entry this supersedes the shape of). Migrated incrementally, same
discipline as `composable-hut-hierarchy.md`: land one real, working,
build/clippy/test-verified step at a time, each its own commit, on the
`dag-hut-rearchitecture` branch (kept off `main` — `main`'s HEAD is what
the user's live nixos config pins by commit hash, and this touches far
more surface area than any single prior migration step did).

## Motivation

The current `Hut` tree (`hut.rs`) is a plain recursive enum —
`Console`/`Tab`/`Tile` — with each variant's children hardcoded to
exactly what that variant needs (a `Vec<Hut>`, a `Vec<(Hut, f64)>`). Every
new kind of composition (Main Window + minimized siblings, layer-shell
client layering, per-output display) needs either shoehorning into the
existing shape or a new bespoke variant with its own hand-written
traversal logic duplicated across `hut.rs`/`stack.rs`/`render.rs`. The
user's framing: model this the way PipeWire models its media graph —
nodes with typed input/output *ports*, links between them — so
composition rules are declared once (as port types) rather than
re-implemented per node kind, and so genuinely different node shapes
(a Tab-Hut's homogeneous list vs. a Main-Window-Hut's two differently-
typed inputs vs. a pure control node with no visual output at all) are
just different port declarations on the same underlying graph
abstraction, not special cases needing their own enum variant and their
own recursive-traversal method on every consumer.

Concretely, four use cases motivated this (the user's own examples,
verbatim from the design conversation):

1. **Tab/Tile** — a Tab-Hut or Tile-Hut takes a single *list* of Huts as
   input, shows/arranges them, produces one composited output. Matches
   today's shape exactly — the easiest node to port first, and the
   proof that the graph model can express what the enum already does
   before it's asked to express what the enum *can't*.
2. **Terminal ↔ Wayland-client** — a terminal Hut is linked as an input
   to its corresponding Wayland-client Hut (the client's own input port
   typed to accept specifically a Console/Terminal-typed producer). The
   user chose **real content flow**, not a bare ownership tag: a
   Wayland-client Hut's own displayed output is "whichever of {its own
   client surface, its linked terminal's output} is currently selected"
   — the same terminal/Main-Window toggle behavior `ConsoleHut::
   showing_terminal` drives today, now expressed as a real pass-through
   port on the client node itself rather than an internal boolean flag
   on a bundling struct.
3. **Main-Window Hut** — accepts one Hut on a `main` input port (shown
   full-size) and a separate list of Huts on a `minimized` input port
   (rendered as edge-docked handles, not composited — replacing
   `docks.rs`'s existing `FloatingWindow`/`Dock::Docked` concept with a
   real port rather than a per-struct enum field).
4. **Layer-Shell Hut** — one Hut on a `display` input port (whatever's
   "underneath" — today's whole Stack/Tab/Tile content) plus a list of
   layer-shell client Huts on a `layers` input port, replacing today's
   global `layer_map_for_output` + `space_render_elements` consolidation
   with a real graph node. Per the user's own follow-up: **one per
   output**, not one globally — see Output Hut below, and multi-monitor.

Plus, mid-design, two more generalizing requirements the user added:

5. **Non-render/control nodes** — a Hut doesn't have to produce
   anything visual at all. The user's own example: a Hut that controls a
   monitor's refresh rate — takes some input (a target Hz, itself
   perhaps produced by a settings/config node) and has *no* rendered
   output, just a side effect. This is why ports are typed generically
   (an enum of possible port *kinds* — texture-producing, Hut-list,
   single-Hut, scalar/control values — not hardcoded to "renders a
   texture").
6. **Every Hut declares both inputs and outputs**, not just inputs — a
   node's own *output* port(s) are exactly what a downstream node's
   input port can link to. A leaf Console/Terminal Hut has zero inputs
   and one texture-producing output. A pure sink (see Output Hut below)
   has an input and *no* output at all — nothing downstream of a
   physical monitor.

And a spontaneous but load-bearing insight during design: making a
physical **Output** (monitor) itself a Hut — an input-only, no-output
sink node — turns "attach a Hut subtree to a specific monitor" into
just another graph link, which is what makes real multi-monitor support
fall out of this model almost for free rather than needing its own
separate architecture pass (see the dedicated section below). The user
explicitly chose to build real multi-output hardware support (not just
a graph-ready single-output stub) as part of this same effort.

## Prior art already in this codebase

- `redraw::Signal<T>` — a generic wrapper that already treats "changing
  this value should trigger a redraw" as a cross-cutting concern
  attachable to any field, not hardcoded per struct. The graph core
  reuses this exact idea for port values.
- `redraw::Redrawable`/`HitTestable` — traits already factored out
  because `hut.rs`'s tree isn't the only thing that needs redraw/hit-test
  dispatch (chrome, docks, the switcher popup all implement them too).
  The graph model generalizes the same instinct one level further: don't
  just factor out *capabilities* across unrelated types, factor out the
  *composition shape* itself.
- `HutSpaceElement`/`space_element.rs` — already a real example of "a
  Hut's content, and a layer-shell surface, unified as one renderable
  element type" — the graph core's texture/render-element port type is
  this same unification, generalized to any node, not just what a
  `Space<HutSpaceElement>` already covers.

## Core types (`graph.rs`, new module)

```rust
/// A value flowing across one link — the thing a port actually carries.
/// Deliberately not generic-typed at the Rust level (`Node<In, Out>` per
/// node kind was considered and rejected — see "Why not generics"
/// below): a fixed, closed enum of every port *kind* the compositor
/// actually needs, checked at link-construction time instead of by the
/// Rust type system.
pub enum PortValue {
    /// A single composited texture this frame — what a leaf (Console/
    /// Terminal, a Wayland client's own surface) or a compositing node
    /// (Tab/Tile/Main-Window/Layer-Shell) produces as its rendered
    /// output.
    Content(RenderedContent),
    /// A single upstream Hut — Main-Window's `main` port, a Wayland-
    /// client Hut's terminal-passthrough port.
    Hut(HutRef),
    /// An ordered list of upstream Huts — Tab/Tile's children, Main-
    /// Window's `minimized` list, Layer-Shell's `layers` list.
    HutList(Vec<HutRef>),
    /// A plain control/scalar value — the refresh-rate Hut's target Hz,
    /// or any future settings-style node. `f64` covers every real
    /// control value this compositor has needed so far (Hz, a scale
    /// factor, a percentage); a richer typed-control-value enum is a
    /// natural follow-up once a second real shape (not just a number)
    /// actually shows up — not designed further ahead of that need, same
    /// reasoning `redraw::Hit`'s doc comment already gives for staying
    /// deliberately minimal until a real caller needs more.
    Control(f64),
}

/// What a node declares it needs/produces — checked when a link is
/// constructed (`Graph::link`), not per-frame: a node's shape doesn't
/// change after construction (adding e.g. a new Tab-Hut child creates a
/// new link, doesn't retype an existing port).
pub enum PortKind {
    Content,
    Hut,
    HutList,
    Control,
}

pub struct InputPort {
    pub name: &'static str,
    pub kind: PortKind,
}

pub struct OutputPort {
    pub name: &'static str,
    pub kind: PortKind,
}
```

`Node` — the trait every Hut kind implements (Console/Terminal,
Wayland-client, Tab, Tile, Main-Window, Layer-Shell, Output-sink,
refresh-rate control, ...):

```rust
pub trait Node {
    fn inputs(&self) -> &[InputPort];
    fn outputs(&self) -> &[OutputPort];

    /// Pull this node's current value for one of its own output ports —
    /// `Graph::resolve` calls this after first resolving every linked
    /// input (depth-first, matching how `Hut::focused_hut`'s recursive
    /// walk already works today). A render-shaped node reaches into
    /// `renderer`/`scale` the same way `ConsoleHut::redraw`/
    /// `content_elements` already do; a control node (refresh-rate)
    /// ignores them and just returns/applies its `Control` value.
    fn resolve(&mut self, ctx: &mut ResolveCtx, port: &str) -> PortValue;
}
```

`Graph` owns every node plus the link table (`HashMap<(NodeId, &'static
str), (NodeId, &'static str)>` — downstream input port to upstream
output port) and topologically resolves whatever a caller asks for,
memoizing within one resolve pass (a diamond — e.g. two different
Tab-Hut children both reachable from the same shared Console-Hut leaf,
which can't happen in today's strict tree but *can* once linking is
general — must only resolve that shared node once per frame, not twice).

### Why not generics (`Node<In, Out>` per node kind)

Considered and rejected: Rust generics would give compile-time port-type
checking, but every consumer that needs to walk the graph generically
(the redraw loop, hit-testing, `switcher.rs`'s thumbnail iteration, the
render pass) would need to be generic over every concrete node type it
might encounter, or the graph would need to be a `Vec<Box<dyn Node<???>>>`
with the `In`/`Out` type parameters erased anyway — at which point the
compile-time checking has nowhere left to attach. A closed runtime
`PortValue`/`PortKind` enum, checked once at `Graph::link` time (link
construction is the only place a type mismatch could ever be introduced,
since a node's own declared ports don't change after construction), gets
correctness at the one point that actually matters without forcing every
generic call site into dynamic dispatch anyway. This mirrors why
`Hut::Console`/`Tab`/`Tile` was already a runtime enum rather than three
unrelated Rust types in the first place.

## Node types and their ports

| Node | Inputs | Outputs |
|---|---|---|
| **Console/Terminal Hut** | — (leaf) | `content: Content` |
| **Wayland-client Hut** | `terminal: Hut` (optional — a client not spawned from a mudhuts terminal, e.g. an autostarted app, has none linked) | `content: Content` (own surface, or pass-through to `terminal`'s `content` while toggled to terminal view — see below) |
| **Tab-Hut** | `children: HutList` | `content: Content` (whichever child is `active`) |
| **Tile-Hut** | `children: HutList` | `content: Content` (every child, tiled) |
| **Main-Window Hut** | `main: Hut`, `minimized: HutList` | `content: Content` (`main`'s content plus docked handles for `minimized`) |
| **Layer-Shell Hut** | `display: Hut`, `layers: HutList` | `content: Content` |
| **Output Hut** (sink) | `display: Hut` | — (none — a physical monitor has nothing downstream of it) |
| **Refresh-Rate Hut** (control, example of a non-render node) | `target: Control` | — (applies the value as a side effect against the real output's mode, same DRM property `render_surface`'s `FrameFlags`/mode-setting already touches) |

The **terminal/Wayland-client pass-through** is the concrete mechanism
behind requirement 2 above: `WaylandClientHut::resolve(ctx, "content")`
returns its own client surface's texture normally; while its owning
`ConsoleHut`-equivalent's `showing_terminal`-style toggle is set, it
instead calls `ctx.resolve(self.terminal_input, "content")` and returns
*that* — a real graph traversal each frame, not a cached copy, so the
terminal's own live content (cursor blink, new output) is always
current when toggled into view. This subsumes `ConsoleHut::
showing_terminal`/`active_main_window`'s current role entirely once the
migration reaches this node type (see Migration below) — those `Signal`
fields move from `ConsoleHut` onto the relevant node/link, they don't
disappear.

## Multi-monitor

`state.output: Option<Output>` (singular) becomes `state.outputs:
Vec<OutputHut>`, one per connected connector — `udev_backend.rs`'s
`connector_connected`/`connector_disconnected` already iterate real
per-crtc `SurfaceData` entries in a `HashMap<crtc::Handle, SurfaceData>`
and already run `render_surface` per-crtc in a loop (`redraw_ping_source`'s
handler collects every known crtc and calls `render_surface` for each) —
the single-output assumption turns out to be concentrated almost
entirely in `state.output` itself being singular plus `render::
build_frame_elements`/`usable_area`/`active_pane_offset` all implicitly
reasoning about "the" output rather than a specific one. Real
multi-CRTC scanning/mode-setting is *already* structurally there; what's
missing is (a) `state.output`/`state.output_size`/`usable_area()` all
becoming per-output, and (b) something deciding which Hut subtree feeds
which `OutputHut`'s `display` input.

For (b): each `OutputHut` starts linked to a *new, independent*
top-level Hut (its own fresh `MruStackHut`-equivalent root) by default
on connect — i.e. each monitor gets its own independent Alt-Tab
workspace, matching the common multi-monitor default (extended desktop,
not mirrored) — but nothing in the graph *prevents* linking the same
root to two `OutputHut`s (real mirroring, for free, as a link
operation, not a new feature) later. `MruStackHut` itself needs to
become per-`OutputHut` rather than a single field on `State` — this is
the single largest blast-radius change in the whole migration (every
`state.stack.*` call site in `input.rs`/`handlers/*.rs`/`render.rs`
currently assumes one global stack) and is scoped as its own late
migration step, after every node type exists and the single-output path
still works end-to-end on the graph model.

Winit backend (`winit_backend.rs`) stays genuinely single-output — a
nested dev/test window has no real second monitor to speak of — so it
only ever creates one `OutputHut`, same as today.

## Migration plan (incremental, one verified step at a time)

1. **Graph/port core** (`graph.rs`) — `PortValue`/`PortKind`/`Node`/
   `Graph`, unit-tested against synthetic nodes only (no real Hut
   integration yet). *No behavior change to the running compositor.*
2. **Tab/Tile onto the graph**, alongside the existing enum (not
   replacing it yet) — prove `TabbedHut`/`TileHut` can be expressed as
   `Node` impls producing identical output to today's
   `Hut::Tab`/`Hut::Tile` handling, verified by literally running both
   paths against the same fixture in a test and asserting equal
   results. Still no change to `main.rs`/`render.rs`'s real call paths.
3. **Console/Terminal + Wayland-client onto the graph**, including the
   real content-flow pass-through port — this is where `ConsoleHut::
   showing_terminal`/`main_windows`/`active_main_window` actually start
   moving off the bundling struct and onto real graph nodes/links.
4. **Cut over the real render/input/hit-test call paths** to walk the
   graph instead of the old enum — the actual "flip the switch" step;
   everything before this is provably-equivalent groundwork. The old
   `hut.rs` enum is deleted only once every call site has moved (per
   the user's chosen incremental-replace style) and the full test suite
   plus a live `mudhuts --tty` check both pass.
5. **Main-Window Hut** (`main`/`minimized` ports) — replaces
   `main_window.rs`'s `MainWindowEntry`/`FloatingWindow`/`Dock` structs.
6. **Layer-Shell Hut** (`display`/`layers` ports, one per `OutputHut`) —
   replaces the global `layer_map_for_output` consolidation.
7. **Output Hut + real multi-monitor** — `state.outputs: Vec<OutputHut>`,
   per-output `MruStackHut`, per-output `usable_area`/render pass. Also:
   a first real non-render control node (refresh-rate) as the proof
   that requirement 5 actually works end to end, not just on paper.

Each step lands as its own commit, `cargo build`/`clippy`/`test --workspace`
clean before the next begins. Steps 5-7 are real architecture work on
the order of steps 1-4 combined — if this session's time runs out before
reaching them, they're fully specified here and safe to pick up in a
later session exactly where this doc leaves off, same as the original
composable-Hut-hierarchy RFC's own multi-session history.

## Mutable vs. immutable links

Raised by the user mid-implementation, ahead of when it'll actually
matter: not every reference link is meant to be rewired at runtime. A
Wayland-client Hut's `terminal` input is fixed at spawn time — "the
console that launched it" never changes for the life of that client,
the same way `ownership.rs`'s PID-ancestry resolution is a one-time
decision made when the client first connects, today. Other links are
*designed* to change — a Tab-Hut's `children` list grows/shrinks as tabs
open and close, a Main-Window Hut's `main` input is repointed every time
the active tab changes.

The graph core itself doesn't need to (and doesn't) distinguish these —
`Graph::link`/`link_hut`/`set_hut_list` are uniformly "set (or replace)
this link," and nothing stops calling code from simply never calling
them again on a given port after construction. The distinction only
becomes real once there's a **management interface** — something that
lets a user (or a future settings UI) rewire arbitrary links themselves
— which needs to know which ports are safe to expose as rewirable and
which aren't (repointing a client's `terminal` input at runtime would be
actively wrong, not just unusual). That's a real constraint on that
future interface's own design, not on `graph.rs`'s core API — tracked
here so it isn't lost, not implemented now (no management interface
exists yet to constrain).

## Explicitly out of scope

- Retyping `Control` beyond a bare `f64` until a second real control
  shape needs it (see `PortValue::Control`'s doc comment above).
- A generic user-facing graph *editor* (letting the user rewire links
  themselves via config/UI) — every link this RFC describes is
  constructed in Rust by the relevant spawn/wrap code, the same way
  `wrap_tab`/`wrap_tile` construct `Hut::Tab`/`Hut::Tile` today. Nothing
  here requires exposing the graph shape to the user directly, and nothing
  in the design prevents adding that later if wanted.
- GPU-level compositing changes — `PortValue::Content`'s `RenderedContent`
  is the same texture-plus-damage-snapshot shape `content_elements`/
  `composite_normal_content` already produce; this RFC changes *how
  nodes are wired together*, not how a texture actually gets rendered.
