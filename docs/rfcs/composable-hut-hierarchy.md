# RFC: Composable Hut Hierarchy

Status: draft, for discussion. No code changes in this RFC itself — migration steps 1–3 (see
"Migration Strategy") have since landed in the actual codebase, each as its own separate commit.

Companion doc: `/home/gavin/.claude/plans/cryptic-honking-lamport.md`, "Composable Hut
hierarchy" wishlist entry (2026-08-13) — this RFC answers the three open questions that entry
flagged as needing a real design pass before any code changes, plus the cross-cutting
redraw/input scope note attached to it.

## Summary

mudhuts today composites its screen through five independently hand-rolled paths: `HutStack`'s
MRU list (`mudhuts/src/stack.rs`), `Village`'s Tab/Tile recursion (`mudhuts/src/village.rs`,
`mudhuts/src/village_chrome.rs`), a `Hut`'s own Main-Window tab strip (`mudhuts/src/hut.rs`,
`mudhuts/src/chrome.rs`), layer-shell surfaces (`mudhuts/src/handlers/layer_shell.rs`, wired
into `mudhuts/src/render.rs`), and the Alt-Tab switcher popup (`mudhuts/src/switcher.rs`) plus
dock handles (`mudhuts/src/docks.rs`). The proposed redesign (already agreed with the user,
captured in the plan doc) replaces the first three with one recursive `Hut` tree, renaming
today's `Village` → `Hut` and today's `Hut` → `Console Hut`.

This RFC's recommendations, in brief:

1. **Space coexistence (Q1)**: don't bypass `Space` — multiply it. Give every Hut-tree node
   that composites real Wayland windows (a Console Hut's own Tabbed-Hut-of-Main-Windows) its
   own private `Space<HutSpaceElement>`, bound to a synthetic, non-hardware `Output` sized to
   that node's own render target. `HutSpaceElement` is a small enum wrapping either a real
   `Window` (a leaf Sub-Hut) or a pre-composited child texture, so `space_render_elements` can
   still do the compositing work generically at every level, not just the currently-privileged
   "Main-Window-visible" branch. This is grounded in Smithay's actual `Space`/`Output` design
   (confirmed by reading the pinned Smithay source, not assumed): a `Space` is a plain
   `Vec`-backed container keyed by its own id, an `Output` is a generic, backend-agnostic struct
   with no hardware coupling, and popup grabs / `PointerGrab` / `LayerMap` have zero dependency
   on there being one privileged `Space` — see the "Q1" section for citations.

2. **Layer-shell collapse (Q2)**: a "Layer-Shell Root Hut" node whose own composite step is one
   `space_render_elements` call against the *real* output, passing whatever `Space` the "normal
   content" child (the MRU Stack Hut) produced. Because `space_render_elements` pulls
   `layer_map_for_output` unconditionally regardless of what's mapped in the `Space` passed to
   it, this single call reuses Smithay's existing anchor/margin/exclusive-zone/keyboard-
   interactivity machinery for free and genuinely replaces all three of today's paths, not just
   two of them.

3. **Hit-test duplication (Q3)**: a recursive `surface_under`-style trait method does eliminate
   the specific duplication flagged in the task (today's three independent walks of
   `usable_area()` + `pane_rects()` in `state.rs`, `render.rs`, `input.rs`) — but only if each
   node kind's "where are my children" rect computation is factored into one function shared by
   both its `render()` and its `hit_test()` implementation, the way `village_chrome.rs`'s
   `level_layout` and `chrome.rs`'s `tab_layout` already (correctly) do today at the single-node
   level. The redesign doesn't invent this pattern; it generalizes it to also cover
   *cross-level* composition, which is where today's actual duplication lives.

4. **Cross-cutting redraw/input (required additional scope)**: two small opt-in traits,
   `Redrawable` (a cheap handle for "mark my content dirty" that owns a clone of the existing
   `Ping`-based redraw mechanism) and `HitTestable` (`hit_test(&self, point) -> Option<Hit>`),
   neither requiring implementors to be Hut nodes. A Hut node implements both; chrome (tab
   strips, dock handles, the switcher popup) implements them too, without becoming part of the
   tree — satisfying the user's explicit constraint that chrome doesn't have to be modeled as
   Hut nodes.

Migration assessment: the *rename* must be one atomic, tooling-assisted, behavior-preserving
commit (not incremental — the meaning-swap makes a staged rename actively dangerous). The
*architectural* change can and should be incremental, proving out the new primitives (the two
traits, per-node `Space`) against low-blast-radius targets (dock handles, the switcher) before
touching `Village`/`HutStack` themselves — but the actual `Village` → generic tree swap is
inherently one large step, because today's `Tab`/`Tile` match arms are inlined across
`stack.rs`, `render.rs`, `input.rs`, and `village_chrome.rs` simultaneously. See "Migration
Strategy" for the full staged plan.

## Background / Motivation

See the plan doc's "Composable Hut hierarchy" section for the full motivation and the tree
shape already agreed with the user:

```
Layer-Shell Root Hut
├─ [Background/Bottom layer-shell client Sub-Huts]
├─ MRU Stack Hut
│   ├─ Console Hut
│   │   └─ Tiling Hut | Tabbed Hut
│   │       └─ Sub-Hut(s) (the actual Wayland windows)
│   ├─ Tile Hut
│   │   ├─ Console Hut (as above)
│   │   └─ Tab Hut
│   │       ├─ Console Hut (as above)
│   │       └─ Console Hut (as above)
│   └─ ...
└─ [Top/Overlay layer-shell client Sub-Huts]
```

This RFC takes that shape as given and works out the three open technical questions the plan
doc flagged as blocking any code changes, plus the cross-cutting redraw/input scope the user
added on 2026-08-13. It does not revisit the rename or the tree shape itself except where a
concrete technical finding suggests a flagged (not silent) change — see "Naming collision:
Sub-Hut vs. Sub-Window" under the `MainWindowEntry` section.

## Grounding note on Smithay internals

Several of the proposals below depend on facts about Smithay's `Space`, `LayerMap`, and popup/
pointer-grab machinery that aren't visible from mudhuts' own source alone. These were verified
against the actual pinned Smithay checkout
(`/home/gavin/.cargo/git/checkouts/smithay-312425d48e59d8c8/4cf0b62/`, the exact revision
`mudhuts/Cargo.toml` pins), not assumed from a differently-versioned mental model:

- `Space<E>` (`src/desktop/space/mod.rs:51-57`) is `{ id, elements, outputs, span }` — each
  instance gets its own id (`space_id::next()`, `mod.rs:76`), removed on `Drop`
  (`mod.rs:66-71`). Nothing about it assumes a singleton; multiple independent `Space`s can
  coexist in one process.
- `Space::map_output` (`mod.rs:354-361`) records the output's location *for that space's id* in
  the `Output`'s own `UserDataMap` (`src/desktop/space/output.rs:8-40`,
  `OutputUserdata = Mutex<HashMap<usize, Point<i32, Logical>>>`) — so the *same* `Output` object
  can be mapped into several different `Space`s simultaneously, each with its own location for
  it.
