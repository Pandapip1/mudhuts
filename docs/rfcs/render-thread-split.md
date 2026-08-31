# Render-thread split

## Status

Drafted 2026-08-31, after a long research/discussion session establishing the target
architecture and closing out every open question except one. This RFC exists so implementation
can begin from settled facts instead of guessing, the same discipline `typed-graph-hut.md`/
`composable-hut-hierarchy.md` used. Two small, real pieces of prep work landed on `main` before
this RFC existed (see "Landed prep work" below); migration Steps 1-3 and 4a (see "Migration
plan") have since landed on the dedicated `render-thread-split` branch. The real thread spawn
itself (Step 4b) hasn't begun.

## Motivation

The user's request: mudhuts' subsystems (Wayland protocol dispatch, GPU rendering, input) should
react to their own interrupts on their own threads, so a slow one never adds latency to another
— rather than everything being serialized through today's single-threaded `calloop` event loop
by construction. mudhuts is already provably idle-until-interrupt with bounded per-event work
(no polling, no busy-loops — audited earlier in the same session), so this isn't fixing a
measured problem; it's a deliberate architectural direction the user wants taken.

The first framing discussed — one OS thread per interrupt source, each independently
synchronized, no master mutex — turned out not to fit how mudhuts' state is actually shaped
(see "Why not one-thread-per-source" below). What emerged instead, through real investigation
rather than assumption, is a two-thread split: one thread owns the scene graph and reacts to
both Wayland requests and input; the other owns the GPU/DRM/session machinery and reacts to
vblank completion, reading published state rather than reaching into the scene graph directly.

## Why not one-thread-per-source

Three research passes (Wayland client-socket dispatch, DRM vblank, libinput) into "which of
these could reasonably run on its own thread, independently synchronized" found:

- `GraphStack` (`mudhuts/src/graph_stack.rs`, `State::stack`) is a single, ~170-call-site scene
  graph that input, render, both backends, docks, grabs, and most Wayland protocol handlers all
  read *and* write. There's no existing seam splitting it into independent per-subsystem pieces
  — focus, window position/z-order, cursor state, and lock state are each touched by at least
  two of {input, render, protocol handlers}, not confined to one.
- `State` itself is `!Send`/`!Sync` today: three direct `Rc<RefCell<_>>` fields (the GL
  renderer, the winit backend handle, the udev backend's inner state), plus `GraphStack`
  carrying an `Rc`-backed `calloop::LoopHandle` and its own aliased renderer handle.
- Two Wayland protocol dispatch callbacks (dmabuf import, screenshot capture) synchronously
  drove real GPU rendering *inline* — dispatch and rendering weren't behaviorally separable in
  the handler code, not just at the type level (since fixed for dmabuf — see "Landed prep work").
- Smithay's own types (`DisplayHandle`, `Client`, per-protocol state structs) are `Arc`-based and
  thread-friendly — the blocker is mudhuts' own `State`, not the framework. No reference
  compositor, including Smithay's own `anvil`, does multi-threaded dispatch; this is genuinely
  new territory, not a well-trodden path.

Splitting into N independently-locked subsystems, given this, would mean either a lock broad
enough to cover the shared scene graph for nearly every real operation (a master mutex by
another name), or a full redesign of what "focus," "window layout," and "cursor state" *are* so
each subsystem can own an independent copy reconciled via messages — real architecture work,
not a threading refactor, with real risk of introducing exactly the stale-state bugs (a render
pass drawing a focus ring input already moved) that don't exist today by construction.

## Target architecture

- **Core thread** (today's main thread, keeps the role): owns `Display<State>`/all Wayland
  protocol dispatch and the full `GraphStack` as its sole writer. Consumes already-parsed input
  events forwarded over a `calloop::channel` from the render thread (see below), rather than
  reading libinput directly.
- **Render thread** (new): owns the GL/EGL context, DRM commit/vblank handling, *and*
  session/libinput ownership. Reads published, genuinely-`Send` state rather than reaching into
  `GraphStack` directly.
- **PTY threads**: unchanged. Already exist, one per `ConsoleHut`, via
  `alacritty_terminal::event_loop::EventLoop::spawn` (`mudhuts-term/src/lib.rs:163`) — the
  existing precedent this whole effort's cross-thread plumbing follows.
- **Wake/notify**: reuse calloop's existing cross-thread-safe primitives. `redraw_ping`'s
  `Ping`/`RedrawHandle` (already `Arc`-backed) is core→render's "something changed, redraw."
  `calloop::channel` (already the PTY-thread→core mechanism, `console_hut.rs`) is the template
  for render→core acknowledgments and the request/response handoffs GPU-bound protocol handlers
  need.
- **Scheduling**: if `SCHED_FIFO` (`perf_config.rs`/`rt_sched.rs`) is ever extended beyond core,
  every real-time thread must share one priority — Linux doesn't preempt between equal-priority
  `SCHED_FIFO` threads, which is what rules out classical priority inversion here (a thread only
  ever waits on another that's genuinely still making progress, never one that got preempted
  mid-critical-section by a third thread). `RLIMIT_RTTIME` needs no new code when this happens:
  confirmed against actual kernel source (`kernel/sched/rt.c`'s `watchdog()`, which checks
  `task_rlimit(p, RLIMIT_RTTIME)` against that specific `task_struct`'s own
  `p->se.sum_exec_runtime`) that the accounting is per-thread while the limit *value* is the
  normal process-wide rlimit, already set once (`rt_sched::apply`, commit `89f81ec`) — a future
  render thread automatically inherits the cap and gets its own independent watchdog for free.

### Why session/DRM/input end up on one thread

`LibSeatSession` (`smithay/src/backend/session/libseat.rs:44-46`) is `Rc`/`RefCell`-based
internally (`Weak<LibSeatSessionImpl>`; the impl holds `RefCell<Seat>`) — not `Send`. Both DRM
device access and libinput's own session interface
(`Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>`) need the *same* session
handle today, ruling out "core owns input, render owns DRM" as originally stated: those two
can't be split across threads without either two separate seat sessions (unverified whether
libseat supports one process holding two, real risk of odd device-fd-duplication behavior) or
keeping them together. Resolution: session ownership, DRM commit/vblank handling, *and*
libinput's raw fd reading live together on the render thread (still the right name — GL/DRM is
still its defining role), doing `libinput_dispatch()`-driven event reading (cheap, bounded, no
scene-graph access needed) and forwarding already-parsed `InputEvent`s to core over a
`calloop::channel`, mirroring `console_hut.rs`'s PTY-thread pattern exactly. Core remains the
sole scene-graph writer regardless — it just receives input via a channel instead of a raw fd.

Independently, `GlesRenderer` (`smithay/src/backend/renderer/gles/mod.rs:404`) carries an
explicit, permanent `_not_send: PhantomData<*mut ()>` field — Smithay's own deliberate design,
not an incidental EGL restriction. A constructed `GlesRenderer` can **never** be moved to
another thread by any means, regardless of whether its underlying `EGLContext` is itself `Send`
while unbound (it is — general EGL semantics and Smithay's own `unsafe impl Send for
EGLContext` agree that "create on thread A, first `eglMakeCurrent` on thread B" is valid,
provided the context is never simultaneously current on two threads — but mudhuts never hands
around a bare `EGLContext`, only the `GlesRenderer` wrapping it). So the render thread has to
run `init_udev`'s entire current GBM/EGL/`GlesRenderer`/`DrmOutputManager` setup itself, from
scratch, from inside the thread — core can't build any part of it and hand it over. Two
independent Smithay-enforced constraints landing on the same architecture, not a coincidence.

## Landed prep work (on `main`, both single-threaded)

- **`RenderedContent`/`ContentPiece` (`graph.rs`) confirmed `Send`** (commit `b7d9d22`), with a
  compile-time `assert_send::<RenderedContent>()` check next to the type definition. Both
  variants are `Arc`-backed internally in this pinned Smithay rev (`GlesTexture(Arc<
  GlesTextureInternal>)`, `Window(Arc<WindowInner>)`), unlike `GraphStack` itself — so resolved
  window content can already cross a thread boundary with no new wrapper type.
- **`render.rs` no longer touches keyboard focus** (commit `7b0469e`). The per-frame focus
  repair backstop that used to run inside `build_frame_elements` now runs through two
  chokepoints on the input/protocol side instead — `State::process_input_event` (a thin `pub`
  wrapper around a private `_unsynced` implementation, so nothing outside `input.rs` can process
  an event without the backstop running) and the Wayland-dispatch closure in
  `init_wayland_listener`, right after `dispatch_clients`. This removes the one case where
  render was a second writer of scene-graph-adjacent state.
- **dmabuf import routed through a channel** (commit `2407834`). `DmabufHandler::dmabuf_imported`
  sends a new `DmabufImportRequest { dmabuf, notifier }` (`udev_backend.rs`) over a
  `calloop::channel` instead of importing inline. The receiver, registered in `init_udev`
  alongside `dmabuf_renderer`/`dmabuf_global`, does exactly what the old inline code did — same
  thread today, one real dispatch tick later instead of synchronously, proving the handoff shape
  before a real second thread exists to receive on the other end. Confirmed safe against
  Smithay's own docs: `ImportNotifier` is explicitly `Send`, "to allow import of a Dmabuf to
  take place on another thread if desired," and the wire protocol's `create`/`created`/`failed`
  exchange is async by design regardless of which thread resolves it.

Two other protocol handlers were investigated for the same conversion and found **not**
independently tractable — both entangled with the real remaining design question below, not
converted:

- **Screenshot/frame capture** (`handlers/capture.rs`): `capture_frame`/`render_capture` call
  `render::resolve_frame_content`/`build_frame_elements`, needing full `GraphStack` access to
  build chrome/docks/window content for the target output — not a self-contained
  `(buffer, notifier)` pair like dmabuf. (Smithay's own protocol/type support is equally
  favorable here too, for the record: the `capture` request's spec explicitly allows the
  compositor to defer "an indefinite amount of time," and `Frame`/`Session` are `Arc<Mutex<...>>`
  -backed with no thread affinity — the blocker is entirely mudhuts' own handler code needing
  the scene graph, not Smithay.)
- **Session-lock confirmation** (`pending_lock_confirmed_outputs`): `mark_this_output_confirmed`/
  `confirm_pending_lock_if_ready` live inside `render_surface` itself
  (`udev_backend.rs:1217-1261`, which moves to the render thread wholesale), and the "is every
  output confirmed yet" check reads `self.stack.outputs()` — core-owned `GraphStack` state.

DRM-leasing and gamma-control handlers were also checked: both are already self-contained w.r.t.
`GraphStack` (only ever touch `udev_inner`/`drm_leasing_global`), so gamma-control could use the
same channel pattern dmabuf now does. DRM-leasing has a separate wrinkle worth a real decision,
not a blocker: `DrmLeaseHandler::lease_request` returns `Result<DrmLeaseBuilder, LeaseRejected>`
directly, with no deferred-completion object like `ImportNotifier`/`Frame` — Smithay's own
dispatch code wants a synchronous answer here. Either core briefly blocks on the render thread's
reply for this one rare, one-off request (a VR headset connecting — not a hot path, arguably an
acceptable deliberate exception), or core keeps a small cached copy of "which connectors/planes
are currently leasable," good enough to decide without asking render at all.

## The real remaining question: chrome/label rendering couples "decide" and "draw"

`chrome::build` (`chrome.rs:195`) takes `renderer: &mut GlesRenderer` directly and rasterizes
tab labels into GPU textures live, with its own per-`ConsoleHut` cache, as part of deciding what
chrome to draw each frame. So for everything except window content (already cleanly separated
by `ContentPiece`), *deciding* what UI to draw and the *GPU work* of drawing it are the same
interleaved step today. This is the one piece of the split without a existing clean seam,
investigated in depth:

### What's actually true about label rendering (verified, not assumed)

- **Rasterization is genuinely GPU-free.** `GlyphCache` (`mudhuts-term/src/render.rs:20-169`)
  uses `fontdue` (pure software rasterizer) plus `fontconfig` for font resolution — no `ab_glyph`/
  `rusttype`/freetype/`cosmic_text`/`swash` anywhere in the tree. `GlyphCache::glyph` produces an
  owned `(fontdue::Metrics, Vec<u8>)` coverage bitmap, memoized per `(char, bold)`, with zero GL
  dependency. Atlas placement (`ShelfPacker::alloc`, `gpu_term.rs:245-261`) is pure arithmetic
  too. Only the final `gl.TexSubImage2D` upload (`GlyphAtlas::atlas_entry`, `gpu_term.rs:471-512`)
  and the instanced draw (`draw_instances`, `gpu_term.rs:531-624`) are real GPU work.
- **The barrier is interleaving, not a real dependency.** `atlas_entry` takes `gl: &ffi::Gles2`
  unconditionally even on its cache-hit path, and `LabelRenderer::render`/`chrome::build` thread
  one live renderer through rasterize-or-skip, upload-if-new, and draw as one inseparable call.
  Nothing here has an actual GPU dependency in the rasterization/placement half — it's just never
  been pulled apart.
- **The atlas is shared with the terminal's own glyph grid, not label-specific.** `GlyphAtlas`
  (shader + atlas texture + `HashMap<(char,bold), AtlasEntry>`) is held as `Rc<RefCell<
  GlyphAtlas>>` and shared between `GpuTermRenderer` (whole terminal grid) and `LabelRenderer`
  (chrome labels) *per `ConsoleHut`* (`console_hut.rs:865-897`) — a tab label and the terminal's
  own glyphs draw from the exact same atlas texture and the exact same `GlyphCache`. Splitting
  label rendering's atlas-upload step necessarily also touches the terminal-content path's
  upload step (`GpuTermRenderer::redraw`, `gpu_term.rs:813-937`) — you can't split one half of a
  shared atlas from the other without either duplicating it or moving both sides.
- **`LabelCache<T>` (`render.rs:124-171`)** is already a pure-CPU value comparison
  (`is_stale`/`PartialEq` on a key like `(String, bool)`) gating whether re-rasterization is
  needed at all — only its `store`/`cached` methods hold the actual `GlesTexture` GPU resource.
- **Blast radius**: on the order of 2,000-2,500 lines touched directly — `chrome.rs` (359),
  `docks.rs` (669), `village_chrome.rs` (231), `gpu_term.rs` (1171), plus the relevant slices of
  `console_hut.rs` (1053, owns the lazy `GlyphAtlas`/`LabelRenderer`/`GpuTermRenderer`
  construction) and `render.rs` (1381, `build_frame_elements` is the single call site that
  threads one live renderer through `switcher::build`/`village_chrome::build`/`chrome::build`/
  `docks::build` every frame). **`switcher.rs` is a smaller concern than the others** — its
  `build` takes `renderer: &GlesRenderer` (shared, not exclusive) and never rasterizes text at
  all; it only recomposites already-rendered `GlesTexture`s at a different size.

### The shape of a fix (not yet designed in full — this is the open item)

The existing window-content split is the model: `graph.rs`'s module doc is explicit that the
graph/node layer "has no dependency on `GlesRenderer` itself, or on doing any actual
rendering" — `RenderedContent` just carries an already-produced `GlesTexture`/`DamageSnapshot`
as a plain field, with GL-touching resolution isolated behind `RenderEnv`, a generic environment
parameter purely-structural nodes never need to know about. Chrome/label rendering has no
equivalent today; `chrome::build` et al. reach directly into `&mut GlesRenderer` and
`ConsoleHut::render_label` inline rather than resolving through a value type that already
carries a finished texture the way `ContentPiece::Texture` does.

A redesign in this spirit would split, per label:
1. **Core-side (pure CPU, no GL context needed)**: decide layout (which chars, what cell
   size/position/colors from `state.theme`/tab-active-state), rasterize any not-yet-cached
   glyphs via `fontdue` (already GPU-free), and decide atlas placement via the packer's own
   pure-arithmetic `alloc` — producing a `Send` description of "what to upload, if anything" plus
   "what instances to draw" (position/uv-into-atlas/color per glyph). This requires the atlas's
   *placement bookkeeping* (`HashMap<(char,bold), AtlasEntry>`, `ShelfPacker` state) to become
   something core can consult/update without a live GL context — either core-owned directly, or
   a core-side mirror kept in sync with render's real atlas (glyph placement decisions are never
   speculative/rolled-back in practice, so an optimistic mirror should be safe).
2. **Render-side (real GPU work)**: apply any pending atlas uploads (`TexSubImage2D`), then run
   the already-decided instance list through the existing `draw_instances`. Sizes/positions are
   fully determined by core's decision — render doesn't decide anything, only executes.

Because the atlas is shared with terminal-grid rendering, this same split needs to extend to
`GpuTermRenderer`'s own upload step for consistency, not stay label-specific — `mudhuts-term`'s
existing CPU-side dirty-cell/damage collection (`take_dirty_cells`/`collect_cells`) is already
the equivalent "core decides" half for terminal content; only the atlas-upload/draw step would
need the same core/render split label rendering does.

This is comparable in scope to `typed-graph-hut.md`'s own migration, not a Phase 2b sub-bullet —
it touches shared, load-bearing rendering infrastructure (the atlas both label and terminal
rendering depend on), not an isolated corner. It deserves its own staged migration plan (see
below), landed on its own branch off `main` given `main`'s HEAD is what the user's live nixos
config pins by commit hash, the same precedent `dag-hut-rearchitecture` set.

## Migration plan

Mirroring `typed-graph-hut.md`'s own discipline — one real, working, build/clippy/test-verified
step at a time, each its own commit, on the `render-thread-split` branch (off `main`, same
precedent as `dag-hut-rearchitecture` — `main`'s HEAD is what the user's live nixos config pins
by commit hash):

1. **DONE** (commit `89109af`) — **extracted the atlas placement decision as a pure function**,
   `place_glyph` (`gpu_term.rs`): given an already-rasterized glyph's size/metrics and the
   atlas's own `ShelfPacker` state, decides where it goes and precomputes the resulting
   `AtlasEntry`, with no GL context involved. Ended up a slightly different shape than first
   sketched here — the cache lookup and rasterization (`glyph_cache.glyph`) stayed inline in
   `atlas_entry` rather than folding into the pure function too, since both were already trivial/
   GPU-free on their own and folding them in would have meant either cloning the glyph bitmap
   unnecessarily or inventing a `GlyphCache` test fixture (real font/fontconfig resolution —
   nothing else in this codebase's tests does that). Only the packer-allocation arithmetic
   actually benefited from extraction, so that's the whole diff. Behavior-preserving: `atlas_
   entry` still calls it inline, immediately followed by the same `TexSubImage2D` upload as
   before, still on one thread. Real new unit test coverage landed alongside it (`place_glyph_
   tests`) that wasn't possible before the split, since exercising the packing decision used to
   require a live `GlesRenderer` just to construct a `GlyphAtlas` at all.
2. **DONE** (commit `d903014`) — **`LabelRenderer::render` split into a `Send`-compatible
   "resolved label" value plus a separate draw step.** Also extended Step 1's `GlyphAtlas` split
   further while here: `atlas_entry` is now `decide_entry` (GL-free — cache lookup,
   rasterization, atlas placement) immediately followed by `apply_upload` (the one real GPU
   call), kept fused for `GpuTermRenderer`'s still-unsplit per-glyph usage (that's Step 3).
   `LabelRenderer::resolve` calls only the GL-free half for every glyph in a label, producing a
   new `ResolvedLabel` (target size, the fully-decided instance list, any newly-rasterized
   glyphs still needing upload — plain owned data, `Send`, no GL objects); `LabelRenderer::draw`
   applies those pending uploads and runs the unchanged `draw_instances` path. **Scoped smaller
   than originally sketched here, deliberately**: `chrome::build`/`docks::build`/`village_
   chrome::build` were *not* changed to call `resolve`/`draw` directly — `render()` stays as
   `resolve()` immediately followed by `draw()`, so every existing call site is untouched.
   Rewiring those call sites only actually matters once there's a real thread boundary to send
   a `ResolvedLabel` across (Step 4); doing it now would've meant either an unused API surface
   or forcing `LabelCache`'s own caching gate (`is_stale`/`store`/`cached`) to be redesigned
   around a value type it doesn't need to know about yet — real, separate scope, not free to
   fold in here. Behavior-preserving: same GL calls, same order, same one `with_context` scope
   per label.
3. **DONE** (commit `686983a`) — **extended the decide/apply split to `GpuTermRenderer`'s
   terminal-grid glyph resolution**, since it shares the atlas with labels and Step 2's split
   would otherwise be fighting a still-fused terminal-content path for the same resource.
   `redraw()`'s glyph loop now calls `GlyphAtlas::decide_entry` directly (GL-free) instead of the
   old fused `atlas_entry` wrapper (removed — both real call sites had moved off it, so it was
   genuinely dead rather than worth keeping around). **Deliberately a different shape from
   Step 2**, and a real finding worth recording: `redraw()` is a genuine per-frame hot path (up
   to 120Hz), where `LabelRenderer`'s pattern of returning an *owned* `ResolvedLabel` value was
   fine (labels are cache-gated, rarely re-resolved) but would be a real, avoidable allocation
   here. So `GpuTermRenderer` keeps using `&mut self` scratch buffers (a new
   `pending_uploads_scratch` field, same clear-not-reallocate pattern as the existing
   `instances_scratch`) rather than an owned intermediate value — the GL-free/GL-only split is
   real (decide_entry needs no `with_context` at all now, down from two calls per redraw to
   one), but **the actual `Send`-compatible cross-thread transport shape for terminal content
   is still an open question for Step 4**, not something this step decided. Whatever it ends up
   being (a double-buffered scratch structure handed across a channel? something else?) has to
   respect this same hot-path constraint — a fresh owned `Vec` every redraw is not an option.
   Behavior-preserving, and a real side-effect win: one `eglMakeCurrent` call per redraw instead
   of two, since the glyph-resolve pass no longer needs a context at all.
4a. **DONE** (commit `cb6308d`) — **split `GlyphAtlas` into `AtlasPlacement` (`Send`, packer +
   glyph-placement `HashMap` + the reserved `white` entry — proven `Send` by a compile-time
   assertion mirroring `graph.rs`'s `RenderedContent` one) and a reduced `GlyphAtlas` (just the
   GL handles: `program`, `atlas_tex`, `quad_vbo`, `u_target_size`).** Investigated why Step 4b
   couldn't proceed without this first: `ConsoleHut` directly owns `gpu: Option<
   GpuTermRenderer>`/`label_renderer: Option<LabelRenderer>`, both holding real GL objects, so
   neither can live on a `ConsoleHut` that has to keep living inside core-owned `GraphStack` —
   and even after Steps 1-3, `GlyphAtlas` still bundled GL-free placement bookkeeping together
   with real GL handles in one struct, so `decide_entry` had no way to run without an
   `Rc<RefCell<GlyphAtlas>>` in scope regardless. `ConsoleHut` gained a new `atlas_placement:
   Option<AtlasPlacement>` field (created/reset in lockstep with `gpu`/`label_renderer`);
   `GpuTermRenderer::redraw`/`LabelRenderer::render`/`resolve` take `atlas_placement: &mut
   AtlasPlacement` as an explicit parameter now instead of reaching into `self.atlas`.
   **Scoped smaller than a first pass might assume**: `gpu`/`label_renderer` themselves stay on
   `ConsoleHut` for now — moving them into a render-thread-owned side table only makes sense
   once Step 4b's real thread exists to own that table; inventing a placeholder home for it in
   this still-single-threaded step would mean redoing the move once that thread lands. Caught
   and fixed a real preserve-behavior-exactly bug on review: an early draft of the reserved
   white slot's `AtlasEntry` reported a 2×2 `uv_size`/`width`/`height` matching its 2×2 reserved
   packer region, but the original code deliberately reports only a 1×1 sub-region of that
   reservation — fixed to match exactly (the visual difference would likely have been nil, the
   reserved region is uniformly white either way, but this migration's whole premise is
   behavior-preservation, not "probably fine").
4b. **Only then**, with `AtlasPlacement`/`GlyphCache` proven sufficient for every decide-side
   call site with zero GL-adjacent state in scope, attempt the real thread split: render thread
   does its own full `init_udev` setup from scratch (per the `GlesRenderer: !Send` finding
   above), owning `gpu`/`label_renderer`/the GL-holding `GlyphAtlas` half in a new per-
   `ConsoleHut` side table (keyed by `ConsoleHut`'s own stable id — this codebase already has
   precedent for "core owns the logical entity, a side cache keyed by its id holds render-side
   state," `switcher.rs`'s thumbnail cache and `render.rs`'s per-id content cache, just
   extending it across a thread boundary instead of within one). Core forwards input via channel
   (per the `LibSeatSession: !Send` finding above), and `capture.rs`/session-lock
   confirmation/terminal-content's still-open transport shape (Step 3) convert to the same
   channel pattern dmabuf already uses.

Each step should land independently verified (`cargo build`/`clippy --all-targets`/`test` inside
`nix develop`, plus a live smoke test on `mudhuts --tty` once possible — blocked this session on
the user's GPG key for pushing/rebuilding) before the next begins, on a dedicated branch off
`main`, not on `main` directly, given the scope.

## Explicitly out of scope (for now)

- Multi-GPU rendering (`GpuManager`) — mudhuts is single-GPU throughout, unrelated to this split.
- Moving Wayland protocol dispatch itself off core — settled as staying on core (it's the same
  "external event, mutate scene graph" category as input; see "Target architecture" above).
- A true lock-free/RCU-style publish model for every piece of shared state — the per-field
  `Mutex`/`ArcSwap` primitives already sketched (cursor position/status, tab-strip visibility,
  lock state) are the default; nothing found this session needs anything more exotic than that
  plus `calloop::channel` for request/response.