- `space_render_elements` (`mod.rs:674-755`) unconditionally calls `layer_map_for_output(output)`
  (`mod.rs:696`) regardless of whether any `Space` passed to it has that output mapped —
  layer-shell rendering is entirely independent of `Space` membership.
- `Output::new` (`src/output.rs:261`) has no coupling to real backend hardware; mode/scale/
  transform are just stored state set via `change_current_state`. A synthetic `Output`
  representing an arbitrary rectangle (e.g. one tile pane's pixel size) is exactly as valid an
  input to `space_render_elements`/`LayerMap::arrange` as a real monitor's `Output` — element
  positions come out relative to whatever that `Output`'s geometry says, per `output_geo.loc`
  subtraction at `mod.rs:560,727-730`. The only side effect to be aware of is `wl_surface.enter`/
  `leave` being sent for that (never-globalized, so harmless) output.
- `LayerMap` is stored per-`Output` via `insert_if_missing_threadsafe` on that `Output`'s own
  `UserDataMap` (`src/desktop/wayland/layer.rs:50-71`) — a real per-instance map, not a global
  registry — and `LayerMap::arrange()` derives its working rect purely from
  `output.current_mode()/current_scale()/current_transform()` (`layer.rs:256-268`), with the
  same "works identically against a synthetic Output" property.
- `PopupManager`/`PopupGrab` (`src/desktop/wayland/popup/manager.rs`,
  `.../popup/grab.rs`) have **no** references to `Space` at all — `grab_popup` takes only a
  `KeyboardFocus`/`PopupKind`/`Seat`/`Serial`, and `find_popup_root_surface`
  (`manager.rs:219-235`) walks the `WlSurface` parent/role chain directly. mudhuts' own
  `unconstrain_popup` (`mudhuts/src/handlers/xdg_shell.rs:324-351`) is application code, not a
  Smithay requirement — it happens to use `self.space.outputs_for_element`/`output_geometry`
  today, but nothing forces that.
- `PointerGrab` (`src/input/pointer/grab.rs`) has no `Space` dependency either — it operates on
  whatever `(surface, point)` focus the caller hands it, agnostic to how that focus was
  determined.

The practical upshot: nothing about Smithay's own design privileges mudhuts' current single
`state.space: Space<Window>` as *the* window database. It's free to become one `Space` per
compositing scope in the new tree, and popups/grabs/hit-testing don't care which scheme is
used.

## Question 1: Per-node texture-compositing vs. Smithay's `Space`-based mapping

### Today's actual behavior

`state.space: Space<Window>` (`mudhuts/src/state.rs:59`) is not a general window database — it
is rebuilt from scratch on every visibility change by `State::sync_visible_main_window`
(`state.rs:589-617`): unmap everything, then map back only the focused Console Hut's active
Main Window plus its floating Sub-Windows and Alerts, each positioned at literal
output-relative coordinates. It is empty whenever the terminal is the visible view or a
Tile-Village is focused.

`render.rs`'s `build_frame_elements` (`render.rs:253-372`) has three content branches:
- Tile-Village (`render.rs:298-304`): bypasses `Space` entirely, calls `build_tile_elements`
  (`render.rs:387-457`), which manually calls `hut.redraw(renderer)` per pane and constructs
  `TextureRenderElement`s by hand.
- Terminal-visible (`render.rs:330-361`): also bypasses `Space`, manually calls
  `layer_elements` (`render.rs:158-190`) to replicate the Background/Bottom-vs-Top/Overlay split
  Smithay's own `space_render_elements` already does automatically, then pushes one terminal
  texture element by hand.
- Main-Window-visible (`render.rs:362-369`): the only branch that calls
  `space_render_elements::<_, Window, _>(renderer, [&state.space], output, 1.0)` — gets
  layer-shell splitting *and* window compositing for free, per that function's own doc comment
  (quoted in `render.rs:150-157`: "this will include layer-shell surfaces added to this
  output's LayerMap").

So today, exactly one of three render branches gets Smithay's automatic layer-splitting +
compositing; the other two hand-roll a subset of the same behavior. This is precisely the
duplication Q2 asks about, and it's also the reason Q1 is hard: a naive "each Hut renders to
its own texture, parent composites" model has no obvious place to keep using `Space` at all,
since `Space`'s render helper wants real mapped `Window`s, not opaque textures.

### Proposal: one `Space` per node that composites real Wayland windows, generalized via a small element enum

Rather than either (a) keeping exactly one shared `Space<Window>` (which is what breaks down
the moment two Console Huts need to be visible/rendered simultaneously — Tile-Village's "every
pane shows a Main Window" follow-up the plan doc flags as deliberately out of v1 scope,
`village.rs:10-21`) or (b) bypassing `Space` everywhere and hand-rolling every surface-tree walk
the way `layer_elements`/`build_tile_elements` already do, define:

```rust
/// What a per-scope Space can hold: a real Wayland window (a leaf Sub-Hut),
/// or another Hut node's own already-composited output, wrapped so it can
/// sit in the same Space as real windows at a specific position/z-order.
enum HutSpaceElement {
    Window(Window),
    Composited(CompositedTexture), // implements SpaceElement + AsRenderElements
}
```

Every Hut-tree node whose children are genuinely simultaneous on screen (a Tabbed-Hut's single
active child, a Tiling-Hut's several side-by-side children, the MRU Stack Hut's single focused
child, the Layer-Shell Root Hut's layered children) owns its own `Space<HutSpaceElement>`,
mapped against a synthetic `Output` sized to that node's own render target — real for the
Layer-Shell Root Hut (the actual monitor), synthetic for everything nested inside it (a tile
pane's own pixel rect, a Console Hut's own content rect below its tab strip, etc.). A node's
`render()` step:
1. Ensures every child that should currently be visible is mapped into its own `Space` at the
   right local-coordinate position (leaf Sub-Huts map their real `Window` directly; non-leaf
   children map their own most-recent composited texture, produced by recursing into that
   child's own `render()` first).
2. Calls `space_render_elements` (or, for the Layer-Shell Root Hut specifically, the real-output
   variant that also picks up `layer_map_for_output`) once against its own `Space`.
3. For an intermediate node, the result gets composited into an offscreen FBO-backed texture
   (via the same GLES renderer, bound to that texture instead of the swapchain) and handed up
   to its own parent as that node's `Composited` element for *this* frame.

This directly generalizes today's privileged Main-Window-visible branch to every branch: the
terminal-visible case becomes "this Console Hut's Tabbed-Hut has zero mapped `Window`
elements, and its terminal texture is pushed as a `HutSpaceElement::Composited` sibling
alongside them" (rather than the current bespoke `layer_elements` + manual texture push); the
Tile-Village case becomes "each pane's Console Hut composites independently into its own
texture, and the Tiling-Hut maps each pane's result as a `Composited` element at that pane's
`pane_rects` position" (reusing `village.rs::pane_rects`, `village.rs:389-420`, exactly as
today, just as *positioning input to a Space* rather than *positioning input to hand-built
`TextureRenderElement`s*).

### What this costs / what it fixes

- **Popups keep working through the same machinery they use today** — confirmed above, popup
  grab/positioning has zero `Space` dependency, so nothing about scoping `Space` per node
  threatens it. The one real, pre-existing gap is that `handlers/xdg_shell.rs`'s
  `unconstrain_popup` (`xdg_shell.rs:324-351`) currently assumes "every Main Window is
  fullscreen at the output's origin... so its geometry is always just the output's — no
  per-window geometry lookup needed" (`xdg_shell.rs:328-334`). That assumption already breaks
  down for a Main Window inside a Tile-Village pane in the v2 scope the plan doc calls out
  (`village.rs:10-21`, "Main-Window-in-a-tile-pane is a tracked follow-up") — this RFC doesn't
  fix that (see Open Questions), but the per-node-`Space` design doesn't make it any worse: a
  popup's root-window lookup would need to walk to whichever Console Hut's own `Space` actually
  has it mapped, and use *that* node's local geometry instead of the real output's.
- **`Space::element_under`-based routing keeps working, scoped per node** — each node's own
  `hit_test` (Q3) calls `self.space.element_under(local_point)` against its own private
  `Space`, translating into that node's local coordinate frame first — this is a direct,
  mechanical generalization of `state.rs`'s existing `surface_under` (`state.rs:654-706`), not
  a new mechanism.
- **Cost**: every intermediate node needs its own FBO-backed render target and, when it maps
  real windows, its own synthetic `Output`/`Space`/`LayerMap` triple. For a deeply nested tree
  this is more GPU memory and per-frame Space bookkeeping than today's single `state.space`. The
  existing "Investigate high memory usage" wishlist entry (plan doc, near the end) is reason to
  treat this as a real, not hypothetical, cost — see Open Questions.
- **Risk, resolved**: `Space<HutSpaceElement>` requires `HutSpaceElement` to implement
  `SpaceElement` (geometry, z-order, opaque-region reporting, etc.) for the `Composited` texture
  variant. This RFC originally proposed the shape without prototyping the actual trait impl —
  since built and live-verified as migration step 3 (`mudhuts/src/hut_space.rs`); see Open
  Question 1's resolution.

## Question 2: Collapsing layer-shell's three compositing paths into one

### Today

Three genuinely separate implementations exist:
1. `space_render_elements`'s automatic behavior, used only in the Main-Window-visible branch
   (`render.rs:363-369`).
2. `layer_elements` (`render.rs:158-190`), a hand-rolled reimplementation of the same
   upper/lower (Top+Overlay vs. Background+Bottom) split — used by the terminal-visible branch
   (`render.rs:336`) and the Tile-Village branch (`render.rs:299`). Its own doc comment
   (`render.rs:144-157`) is explicit that it exists purely because those two branches "don't go
   through a `Space` at all."
3. Click/keyboard routing for layer surfaces is *also* separately hand-rolled, independent of
   both render paths above: `state.rs`'s `surface_under` (`state.rs:654-706`) and
   `input.rs`'s `try_click_layer_surface`/`exclusive_layer_surface`
   (`input.rs:318-372`) each re-derive the same Top/Overlay-then-content-then-Bottom/Background
   ordering by hand.

### Proposal: Layer-Shell Root Hut = one `space_render_elements` call against the real output

Given Q1's per-node-`Space` design, the Layer-Shell Root Hut is the *simplest* node in the whole
tree, not a fourth bespoke implementation: it owns the one `Space<HutSpaceElement>` that's
bound to the real, physical `Output` (every other node's `Space` is bound to a synthetic one).
Its "normal content" child (the MRU Stack Hut) is mapped into that `Space` as a single
`HutSpaceElement::Composited` element, sized to `State::usable_area()` (`state.rs:523-529`,
already exactly "the real output minus every layer surface's exclusive zone" — no change
needed there). The Layer-Shell Root Hut's entire `render()` body becomes:

```rust
self.space.map_element(self.normal_content_element(), usable_origin, true);
space_render_elements::<_, HutSpaceElement, _>(renderer, [&self.space], &real_output, 1.0)
```

That one call gets Background/Bottom-vs-Top/Overlay splitting automatically, for every content
branch uniformly — because from `space_render_elements`'s point of view there is no longer a
"terminal branch" or "Tile-Village branch," just whatever single composited texture the MRU
Stack Hut produced this frame. This literally deletes `layer_elements` (`render.rs:158-190`)
rather than generalizing it — it becomes dead code once every content branch produces its
composited output the same way.

For hit-testing, the Layer-Shell Root Hut's `hit_test` reuses the exact same ordering by
delegating to its own `Space::element_under` plus a direct `layer_map_for_output` check for the
z-order slots `Space::element_under` alone doesn't cover (Background/Bottom are *below*
whatever's mapped, so they only get checked as a fallback) — this is a direct port of
`state.rs::surface_under`'s existing logic (`state.rs:654-706`), just now living in one place
(the Layer-Shell Root Hut's `hit_test` impl) instead of being re-derived independently in
`input.rs::try_click_layer_surface`/`exclusive_layer_surface` (`input.rs:318-372`) as well.
`exclusive_layer_surface`'s specific check (`input.rs:362-372`, an `exclusive`
keyboard-interactivity Top/Overlay surface must win over even mudhuts' global keybindings) stays
a small piece of policy layered on top of the Layer-Shell Root Hut's hit-test result — not
itself part of the generic tree, since it's a keyboard-routing rule, not a spatial hit-test.

### What this costs / risks

- **Real gain, not just relocation**: unlike Q3 (see below), this collapses from 3 independent
  implementations to 1 for both render *and* hit-test, because `space_render_elements`'s
  layer-splitting behavior was already correct and complete — the other two render paths were
  reimplementing a subset of it, not doing something genuinely different. There's no residual
  duplication to flag here the way there is for Q3's cross-level rect math.
- **Cost**: this is the piece most load-bearing on Q1's `HutSpaceElement`/synthetic-`Output`
  design actually working (a `CompositedTexture: SpaceElement` impl) — if that turns out to be
  harder than expected (see Q1's open risk), this collapse doesn't happen either, since it's
  the same underlying mechanism.
- **v1 gap preserved, not fixed**: `State::usable_area`'s own doc comment (`state.rs:517-522`)
  already flags that mudhuts' own chrome "still anchors to the raw output rect unconditionally
  — a real, accepted v1 gap." This RFC doesn't change that; the Layer-Shell Root Hut's
  "normal content" slot is still sized to `usable_area()`, and chrome still isn't threaded
  through it.

## Question 3: Does generic dispatch eliminate the hit-test duplication, or relocate it?

### What's actually duplicated today (and what already isn't)

The task's framing cites three call sites that "must agree": `state.rs`'s
`active_pane_offset` (`state.rs:552-577`), `render.rs`'s `build_tile_elements`
(`render.rs:374-457`), and `input.rs`'s `try_click_chrome` (`input.rs:224-304`). Reading all
three closely: they don't duplicate the *per-node* rect math — that part is already correctly
factored out and shared (`village.rs::pane_rects`, `village.rs:389-420`, is the single source
of truth for a Tile-Village's pane geometry, called by all three sites; `village_chrome.rs`'s
`level_layout` (`village_chrome.rs:78-93`) is shared between `build` and `handle_click` within
that module; `chrome.rs`'s `tab_layout` (`chrome.rs:120-142`) is shared the same way; `docks.rs`'s
`handle_layout` (`docks.rs:79-110`) likewise, per its own doc comment at `docks.rs:65-68`).

What's duplicated is the *composition across levels*: three separate call sites each
independently re-derive "start from `usable_area()`, add the active Tile-Village pane's offset
if there is one" — `active_pane_offset` computes it for mouse-interaction routing,
`build_tile_elements` computes an equivalent for render positioning, `try_click_chrome`
computes it a third time for its own Tile-Village click branch (`input.rs:250-266`). This is a
much narrower duplication than "every hit-test everywhere is duplicated" — it's specifically
the *walk from the tree root down to whichever leaf currently has effective focus*, done three
times by three different call sites that each only walk *part* of the tree relevant to their
own concern (mouse offset; tile-pane render positions; tile-pane click regions), rather than
one shared recursive walk.

### Proposal: `Hut::hit_test` recurses using the same node-local layout function `render` used

```rust
trait Hut {
    /// Composite this node's children (already-rendered, for a non-leaf
    /// child) into this node's own render target.
    fn render(&mut self, renderer: &mut GlesRenderer, target: Rectangle<i32, Physical>)
        -> CompositedTexture;

    /// Hit-test `point`, already translated into this node's own local
    /// coordinate space (i.e. `point - target.loc` was already applied by
    /// the caller before recursing in) — must use the exact same
    /// child-rect computation `render` used, not a re-derived one.
    fn hit_test(&self, point: Point<i32, Physical>) -> Option<Hit>;
}

enum Hit {
    Surface(WlSurface, Point<f64, Logical>),
    Terminal { hut_id: u64, local: Point<f64, Logical> },
    Chrome(Box<dyn HitTestable>),
}
```

A Tiling-Hut's impl, for instance, calls its own private `fn child_rects(&self) ->
Vec<Rectangle<...>>` (the direct generalization of today's `pane_rects`) from *both* `render`
and `hit_test` — `render` iterates every rect to place every child's composited texture;
`hit_test` finds the one rect containing `point` and recurses into that child with the point
translated into its local frame. Both consume the same underlying data, but the *traversal
shape* genuinely differs (render visits every visible child; hit-test visits exactly one) — so
the elimination is at the "don't independently recompute the rects" level, not at the "one
function handles both" level; that distinction already exists correctly in the current code
(e.g. `chrome.rs::tab_layout`/`build` do exactly this split today) and the redesign just makes
it apply uniformly, recursively, across every node kind instead of per-module.

Concretely, this removes `active_pane_offset` (`state.rs:552-577`), the pane-position half of
`build_tile_elements` (`render.rs:391-398,434`), and the pane-hit half of `try_click_chrome`
(`input.rs:250-266`) as three separate implementations, replacing them with one call to the
tree's own recursive `hit_test`/`render` starting from the root — each of which internally uses
exactly one `child_rects`-equivalent per node kind.

### Is this fully achievable, or does some duplication remain?

Mostly yes, with two honest caveats:

1. **Terminal-vs-surface routing is a further per-leaf decision that sits *below* the generic
   spatial hit-test**, not eliminated by it. `showing_terminal` (`hut.rs:114`, soon "Console
   Hut" — a per-node mode flag, not a tree-position fact) determines whether a Console Hut's
   effective leaf is "the terminal grid" or "the active Main Window," and today's
   `showing_terminal_effective()` (`state.rs:531-550`) additionally special-cases the
   Tile-Village branch to *force* terminal mode regardless of the flag (`state.rs:544-547`,
   with its own comment explaining this exists specifically to keep mouse-interaction routing
   from desyncing from what's on screen). This mode flag has to be consulted by a Console Hut's
   own `hit_test` impl, and by its own `render` impl, independently — that's two places
   reading the same flag, not duplicated *computation*, but worth being honest that "one
   recursive hit-test" doesn't make every per-node mode decision disappear.
2. **Mouse-report vs. text-selection vs. click-to-focus policy stays in `input.rs`, above the
   hit-test result** — `Hit::Terminal`'s local point still needs `pixel_to_cell`
   (`hut.rs:425-443`) and then a decision (mouse-reporting active? selecting? starting a new
   selection?) that has nothing to do with tree traversal. The proposed `Hit` enum's job is
   only to answer "what's under this point," matching today's `state.rs::surface_under`'s own
   scope (`state.rs:641-653`, whose doc comment already frames it exactly this way) — this RFC
   doesn't claim the *entire* `input.rs::PointerButton` handler collapses into the trait, only
   the spatial-routing portion that's actually duplicated today.

So: yes, this eliminates the specific, cited duplication (the three independent
`usable_area()`+`pane_rects()` walks) without relocating it elsewhere in a way that recreates
the problem — but it doesn't (and shouldn't try to) absorb every input-handling concern into
the trait.

## Cross-cutting: generalized redraw/input dispatch (required scope, not Hut-exclusive)

The user's explicit constraint: chrome (tab strips, dock handles, the switcher popup) is not
necessarily going to be modeled as Hut nodes, so "redraw happens automatically" and "input
dispatch happens automatically" must be capabilities anything can opt into, not behavior baked
into the `Hut` trait itself.

### Proposal: two small, independent, opt-in traits

```rust
/// A cheap, cloneable handle any renderable thing holds so its own state
/// mutations can report "I changed" without the call site needing to
/// remember `State::request_redraw()` itself. Backed by the same `Ping`
/// mechanism `State::redraw_ping` (`state.rs:292`) already uses — no new
/// primitive, just handed out more widely instead of `State` being the
/// only thing that can ever call it.
#[derive(Clone)]
pub struct RedrawHandle(smithay::reexports::calloop::ping::Ping);

impl RedrawHandle {
    pub fn mark_dirty(&self) {
        self.0.ping();
    }
}

pub trait Redrawable {
    /// Called once at construction; implementors store the handle and call
    /// `mark_dirty()` from inside their own mutating methods instead of
    /// exposing "please remember to redraw after calling this" as the
    /// caller's responsibility.
    fn attach_redraw_handle(&mut self, handle: RedrawHandle);
}

/// Anything that can claim a click/hit-test at a point already translated
/// into its own local coordinate space. Independent of `Redrawable` — the
/// switcher popup, for instance, currently has no click behavior at all
/// (Alt-Tab-preview-only, keyboard-driven), so it would implement
/// `Redrawable` without `HitTestable`.
pub trait HitTestable {
    fn hit_test(&self, point: Point<i32, Physical>) -> Option<Hit>;
}
```

A Hut-tree node implements both (its `render`/`composite` step calls `self.redraw.mark_dirty()`
whenever *its own* state changes in a way that should trigger a repaint — e.g. a Tabbed-Hut's
`active` index changing — replacing today's pattern of every call site in `input.rs` remembering
to call `self.request_redraw()` after every `Action::TabNext`/`WrapTab`/`StackNext`/etc.
(`input.rs:453-529`), each currently a distinct, easy-to-forget line). Chrome elements — a
`TabStripChrome`, a `DockHandleChrome`, the `SwitcherPopup` — implement the same traits directly,
without needing to be tree nodes: `docks.rs::DockDrag`'s `advance_drag`/`finish_drag`
(`docks.rs:223-281`) already independently call `state.request_redraw()`/
`sync_visible_main_window()` by hand today; under this scheme they'd instead hold a
`RedrawHandle` and call `mark_dirty()` internally, the same mechanism a Hut node uses.

Ownership/registration: a chrome element that's visually "attached" to a specific Console Hut
(a tab strip, dock handles) is constructed and owned by that Console Hut, and gets consulted by
that Console Hut's own `render`/`hit_test` impl as an internal implementation detail — not
registered globally. The switcher popup, which floats above the entire tree regardless of
position, is the one case `State` itself owns and consults directly, exactly as today
(`render.rs:288`, pushed first/frontmost).

### Does this actually replace the hand-wired call sites, or just move them?

For redraw: genuinely replaces them. Today, `Action::ToggleTerminal` originally forgot to call
`request_redraw()` at all (flagged explicitly in the plan doc's wishlist entry,
`.../cryptic-honking-lamport.md:995-996`, as a real bug this session hit) — the actual current
code does call it (`input.rs:450`), but the fact it was missing once and had to be added back
by hand is exactly the failure mode `Redrawable` is meant to structurally prevent: the call
moves from "every action handler must remember" to "every state-mutating method calls
`mark_dirty()` as part of doing the mutation," which can't be forgotten by a caller who never
sees the flag exists.

For hit-test: this is where the honest answer is "mostly, with one residual manual piece." The
*ordering* of who gets first crack at a click (switcher popup, frontmost; then Top/Overlay layer
surfaces; then the Hut tree; then Bottom/Background layer surfaces) still has to be specified
somewhere — today that's `input.rs::PointerButton`'s explicit `if`/`else if` chain
(`input.rs:701-791`). Under this proposal it becomes one ordered `Vec<&dyn HitTestable>`
constructed once (mirroring `render.rs::build_frame_elements`'s own front-to-back push order,
`render.rs:284-372`, which already *is* that same ordering, just for rendering rather than
hit-testing) and walked generically — a real reduction (N hand-chained branches become one
loop over a declared list), but the list's *order* is still a piece of hand-maintained policy,
not something the trait system derives on its own. This RFC doesn't claim otherwise.

## Briefly: `MainWindowEntry`/`SubWindow`/`Alert` under the new tree

Today, `Hut::main_windows: Vec<MainWindowEntry>` (`hut.rs:105`) is a flat, tab-cycled list; each
entry separately owns `sub_windows: Vec<SubWindow>` and `alerts: Vec<Alert>`
(`main_window.rs:101-104`) that are *not* part of tab-cycling — they float or dock alongside
whichever Main Window tab is currently active, all shown simultaneously
(`state.rs::sync_visible_main_window`, `state.rs:589-617`, maps every floating Sub-Window and
every Alert of the active entry, not just the active entry itself).

The plan doc's tree diagram (`.../cryptic-honking-lamport.md:748-765`) shows a Console Hut's
Tiling/Tabbed-Hut's leaf children as "Sub-Hut(s) (the actual Wayland windows)" — read literally,
that would mean Main Windows *and* Sub-Windows/Alerts become uniform leaves of the same
Tiling/Tabbed-Hut. Reading the actual current semantics closely, that doesn't fit:

- Sub-Windows/Alerts are deliberately never part of "only one visible at a time" tab-cycling —
  they're always-additionally-visible overlays on top of whichever Main Window tab is active.
- A **docked** Sub-Window isn't a mapped/composited surface at all today — `docks.rs`'s own
  module doc is explicit: "isn't mapped as a real surface at all — nothing to composite... a
  small handle instead" (`docks.rs:6-14`). It has no `Window` to hand a tree leaf in the first
  place while docked.

**Recommendation**: the Tabbed-Hut-of-Sub-Huts in a Console Hut should model only
`main_windows`' existing tab-cycling (a direct rename of today's `Vec<MainWindowEntry>` +
`active_main_window` + `showing_terminal`, `hut.rs:105-114`, into the generic Tabbed-Hut shape).
Sub-Windows/Alerts should stay a Console-Hut-owned side structure, composited as an *additional*
sibling layer on top of the Tabbed-Hut's active child (a third composition rule alongside
Tiling/Tabbed — "show my active tab, plus every currently-floating overlay" — which still fits
the "parent composites children per its own kind" principle, just not as literal
same-`Vec` leaves). A docked Sub-Window, having no surface to composite, is modeled the same way
dock handles already are: as `HitTestable`/`Redrawable` chrome (see the cross-cutting section),
not a tree node.

**Naming collision — resolved by the user (2026-08-13): `Sub-Window` → `Floating Window`.** The
plan doc's proposed leaf name "Sub-Hut" collided with the pre-existing "Sub-Window" concept
(`main_window.rs:31-45`) — a Sub-Hut means "any leaf Wayland window," while the old
"Sub-Window" specifically meant "a window tagged as belonging to, and dockable/floating
relative to, a Main Window." Rather than rename the new "Sub-Hut" leaf concept, the user chose
to rename the old, pre-existing concept: `SubWindow` → `FloatingWindow` throughout
(`main_window.rs`, `docks.rs`, `handlers/xdg_shell.rs`'s `set_sub`/role-assignment code, the
`mudhuts_shell_v1` protocol's `set_sub` request/role naming, doc comments, test names). Note
this name is slightly imprecise for the *docked* state (a "Floating Window" that's currently
docked isn't floating) — matches how "Sub-Window" was already used for both docked and
floating states today, so this isn't a new inconsistency. Should land as part of the same
atomic rename commit as `Village`→`Hut`/`Hut`→`Console Hut` (see Migration Strategy), not a
separate pass — same "sweeping, invasive, do it in one commit" reasoning applies.

## Briefly: damage-tracking state (`LabelCache`/`ChangeTracker`/`DamageBag`)

Confirmed, not just assessed: this genuinely is a small deviation. These are already
per-instance, not global, at every level that has one today — `TabVillage` owns its own
`label_cache: Vec<LabelCache<...>>`/`tab_ids: Vec<(Id, Id)>`/`bg_tracker: Vec<ChangeTracker<...>>`
(`village.rs:62-72`), grown/shrunk in lockstep with `children` (`village_chrome.rs:149-153`,
`village.rs:296-306`); `Hut` (soon Console Hut) owns its own terminal-tab cache/tracker/ids
(`hut.rs:70-78`); `MainWindowEntry` owns its own tab cache/tracker/ids (`main_window.rs:105-121`);
`SubWindow` owns its own handle-label cache (`main_window.rs:34-44`). The redesign's "each Hut
owns whatever state its own render step needs" principle is already exactly how these work —
migrating a `TabVillage` into a generic Tabbed-Hut node doesn't change this pattern, it just
means the same 3-field bundle needs to live on whatever struct represents "a Tabbed-Hut" instead
of specifically `TabVillage`.

One genuine opportunity, not a requirement: today these are four independently hand-maintained
`Vec` bundles kept in sync by hand (`village.rs::remove_child_hut`'s `keep: Vec<bool>` dance,
`village.rs:296-306`) — the plan doc's own "General code cleanup" wishlist entry already flags
this as "could plausibly be one generic 'parallel Vec of per-child state' container instead of
four hand-maintained ones" (`.../cryptic-honking-lamport.md:1026-1029`). Migrating to the new
tree is a natural point to actually do that consolidation (one `ChildState<T>` container used by
every Tabbed-Hut-shaped node), but it's an independent cleanup, not something the tree redesign
requires.

## Migration Strategy

**The rename must not be incremental.** The plan doc is explicit that `Village`/`Hut` "swap
meaning rather than one simply being retired" (`.../cryptic-honking-lamport.md:727-728`) — a
staged rename would leave both old meanings of the bare word "Hut" live in the same codebase
simultaneously for the duration, which is a correctness/reviewability hazard strictly worse
than the size of the mechanical change itself. Recommend: one atomic, tooling-assisted
(not hand-typed), behavior-preserving commit, reviewed as a pure rename (verified by identical
before/after test results and, ideally, an automated check that the diff is rename-shaped)
before any of the architectural changes below begin.

**The architectural change can be substantially incremental**, staged to prove out the new
primitives against low-blast-radius targets before touching the load-bearing `Village`/
`HutStack` types:

1. Introduce `RedrawHandle`/`Redrawable`/`HitTestable` (cross-cutting section) and convert
   `docks.rs`'s dock handles to use them, net-new, alongside the existing code paths — the
   smallest, least load-bearing chrome element, and already the one whose docs most explicitly
   call out shared render/hit-test rect math (`docks.rs:65-68`), so it's the natural first
   proof point.
2. Convert the switcher popup (`switcher.rs`) the same way — still self-contained, still
   non-Hut, validates that `Redrawable` alone (no `HitTestable`, since the switcher isn't
   currently click-driven) is a real, useful partial adoption.
3. **Done (2026-08-13, `mudhuts/src/hut_space.rs`).** Prove out Q1's per-node
   `Space<HutSpaceElement>` design for exactly one scope — the currently-focused Console Hut's
   Main-Window-visible case — rendering into its own texture via a synthetic `Output`, side by
   side with (not replacing) `state.space`/`sync_visible_main_window`, gated (env var, off by
   default) so it can be compared against the existing path's output before anything is
   removed. See Open Question 1's resolution below for what this actually found.
4. Only once 1–3 are validated, replace `Village`'s `Tab`/`Tile` enum variants and `HutStack`'s
   bespoke MRU bookkeeping with real generic Hut-tree node kinds (an `MruStackHut`, `TileHut`,
   `TabbedHut`, and the renamed Console Hut) implementing the traits proven out above. Be
   honest that this step is inherently large by itself: today's `Village::Tab`/`Village::Tile`
   match arms are inlined across `stack.rs`, `render.rs`, `input.rs`, and `village_chrome.rs`
   simultaneously (e.g. `village.rs::pane_rects` alone has three independent call sites across
   three files, per Q3's findings above), so there is no way to swap *only* `village.rs` and
   leave the others on the old model mid-flight without a temporary compatibility shim uglier
   than just doing the swap in one step. This is the one place a genuinely large single
   change is realistic and shouldn't be pretended otherwise.
5. The Layer-Shell Root Hut (Q2) last — it depends on nothing else in the tree and can land
   either alongside step 4 or after it; until it lands, `layer_elements` can stay as-is as a
   temporary bridge.

**Honest overall assessment**: mostly incremental — the traits and per-scope `Space` design get
real, independent validation before the highest-risk step — but step 4 is unavoidably a large,
mostly-atomic change given how entangled the current `Village`/`HutStack` types already are
with every render/input call site. Calling the whole effort "incremental" would undersell that
one step's real size and risk.

## Open Questions Still Unresolved

1. **Resolved by migration step 3's spike (`mudhuts/src/hut_space.rs`, 2026-08-13).**
   `HutSpaceElement`'s `SpaceElement`/`AsRenderElements` impls for the `Composited` variant are
   now real, working code — confirmed there is no existing precedent anywhere in Smithay,
   anvil, or smallvil for a texture-backed (non-`Window`) `SpaceElement` (`Window`, `X11Surface`,
   and anvil's `WindowElement` are the only implementors, and all three are ultimately
   `Window`-backed), so this genuinely was a novel impl, not an adaptation. Required 5 methods
   with no default (`bbox`, `is_in_input_region`, `set_activate`, `output_enter`,
   `output_leave`) plus `IsAlive::alive` — all trivial no-ops/constants for a texture with no
   persistent identity (freshly built and discarded every use), and a `PartialEq` impl by a
   fresh per-instance `Rc<()>` marker (`GlesTexture` itself has no `PartialEq`, so this couldn't
   be derived). Live-verified, not just compiled: a private `Space<HutSpaceElement>` bound to a
   synthetic `Output`, holding the same real `Window`s `state.space` maps today at translated
   local coordinates, rendered byte-for-byte identically to the existing path against a real
   client (`cosmic-term`) — and a `Window` and a `Composited` texture were confirmed to coexist
   in the same `Space` and render correctly together (not something today's Main-Window-visible
   scope alone can exercise, since there's no non-leaf Console-Hut child yet to produce a
   `Composited` element from in real use — this pushed the focused Console Hut's own cached
   terminal texture in alongside the real window purely to spike the mechanism ahead of step 4
   needing it).

   **One real gotcha surfaced by getting the comparison wrong the first time**: an initial
   attempt reused the real "winit" `Output` for the old-path half of the comparison and saw a
   spurious ~56%-of-pixels "divergence" — caused by `OutputDamageTracker::render_output` baking
   in that output's own `Transform::Flipped180` (a `winit_backend.rs`-specific workaround) and
   the host's real DPI scale, neither of which has anything to do with whether
   `HutSpaceElement` composites correctly. Fixed by rendering *both* paths through matching
   clean `Transform::Normal`/scale-`1.0` stand-in outputs, isolating the actual variable under
   test. Worth remembering for step 4: any future comparison between old and new render paths
   needs to control for output transform/scale explicitly, or a real bug and a harness artifact
   look identical.
2. **Per-node synthetic `Output`/`Space`/`LayerMap` memory/perf cost at real nesting depth**
   wasn't measured — this RFC only confirms it's *architecturally* valid, not that it's free.
   Given the plan doc's own unresolved "Investigate high memory usage" item
   (`.../cryptic-honking-lamport.md:1030-1036`), this deserves real profiling before committing,
   not after.
3. **Popup positioning for a genuinely non-fullscreen leaf** (a Main Window inside a Tile-Village
   pane, or nested several levels deep) still isn't designed — `unconstrain_popup`'s current
   fullscreen-at-output-origin shortcut (`xdg_shell.rs:328-334`) was already flagged by the plan
   doc as out of v1 Tile-Village scope for exactly this reason (`village.rs:10-21`), and this
   RFC's tree makes "arbitrary nesting depth" the *point*, which reopens rather than resolves
   it.
4. **The full set of `input.rs` special cases above the hit-test layer** (mouse-report vs.
   text-selection vs. click-to-focus, `keyboard-shortcuts-inhibit-unstable-v1`'s per-surface
   opt-out at `input.rs:588-601`, session-lock's total override at
   `input.rs:387-410,548-561`) weren't individually re-verified against the proposed `Hit` enum
   — Q3's proposal is scoped to the spatial-routing portion `state.rs::surface_under` already
   covers today, and this RFC didn't exhaustively check that none of those other special cases
   secretly assume something about `Space`/`self.space` specifically rather than "whatever the
   hit-test layer returns."
5. **`hit_test` using the same rect math as `render`** is proposed as a convention (share one
   `child_rects`-style helper), not something enforced by the type system — nothing stops a
   future node-kind impl from drifting the same way the pre-redesign duplication happened in
   the first place. Worth a follow-up on whether a lint, a shared-helper-only constructor
   pattern, or a test asserting render/hit-test agreement can make this structural rather than
   disciplinary.
6. **Sub-Window/Alert modeling** (the "third composition rule" proposal above) is a sketch, not
   a full design — doesn't yet cover the interaction between the drag mechanics in
   `docks.rs::DockDrag`/`advance_drag`/`finish_drag` (`docks.rs:52-63,219-281`) and the new
   `Redrawable`/`HitTestable` traits in detail (e.g. exactly when a docked Sub-Window's handle
   stops being `HitTestable`-as-chrome and starts needing tree-node treatment mid-drag, the
   instant it's mapped as a real surface — `docks.rs:236-244`).
7. **The Sub-Hut/Sub-Window naming collision** is flagged, not resolved — needs the user's own
   call before the rename in step 1 of migration locks it in.
