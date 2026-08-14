use std::cell::RefCell;
use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::{Bind, Offscreen, Renderer, Texture};
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree};
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{CommitCounter, DamageBag, DamageSnapshot};
use smithay::backend::renderer::{ImportAll, ImportMem, RendererSuper};
use smithay::desktop::space::{Space, SpaceRenderElements, space_render_elements};
use smithay::output::Output;
use smithay::utils::{Buffer, Rectangle, Scale, Transform};

use crate::State;
use crate::chrome::to_color32f;
use crate::console_hut::ConsoleHut;
use crate::space_element::{CompositedTexture, HutSpaceElement, HutSpaceRenderElement, synthetic_output};
use crate::{chrome, docks, switcher, village_chrome};

/// The [`TextureRenderElement`] buffer-scale ([`Element::src`]/
/// [`Element::geometry`]'s shared `self.scale: i32`) that makes a texture
/// rendered pixel-native at real *physical* resolution (mudhuts' own
/// chrome/terminal — see this module's other doc comments on that
/// design) composite at its true on-screen size once the output's scale
/// is non-1.0.
///
/// **This has to be the buffer-scale argument, not an explicit `size`
/// override** — a first attempt at this fix passed `size:
/// Some(logical_size)` instead and looked identical to the original bug
/// (fuzzy, only the top-left corner visible) despite fixing
/// `geometry()`'s *destination* size correctly, because
/// `TextureRenderElement::src()` *also* derives the sampled *source*
/// region from `logical_size()` whenever `src` is left `None` — see
/// `.../src/backend/renderer/element/texture.rs`'s `Element::src` impl:
/// `self.src().to_buffer(self.scale as f64, transform, &logical_size)`.
/// Shrinking `logical_size` via an explicit `size` while leaving
/// `self.scale` (buffer-scale) at its default of 1 shrinks the *sampled*
/// region to match — only the top-left corner of the real texture gets
/// sampled, then stretched back up to fill the (correctly-sized now)
/// destination rect. Passing this as the buffer-scale argument instead
/// fixes both halves of the same underlying contract at once: `src()`
/// un-scales by it (recovering the *full* texture as the sample region)
/// and `geometry()` re-scales by it (landing back on the texture's real
/// physical size) — the mechanism Smithay actually designed for exactly
/// this "buffer is N× the surface's logical size" case.
///
/// Rounds to the nearest whole number (`detect_output_scale` in
/// `udev_backend.rs` only ever produces one anyway) — `scale: i32` is a
/// hard Smithay API constraint here, so a genuinely fractional host scale
/// (winit) loses sub-pixel precision at this specific call site; a real
/// fractional-native fix would need an explicit `src` *and* `size`
/// together (mirroring how `switcher.rs`'s thumbnail already sets both),
/// not attempted here since it's out of scope for the bug this exists to
/// fix.
pub(crate) fn texture_buffer_scale(scale: f64) -> i32 {
    scale.round().max(1.0) as i32
}

/// Scale a hand-tuned chrome constant (padding, margin, handle size, ...)
/// so it stays the same *apparent* size regardless of the output's real
/// DPI scale, instead of the same fixed physical-pixel count everywhere —
/// used by `chrome.rs`/`village_chrome.rs`/`docks.rs`/`switcher.rs`/this
/// module's own tile-pane border, wherever a constant was previously a
/// bare physical-pixel literal.
pub(crate) fn scaled(px: i32, scale: f64) -> i32 {
    ((px as f64) * scale).round() as i32
}

/// Tracks whether some comparable value changed since the last check,
/// bumping a [`CommitCounter`] when it has. Backs real damage tracking for
/// render elements whose *content* (not geometry) can change while kept at
/// a stable `Id` — e.g. a tab's background flipping between active/
/// inactive colors, or its label's text/color changing, all at a fixed
/// on-screen position. Smithay's per-element damage tracker only learns
/// about a content-only change via an explicit commit bump; a
/// `CommitCounter` that never advances (e.g. `CommitCounter::default()`
/// passed fresh every frame) means "never damaged again after the first
/// frame" — the same class of bug as `TextureRenderElement::from_static_texture`
/// (see `ConsoleHut::damage_tracker`'s doc comment), just for
/// `SolidColorRenderElement`/hand-tracked content instead.
pub(crate) struct ChangeTracker<T> {
    last: Option<T>,
    commit: CommitCounter,
}

impl<T: PartialEq> ChangeTracker<T> {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            commit: CommitCounter::default(),
        }
    }

    /// Compare `value` against what was seen last time, bumping the
    /// commit counter if it differs, and return the (possibly
    /// just-bumped) counter to attach to this frame's render element.
    pub(crate) fn commit(&mut self, value: T) -> CommitCounter {
        if self.last.as_ref() != Some(&value) {
            self.commit.increment();
            self.last = Some(value);
        }
        self.commit
    }
}

/// Caches a rendered label texture (from `ConsoleHut::render_label`), only
/// actually re-rendering it when the value identifying its content
/// (title text, active/inactive state, ...) changes since last time.
/// `ConsoleHut::render_label` does real GPU work every call — glyph-atlas
/// lookups plus instanced draw calls into an FBO — and without this
/// cache, `chrome.rs`'s tab strip and `docks.rs`'s dock handles would
/// pay that cost on *every single frame* they're visible, even though a
/// label's text/color essentially never changes between frames. Also
/// backs real damage tracking for the resulting `TextureRenderElement`
/// (via `from_texture_with_damage`), for the same reason
/// [`ChangeTracker`] exists: a snapshot that never advances means "never
/// damaged again after the first frame".
pub(crate) struct LabelCache<T> {
    last: Option<T>,
    texture: Option<GlesTexture>,
    damage: DamageBag<i32, Buffer>,
}

impl<T: PartialEq> LabelCache<T> {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            texture: None,
            damage: DamageBag::default(),
        }
    }

    /// Whether `key` differs from what's cached (or nothing's been
    /// rendered yet) — split from [`Self::store`] (rather than a single
    /// render-and-cache call taking a closure) specifically so the
    /// caller can render a fresh texture via a `&mut self` method of its
    /// *own* (e.g. `ConsoleHut::render_label`) in between the two, without that
    /// call fighting a simultaneous mutable borrow of this cache.
    pub(crate) fn is_stale(&self, key: &T) -> bool {
        self.texture.is_none() || self.last.as_ref() != Some(key)
    }

    /// Store a freshly-rendered texture for `key` (call after
    /// [`Self::is_stale`] returned `true`), marking it fully damaged, and
    /// return it alongside the fresh snapshot.
    pub(crate) fn store(
        &mut self,
        key: T,
        texture: GlesTexture,
    ) -> (GlesTexture, DamageSnapshot<i32, Buffer>) {
        self.damage.add([Rectangle::from_size(texture.size())]);
        self.texture = Some(texture.clone());
        self.last = Some(key);
        (texture, self.damage.snapshot())
    }

    /// The already-cached texture and a damage snapshot for it, for when
    /// [`Self::is_stale`] returned `false`. `None` if nothing's been
    /// rendered yet — shouldn't happen if the caller checked
    /// `is_stale` first, but stays panic-free rather than assumed.
    pub(crate) fn cached(&self) -> Option<(GlesTexture, DamageSnapshot<i32, Buffer>)> {
        let texture = self.texture.clone()?;
        Some((texture, self.damage.snapshot()))
    }
}

// Generic over the renderer `R` (matching the same pattern Smithay's own
// `anvil` demo uses for its `OutputRenderElements`) so every variant is
// expressed in terms of the same `R` consistently — `R::TextureId` here
// resolves to `GlesTexture` once `R` is instantiated as `GlesRenderer` at
// the call site (`winit_backend.rs`), which is the only renderer this
// compositor ever actually uses (see the Phase 2.6 plan notes on why
// GLES rather than a fully renderer-agnostic abstraction).
//
// `Terminal` is also reused for the Alt-Tab popup's thumbnails
// (`switcher.rs`) — they're the same element type (a texture composited
// at an explicit size/location), just wrapping a different ConsoleHut's cached
// texture. `SolidColor` backs the popup's background panel and highlight
// border (`SolidColorRenderElement` isn't generic over `R` at all).
// `Pointer` backs the udev/DRM backend's own compositor-drawn cursor
// (see `cursor.rs`) — unused (never constructed) under the winit
// backend, which relies on the host compositor's own cursor instead.
smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem;
    Space = SpaceRenderElements<R, E>,
    Terminal = TextureRenderElement<<R as RendererSuper>::TextureId>,
    SolidColor = SolidColorRenderElement,
    Pointer = crate::cursor::PointerRenderElement<R>,
}

type Element = OutputRenderElements<GlesRenderer, HutSpaceRenderElement>;

/// Convert a graph node's resolved `Vec<`[`crate::graph::ContentPiece`]`>`
/// into real frame elements — migration step 4's render bridge. `origin`
/// is `State::usable_area()`'s own `(x, y)`, applied to `Texture` pieces
/// only (they arrive local-frame-relative — see `ContentPiece`'s own doc
/// comment); `Window` pieces arrive already-absolute (`ConsoleNode`
/// reads them straight from `ConsoleHut::space`, whose own elements were
/// mapped with this same origin already baked in) and must **not** be
/// translated again here, or a Main Window would render offset twice.
///
/// `Window` pieces are expanded into real render elements right here,
/// not any earlier — `AsRenderElements::render_elements` produces
/// `TextureRenderElement`/`WaylandSurfaceRenderElement`, neither of which
/// implements `Clone` (each owns real per-element damage state with its
/// own `Drop`-time bookkeeping, confirmed against Smithay's pinned
/// source), so they can never be the thing `Graph::resolve_output`'s
/// per-frame memoization cache clones — this function is what turns the
/// still-Clone-safe `ContentPiece::Window` (a bare `Window` + position)
/// into the real, non-Clone elements, exactly once per frame, after
/// caching is no longer a concern. Smithay's own `Window::AsRenderElements`
/// impl already walks `PopupManager::popups_for_surface` internally, so
/// this loses no popup fidelity versus the pre-graph
/// `space_render_elements`-based path, which delegates to the exact same
/// `Window` impl under the hood.
pub(crate) fn content_pieces_to_elements(
    pieces: Vec<crate::graph::ContentPiece>,
    renderer: &mut GlesRenderer,
    origin: (f64, f64),
    scale: f64,
) -> Vec<Element> {
    let mut elements = Vec::new();
    for piece in pieces {
        match piece {
            crate::graph::ContentPiece::Texture { id, texture, damage, position: (x, y) } => {
                let element = TextureRenderElement::from_texture_with_damage(
                    id,
                    renderer.context_id(),
                    (origin.0 + x, origin.1 + y),
                    texture,
                    texture_buffer_scale(scale),
                    Transform::Normal,
                    None,
                    None,
                    None,
                    None,
                    damage,
                    Kind::Unspecified,
                );
                elements.push(OutputRenderElements::from(element));
            }
            crate::graph::ContentPiece::Window { window, position } => {
                let physical_loc = position.to_f64().to_physical_precise_round(Scale::from(scale));
                let window_elements: Vec<HutSpaceRenderElement> =
                    smithay::backend::renderer::element::AsRenderElements::<GlesRenderer>::render_elements(
                        &window,
                        renderer,
                        physical_loc,
                        Scale::from(scale),
                        1.0,
                    );
                // `SpaceRenderElements::Element(Wrap::from(_))`, not a
                // direct `OutputRenderElements::from` — matches exactly
                // how `space_render_elements`'s own internals wrap a
                // `Space`-external render element (checked against
                // Smithay's pinned source): `OutputRenderElements` only
                // has a `From<SpaceRenderElements<R, E>>`, not a direct
                // `From<HutSpaceRenderElement>`.
                elements.extend(window_elements.into_iter().map(|e| {
                    OutputRenderElements::from(SpaceRenderElements::Element(
                        smithay::backend::renderer::element::Wrap::from(e),
                    ))
                }));
            }
        }
    }
    elements
}

/// Stable identity for [`composite_normal_content`]'s single composited
/// wrapper element — a module-level singleton (not a `State`/per-instance
/// field) since there's only ever one "normal content" slot in the whole
/// compositor, matching every other stable-`Id` pattern in this codebase
/// (see e.g. `ConsoleHut::element_id`'s doc comment) in spirit, just
/// without an owning struct instance to hang it off of.
/// Keyed by output index, not a single global slot — real multi-monitor:
/// each output composites its own "normal content" into its own wrapper
/// element, and sharing one `Id`/damage/texture across outputs would
/// make one output's content silently overwrite or misreport damage for
/// another's (mirrors [`HutContentTarget`]/[`HUT_CONTENT`]'s own
/// per-id-keyed shape, just keyed by output index instead of hut id).
fn normal_content_id(output_index: usize) -> Id {
    thread_local! {
        static IDS: RefCell<HashMap<usize, Id>> = RefCell::new(HashMap::new());
    }
    IDS.with(|ids| ids.borrow_mut().entry(output_index).or_insert_with(Id::new).clone())
}

/// Real, persistent damage tracking for [`composite_normal_content`]'s
/// wrapper element — a module-level singleton for the same "one normal
/// content slot" reason [`normal_content_id`] is. **Must** be persistent
/// (not a fresh `DamageBag::default()` built inside `composite_normal_content`
/// itself, which an earlier version of this function did): a brand new
/// `DamageBag` has commit counter `0` and zero recorded damage every single
/// time, and since `normal_content_id()`'s `Id` is stable across frames, the
/// outer per-element damage tracker (`DrmCompositor`, in particular) records
/// "last commit seen for this Id was `0`" the first time it's rendered —
/// every later frame then calls `damage_since(Some(0))` against another
/// fresh, still-`0`, still-empty snapshot and gets back `Some(empty set)`,
/// i.e. *zero* damage, not "unknown, treat as fully damaged" (`None`) the
/// way the removed version's doc comment assumed. That reproduced exactly
/// the "terminal never repaints except when something else forces a frame
/// through" regression: this element looked undamaged on every frame after
/// the first, so a redraw pass driven purely by PTY output (this element's
/// own content changing) queued nothing, while a frame that got *some*
/// other reason to redraw (pointer motion moving the cursor sprite, which
/// carries its own always-changing geometry) still queued a full composite
/// pass and incidentally showed the now-stale-no-longer content, since
/// `content_elements`/`hut.redraw` had kept re-rendering the underlying GPU
/// texture correctly the whole time regardless — only the *reported*
/// damage on this specific wrapper element was wrong. See
/// `ConsoleHut::damage_tracker`'s doc comment for the identical trap in a
/// different guise (`from_static_texture`'s implicit "no damage, ever"),
/// and this module's own `ChangeTracker`/`LabelCache` doc comments, which
/// already state the general rule this violated: "a `CommitCounter` that
/// never advances means 'never damaged again after the first frame'."
fn with_normal_content_damage<T>(output_index: usize, f: impl FnOnce(&mut DamageBag<i32, Buffer>) -> T) -> T {
    thread_local! {
        static DAMAGE: RefCell<HashMap<usize, DamageBag<i32, Buffer>>> = RefCell::new(HashMap::new());
    }
    DAMAGE.with(|damage| f(damage.borrow_mut().entry(output_index).or_default()))
}

/// [`composite_normal_content`]'s offscreen render target — the synthetic
/// output, the GL texture it renders into, and the `OutputDamageTracker`
/// tracking damage across calls, kept alive between frames instead of
/// rebuilt from scratch every single one (an earlier version of this
/// function did exactly that). Profiling a live session (`perf record` on
/// the real `mudhuts --tty` process) after fixing the two correctness bugs
/// this same redesign introduced (see this file's `git log`) turned up a
/// second, independent problem from the same source: rebuilding the
/// `Output` every call meant re-running its `tracing::info!("Creating new
/// Output")` span — Debug-formatting a whole `PhysicalProperties` struct —
/// on every frame (`core::fmt::write`/`String::write_str` showed up as a
/// meaningful chunk of self-time), and rebuilding the GL texture every call
/// meant a real texture alloc/free (with its own sync-fence teardown) every
/// frame too (`delete_textures`, `TextureSync::wait_for_upload`,
/// `eglDestroySync`, `drmSyncobjDestroy` all showed up).
///
/// Worse than either of those alone: a **fresh `OutputDamageTracker` every
/// call** defeats incremental damage tracking entirely for this whole
/// offscreen step, regardless of the per-element damage fed into it —
/// `OutputDamageTracker::render_output` only skips redrawing a region it
/// already believes is correct, and a tracker with no history believes
/// nothing yet. That's why `render_output` itself dominated the profile
/// (~30% of samples, real GPU draw calls underneath it): every element
/// `content_elements` returns was being fully redrawn into this buffer on
/// every single frame, not just the frames where something in it actually
/// changed.
///
/// Rebuilt only when `size`/`scale` genuinely change (output resize/
/// rescale — rare, effectively never at runtime per `State::output_scale`'s
/// own doc comment) rather than every call, the same way the real output
/// and `ConsoleHut::space_output` already avoid rebuilding themselves every
/// frame.
struct NormalContentTarget {
    // Never read directly, but load-bearing: `OutputDamageTracker::from_output`
    // (`.../backend/renderer/damage/mod.rs`) stores only a *weak* handle to
    // the `Output` it's given (`output.downgrade()`), on the assumption
    // that whoever built the tracker keeps the real `Output` alive
    // elsewhere for as long as the tracker itself lives. This field is that
    // "elsewhere" — dropping it (or never storing it here to begin with)
    // would silently degrade every future `render_output` call once the
    // weak handle stops upgrading.
    #[allow(dead_code)]
    output: Output,
    texture: GlesTexture,
    tracker: OutputDamageTracker,
    size: (i32, i32),
    scale: f64,
}

/// One entry's own persistent copy of [`NormalContentTarget`]'s idea —
/// same shape, same staleness-check pattern — but keyed by
/// `ConsoleHut::id` instead of being a single unkeyed slot, so a
/// *backgrounded* top-level Stack entry that's showing a Main Window can
/// have its own last-known content cached too (`NormalContentTarget`
/// itself only ever holds the *focused* entry's content — see
/// `refresh_hut_content_thumbnail`'s doc comment for why this can't just
/// clone that one instead). Real output resolution, not thumbnail-sized —
/// same reason `composite_normal_content`'s own doc comment gives:
/// `space_render_elements`' output uses real-output-absolute coordinates,
/// so a smaller canvas would just show a cropped corner, not a scaled-
/// down view. `switcher.rs` is the one that scales it down for display,
/// the same way it already does for `ConsoleHut::cached_texture`'s
/// terminal texture.
struct HutContentTarget {
    #[allow(dead_code)]
    output: Output,
    texture: GlesTexture,
    tracker: OutputDamageTracker,
    size: (i32, i32),
    scale: f64,
}

thread_local! {
    static HUT_CONTENT: RefCell<HashMap<u64, HutContentTarget>> = RefCell::new(HashMap::new());
}

thread_local! {
    /// Real, persistent per-id damage tracking for [`HUT_CONTENT`]'s
    /// entries — same reasoning as [`with_normal_content_damage`]: a fresh
    /// `DamageBag` reports commit `0` every single call, and since each
    /// entry's `Id` (`ConsoleHut::thumbnail_id`) is stable across frames, the
    /// outer per-element damage tracker would see "commit 0 now, commit 0
    /// last time" after the first frame and stop repainting it — must keep
    /// advancing across calls, not be rebuilt alongside [`HutContentTarget`]
    /// on a staleness reset.
    static HUT_CONTENT_DAMAGE: RefCell<HashMap<u64, DamageBag<i32, Buffer>>> = RefCell::new(HashMap::new());
}

/// Refreshes `hut`'s entry in the per-id thumbnail cache with its current
/// Main-Window-mode content — the *only* place that cache is written, and
/// only ever called while the Alt-Tab popup is actually open
/// (`build_frame_elements`'s `is_previewing()` gate), mirroring the
/// existing per-entry terminal-redraw loop right next to it: nothing here
/// runs on a frame nobody's previewing.
///
/// Syncs `hut`'s own `space` first (`ConsoleHut::sync_main_window_space`)
/// — a backgrounded entry's `space` isn't otherwise kept in sync with its
/// Main Window at all (see this function's call site's doc comment) — then
/// renders it the same way `content_elements`'s own Main-Window branch
/// does, into this entry's own persistent [`HutContentTarget`] rather than
/// the single shared one `composite_normal_content` uses: that one gets
/// re-rendered into on every frame regardless of which Hut is focused, so
/// cloning its texture handle into a per-entry cache would alias — every
/// "cached" entry would silently show whatever's currently focused, not a
/// real snapshot of its own content.
fn refresh_hut_content_thumbnail(
    hut: &mut ConsoleHut,
    renderer: &mut GlesRenderer,
    area_origin: (i32, i32),
    size: (i32, i32),
    scale: f64,
) {
    if size.0 <= 0 || size.1 <= 0 {
        return;
    }
    hut.sync_main_window_space(area_origin);
    let elements = match space_render_elements::<_, HutSpaceElement, _>(renderer, [&hut.space], &hut.space_output, 1.0) {
        Ok(elements) => elements,
        Err(err) => {
            tracing::warn!("failed to collect space elements for Alt-Tab thumbnail: {err}");
            return;
        }
    };
    let buffer_size: smithay::utils::Size<i32, Buffer> = size.into();
    let id = hut.id;

    HUT_CONTENT.with(|cell| {
        let mut cache = cell.borrow_mut();
        let stale = !matches!(cache.get(&id), Some(t) if t.size == size && t.scale == scale);
        if stale {
            let output = synthetic_output("hut-content-thumbnail", size, scale);
            let Ok(texture) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Argb8888, buffer_size)
                .inspect_err(|err| tracing::warn!("failed to create offscreen buffer for a Hut thumbnail: {err}"))
            else {
                return;
            };
            let tracker = OutputDamageTracker::from_output(&output);
            cache.insert(id, HutContentTarget { output, texture, tracker, size, scale });
        }
        let Some(target) = cache.get_mut(&id) else { return };

        let Ok(mut bound) = renderer
            .bind(&mut target.texture)
            .inspect_err(|err| tracing::warn!("failed to bind offscreen buffer for a Hut thumbnail: {err}"))
        else {
            return;
        };
        if let Err(err) = target
            .tracker
            .render_output(renderer, &mut bound, 0, &elements, [0.0, 0.0, 0.0, 0.0])
        {
            tracing::warn!("failed to render a Hut thumbnail offscreen: {err}");
        }
    });
}

/// The texture+damage pair [`refresh_hut_content_thumbnail`] last cached
/// for `id`, if any — `None` for a Hut that's never had its thumbnail
/// refreshed yet (never previewed while showing a Main Window). Marks the
/// whole buffer damaged unconditionally on every call, same justification
/// `composite_normal_content`'s own snapshot has: this only ever runs
/// during an already-demand-driven redraw pass to begin with.
pub(crate) fn hut_thumbnail_texture(id: u64) -> Option<(GlesTexture, DamageSnapshot<i32, Buffer>)> {
    HUT_CONTENT.with(|cell| {
        let cache = cell.borrow();
        let target = cache.get(&id)?;
        let buffer_size: smithay::utils::Size<i32, Buffer> = target.size.into();
        let snapshot = HUT_CONTENT_DAMAGE.with(|damage| {
            let mut damage = damage.borrow_mut();
            let bag = damage.entry(id).or_default();
            bag.add([Rectangle::from_size(buffer_size)]);
            bag.snapshot()
        });
        Some((target.texture.clone(), snapshot))
    })
}

/// Resolve whichever content this tick's focused view actually shows —
/// migration step 4's real cutover point: walks the typed graph
/// (`docs/rfcs/typed-graph-hut.md`) instead of recursing through the old
/// `Hut` enum. **Must be called before the caller's own backend acquires
/// its own borrow of the shared renderer** (`udev_backend.rs`'s
/// `render_surface`, `winit_backend.rs`'s redraw handler) — this
/// internally borrows the exact same `Rc<RefCell<GlesRenderer>>` a
/// backend's own render pass also borrows (see `graph_nodes::RenderEnv`'s
/// own doc comment for why they have to be the same allocation), and
/// `RefCell` panics on a second concurrent borrow. Called once per real
/// frame (the returned `Vec<ContentPiece>` is then threaded through
/// [`build_frame_elements`]), not resolved fresh at every point something
/// needs it.
///
/// Empty while `state.locked` — matches the old pre-graph behavior
/// exactly (`content_elements` was only ever reachable through
/// `composite_normal_content`, itself only called after
/// `build_frame_elements`'s own locked-session early return) — no reason
/// to spend a real GPU redraw on content that's never going to be shown.
///
/// `output_index` selects *which* output's own independent stack to
/// resolve — real multi-monitor: each `OutputSlot` shows its own
/// top-level entry, not necessarily the one the user currently has
/// input focus on (a backgrounded second monitor still renders its own
/// live content every frame). `begin_frame()` (clearing the graph's
/// per-frame memoization cache) is *not* called here — a real multi-
/// output render pass calls this once per output in the same frame, and
/// clearing the cache between them would defeat memoization for any
/// node shared across outputs (none exist yet, but nothing about the
/// graph model rules it out — see the RFC). Callers driving a whole
/// frame across every output call [`crate::graph_stack::GraphStack::begin_frame`]
/// themselves, once, before resolving any of them.
pub fn resolve_frame_content(state: &mut State, output_index: usize) -> Vec<crate::graph::ContentPiece> {
    if state.locked {
        return Vec::new();
    }
    let top = state.stack.focused_top_level_for(output_index);
    state.stack.resolve_content(top)
}

/// Convert `content` (already resolved by [`resolve_frame_content`],
/// *before* `renderer` was ever borrowed — see that function's doc
/// comment) into real frame elements, in the same real-output-*absolute*
/// physical coordinates every branch always used pre-graph: `Texture`
/// pieces arrive local-frame-relative and get `usable_area()`'s own
/// origin applied here (matching `State::active_pane_offset()`'s
/// identical assumption for mouse routing); `Window` pieces arrive
/// already-absolute and don't (see `graph::ContentPiece`'s own doc
/// comment on why the two aren't symmetric).
///
/// Also draws a Tile-Hut's active-pane border highlight — the one piece
/// of `content_elements`'s pre-graph job that stays a `render.rs`-level
/// concern rather than something `TileNode::resolve` itself produces
/// (see `TileNode::highlight_ids`'s own doc comment) — using the exact
/// same `hut::pane_rects` call `TileNode`'s own `resolve`/
/// `resize_to_pixels` already use, so this can never disagree with where
/// a pane actually is.
fn content_elements(
    state: &mut State,
    renderer: &mut GlesRenderer,
    content: Vec<crate::graph::ContentPiece>,
    output_index: usize,
) -> Vec<Element> {
    let area = state.usable_area_for(output_index);
    let scale = state.output_scale_for(output_index);
    let mut elements = content_pieces_to_elements(content, renderer, (area.0 as f64, area.1 as f64), scale);

    let top = state.stack.focused_top_level_for(output_index);
    if let Some(tile) = state.stack.graph().downcast::<crate::graph_nodes::TileNode>(top) {
        let children_len = state.stack.graph().hut_list_input(top, "children").len();
        if children_len >= 2 {
            let fracs = if tile.fracs.len() == children_len { tile.fracs.clone() } else { vec![1.0; children_len] };
            let active = (*tile.active).min(children_len.saturating_sub(1));
            let highlight_ids = tile.highlight_ids.clone();
            let rects = crate::hut::pane_rects(tile.axis, fracs.into_iter(), (area.2, area.3));
            if let Some(&(x, y, w, h)) = rects.get(active) {
                let (x, y) = (x + area.0, y + area.1);
                const BASE_BORDER: i32 = 3;
                let border = scaled(BASE_BORDER, scale).max(1);
                let color = to_color32f(state.theme.tile_border);
                let strips = [
                    (x, y, w, border),
                    (x, y + h - border, w, border),
                    (x, y, border, h),
                    (x + w - border, y, border, h),
                ];
                let mut highlights = Vec::new();
                for (id, (sx, sy, sw, sh)) in highlight_ids.into_iter().zip(strips) {
                    let background = SolidColorRenderElement::new(
                        id,
                        Rectangle::<i32, smithay::utils::Physical>::new(
                            smithay::utils::Point::from((sx, sy)),
                            smithay::utils::Size::from((sw, sh)),
                        ),
                        CommitCounter::default(),
                        color,
                        Kind::Unspecified,
                    );
                    highlights.push(OutputRenderElements::from(background));
                }
                // Frontmost — pushed ahead of the pane content itself, per
                // this module's front-to-back push order (index 0 renders
                // on top), so the border actually renders *above* the
                // active pane's content instead of being hidden behind it.
                elements.splice(0..0, highlights);
            }
        }
    }

    elements
}

/// Composite [`content_elements`]'s current output into one offscreen
/// texture sized to `size` — the *real, full output* size (physical
/// pixels; the same `size` `build_frame_elements` itself already receives
/// from its caller), **not** just `State::usable_area()`'s smaller one.
/// This matters for correctness, not just consistency: `content_elements`'s
/// branches (`ConsoleHut::space`'s Main Window mapping via
/// `State::sync_visible_main_window`, `TileHut::absolute_pane_rects`) all
/// position things in real-output-*absolute* physical coordinates — the
/// same coordinates `input.rs`'s click routing and
/// `handlers/xdg_shell.rs::unconstrain_popup` also depend on staying
/// absolute — so the offscreen canvas they're rendered into has to span
/// the same coordinate range those positions assume, or content silently
/// clips/misplaces itself the moment a layer-shell surface reserves any
/// part of the output (`usable_area()`'s origin stops being `(0, 0)`).
/// `build_frame_elements` maps the finished texture at `(0, 0)` in the
/// real output's own `Space` accordingly — not `usable_area()`'s origin —
/// for the same reason.
///
/// The "normal content" child `build_frame_elements` maps into a `Space`
/// bound to the *real* output, alongside whatever layer-shell surfaces are
/// mapped there, via exactly one `space_render_elements` call. Composable
/// Hut hierarchy RFC migration step 5 sub-step 4 (the Layer-Shell Root
/// Hut, Q2) — the production version of the original step-3 prototype's
/// own `render_offscreen` helper (`git log` for this file's earlier
/// `hut_space.rs` history), minus the CPU readback that only ever existed
/// for that prototype's own byte-diff comparison.
///
/// Transparent clear (`[0.0, 0.0, 0.0, 0.0]`), not opaque — if
/// `content_elements` returns nothing at all (e.g. a transient
/// `ConsoleHut::redraw` failure), this must let whatever's mapped
/// *underneath* it in the real `Space` (a background layer-shell surface)
/// show through, matching exactly what happens today when a content
/// branch simply has nothing to push.
///
/// `None` (logged, not panicked) on any real failure — every one of the
/// GL/allocation calls this makes is fallible for reasons outside this
/// compositor's control (a legitimate GPU-memory shortage, a renderer
/// hiccup), never something to crash the whole compositor over, matching
/// every other offscreen-render call site in this codebase
/// (`handlers/capture.rs::render_capture`).
fn composite_normal_content(
    state: &mut State,
    renderer: &mut GlesRenderer,
    size: (i32, i32),
    content: Vec<crate::graph::ContentPiece>,
    output_index: usize,
) -> Option<CompositedTexture> {
    if size.0 <= 0 || size.1 <= 0 {
        return None;
    }
    let scale = state.output_scale_for(output_index);
    let elements = content_elements(state, renderer, content, output_index);
    let buffer_size: smithay::utils::Size<i32, Buffer> = size.into();

    // Keyed by output index, not a single unkeyed slot — see
    // `normal_content_id`'s own doc comment for why real multi-monitor
    // needs this (mirrors `HUT_CONTENT`'s identical per-key shape).
    thread_local! {
        static TARGETS: RefCell<HashMap<usize, NormalContentTarget>> = RefCell::new(HashMap::new());
    }

    let (texture, snapshot) = TARGETS.with(|cell| {
        let mut targets = cell.borrow_mut();
        let stale = !matches!(targets.get(&output_index), Some(t) if t.size == size && t.scale == scale);
        if stale {
            // Real scale, not `1.0` — see `synthetic_output`'s doc comment:
            // this is the one caller that hands its output straight to
            // `OutputDamageTracker::render_output` rather than a `Space`,
            // so this scale is what turns each element's own baked-in
            // integer buffer scale back into its correct on-screen size.
            // Getting this wrong silently shrinks (or grows) everything
            // composited through it on any real, non-1.0-scale output.
            let output = synthetic_output("normal-content", size, scale);
            let texture = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Argb8888, buffer_size)
                .inspect_err(|err| tracing::warn!("failed to create offscreen buffer for normal content: {err}"))
                .ok()?;
            let tracker = OutputDamageTracker::from_output(&output);
            targets.insert(output_index, NormalContentTarget { output, texture, tracker, size, scale });
        }
        let target = targets.get_mut(&output_index).expect("just ensured Some above");

        let mut bound = renderer
            .bind(&mut target.texture)
            .inspect_err(|err| tracing::warn!("failed to bind offscreen buffer for normal content: {err}"))
            .ok()?;
        target
            .tracker
            .render_output(renderer, &mut bound, 0, &elements, [0.0, 0.0, 0.0, 0.0])
            .inspect_err(|err| tracing::warn!("failed to render normal content offscreen: {err}"))
            .ok()?;
        drop(bound);

        // A real, persistent damage snapshot — see `with_normal_content_damage`'s
        // doc comment for why a fresh `DamageBag::default()` built right
        // here (an earlier version of this function did exactly that) is
        // wrong, not just redundant. Unconditionally marking the whole
        // buffer damaged on every call (rather than tracking finer-grained
        // damage) is still correct, not just simplest: this function only
        // ever runs during an actual demand-driven redraw pass to begin
        // with (Phase 2.6's damage-avoidance discipline lives one level
        // up, in whatever decided to ping `redraw_ping`), so by the time
        // it's called, something real already triggered it.
        let snapshot = with_normal_content_damage(output_index, |damage| {
            damage.add([Rectangle::from_size(buffer_size)]);
            damage.snapshot()
        });

        Some((target.texture.clone(), snapshot))
    })?;

    Some(CompositedTexture::new(normal_content_id(output_index), texture, scale, snapshot))
}

/// Everything mudhuts draws while `state.locked` is set (see
/// `handlers/session_lock.rs`'s module doc) — the *only* thing drawn in
/// that state, replacing every other branch of [`build_frame_elements`]
/// rather than being layered on top of it, since none of that (Alt-Tab
/// popup, chrome, terminal/window content) should be visible, or even
/// get a fresh texture, while the session is locked.
///
/// The locking client's own [`LockSurface`](smithay::wayland::session_lock::LockSurface),
/// once mapped and alive, wins; otherwise (nothing mapped yet, or a
/// stale one that's gone) a plain opaque rectangle covering the whole
/// output, so the screen actually goes blank the instant `state.locked`
/// flips true rather than leaving the last real frame on screen until
/// some client gets around to mapping a lock surface — the protocol
/// requires exactly that blank-or-locked frame to have been presented
/// before `locked()` can even be sent to the client (see
/// `handlers/session_lock.rs`'s `lock` doc comment).
fn lock_screen_elements(
    state: &State,
    renderer: &mut GlesRenderer,
    size: (i32, i32),
) -> Vec<Element> {
    if let Some(surface) = &state.lock_surface
        && surface.alive()
    {
        let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = render_elements_from_surface_tree(
            renderer,
            surface.wl_surface(),
            smithay::utils::Point::<i32, smithay::utils::Physical>::from((0, 0)),
            Scale::from(state.output_scale()),
            1.0,
            Kind::Unspecified,
        );
        if !elems.is_empty() {
            return elems
                .into_iter()
                .map(|e| Element::from(SpaceRenderElements::Surface(e)))
                .collect();
        }
    }

    let background = SolidColorRenderElement::new(
        state.lock_backdrop_id.clone(),
        Rectangle::<i32, smithay::utils::Physical>::new(
            smithay::utils::Point::from((0, 0)),
            smithay::utils::Size::from(size),
        ),
        CommitCounter::default(),
        [0.0, 0.0, 0.0, 1.0],
        Kind::Unspecified,
    );
    vec![Element::from(background)]
}

/// Build one frame's worth of render elements (Alt-Tab popup, tab-strip
/// chrome, docked-handle chrome, then the "normal content" — a Tile-Hut's
/// panes, the terminal, or the focused Console Hut's visible Main
/// Window/Floating Windows/Alerts — layered against the real output's own
/// layer-shell surfaces) in front-to-back order — shared by every backend
/// (`winit_backend.rs`/`udev_backend.rs`) so the element-assembly logic
/// itself is written once. Backend-specific concerns (binding a
/// framebuffer, damage tracking vs. DRM's `render_frame`, submitting/
/// queuing the result) stay in each backend's own module.
/// `output_index` selects which `OutputSlot` this frame is being built
/// for — real multi-monitor: chrome/docks/the Alt-Tab popup all show
/// *that* output's own independent stack state (its own focused entry,
/// its own preview session), not necessarily whichever output currently
/// has input focus, since every output renders its own live content
/// every frame regardless of focus. A single-output session always
/// passes `0`.
pub fn build_frame_elements(
    state: &mut State,
    renderer: &mut GlesRenderer,
    size: (i32, i32),
    content: Vec<crate::graph::ContentPiece>,
    output_index: usize,
) -> Vec<OutputRenderElements<GlesRenderer, HutSpaceRenderElement>> {
    // First, always — regardless of what else this frame does (even a
    // locked session below still has a live renderer here): reclaim
    // whatever GL objects a ConsoleHut closed since the last frame queued
    // for deletion. See `gpu_term::queue_gl_delete`'s doc comment for why
    // this can't happen at the point a ConsoleHut is actually dropped
    // instead (no renderer in scope there).
    crate::gpu_term::drain_pending_gl_deletes(renderer);

    // Checked before every other branch below (switcher, Hut chrome,
    // terminal/window content): a locked session must render *nothing*
    // else, not even layered underneath a lock surface — see
    // `handlers/session_lock.rs`'s module doc on why the protocol
    // requires this before it can even tell the client the lock
    // succeeded. Session lock stays a whole-compositor concept (every
    // output blanks together), not per-output.
    if state.locked {
        let elements = lock_screen_elements(state, renderer, size);
        // See the matching call at this function's other exit point below.
        crate::malloc::trim(0);
        return elements;
    }

    let show_terminal = state.showing_terminal_effective_for(output_index);
    let top = state.stack.focused_top_level_for(output_index);
    let is_tile = state.stack.graph().downcast::<crate::graph_nodes::TileNode>(top).is_some()
        && state.stack.graph().hut_list_input(top, "children").len() >= 2;
    let scale = state.output_scale_for(output_index);
    let mut elements = Vec::new();

    // Only the focused ConsoleHut normally gets redrawn (see Phase 2.6's
    // damage-avoidance work) — but the Alt-Tab popup shows every ConsoleHut's
    // thumbnail, so while it's open they all need fresh cached textures.
    // Redundant with the focused ConsoleHut's own redraw further down (cheap: a
    // second `redraw` call in the same tick is a no-op cache hit, since
    // damage was already reset by the first). Uses the already-borrowed
    // `renderer` parameter directly (safe here — this runs *after*
    // `resolve_frame_content` already released its own internal borrow of
    // the same underlying renderer; see that function's doc comment),
    // not another `graph.resolve_output` call.
    if state.stack.is_previewing_for(output_index) {
        let top_level: Vec<crate::graph::NodeId> = state.stack.top_level_entries_for(output_index).copied().collect();
        for &top in &top_level {
            let leaf = state.stack.graph().focused_leaf(top);
            if let Some(console) = state.stack.graph_mut().downcast_mut::<crate::graph_nodes::ConsoleNode>(leaf) {
                console.hut.redraw(renderer);
            }
        }
        // Same "only while previewing" gate, for entries showing a Main
        // Window instead of their terminal — see
        // `refresh_hut_content_thumbnail`'s doc comment on why this needs
        // its own step (a background entry's `space` isn't otherwise kept
        // in sync at all, and its content isn't cheap to get from
        // anywhere else the way `ConsoleHut::redraw`'s terminal texture
        // already is).
        let (area_x, area_y, _, _) = state.usable_area_for(output_index);
        for &top in &top_level {
            if !state.stack.shows_terminal_effective(top) {
                let leaf = state.stack.graph().focused_leaf(top);
                if let Some(console) =
                    state.stack.graph_mut().downcast_mut::<crate::graph_nodes::ConsoleNode>(leaf)
                {
                    refresh_hut_content_thumbnail(&mut console.hut, renderer, (area_x, area_y), size, scale);
                }
            }
        }
    }

    // Pushed first (frontmost — `render_output`/`render_frame` take
    // elements in front-to-back order) so the popup sits on top of
    // whatever's below, regardless of whether that's the terminal or a
    // client window; empty when no preview session is open.
    elements.extend(switcher::build(&state.stack, output_index, size, renderer, scale));

    // Tile-Hut (Phase 6) still bypasses the normal single-ConsoleHut
    // chrome/docks pipeline entirely — every pane is visible
    // simultaneously, side by side, each always showing its own ConsoleHut's
    // terminal (never a Main Window — see `village.rs`'s module doc on
    // why that's this pass's deliberate scope), so there's no Hut-level
    // tab strip / Main-Window tab strip / dock handle to draw. A 1-child
    // (or empty) Tile never actually exists (`GraphStack::remove_exited`'s
    // collapse rule unwraps it immediately), but the length check
    // (`is_tile`, above) stays as a defensive fallback to the normal
    // pipeline rather than assumed.
    if !is_tile {
        // Hut-level tab strip(s) (Phase 6) — one per Tab-Hut along
        // the active path, stacked from the top of the screen, outermost
        // first (see `village_chrome.rs`'s module doc). Empty unless the
        // focused top-level Hut actually is a Tab-Hut with 2+
        // children; `next_y` is unchanged (0) in that case.
        let cell_w = state.stack.focused_for(output_index).glyphs.cell_width().max(1);
        let cell_h = state.stack.focused_for(output_index).glyphs.cell_height().max(1) as i32;
        let (village_tab_elements, next_y) = village_chrome::build(
            state.stack.graph_mut(),
            top,
            renderer,
            0,
            cell_w,
            cell_h,
            scale,
            &state.theme,
        );
        elements.extend(village_tab_elements);

        // Tab-strip chrome (Phase 4) — pushed below any Hut-level strips
        // above it, still on top of the terminal/window content and still
        // below the Alt-Tab popup above. Empty when the focused ConsoleHut has no
        // Main Windows.
        elements.extend(chrome::build(state.stack.focused_mut_for(output_index), renderer, next_y, scale, &state.theme));

        // Docked Floating Window handles (Phase 5) — same z-order slot as the tab
        // strip, only shown alongside the Main Window they belong to (never
        // while the terminal itself is the visible view).
        if !show_terminal {
            elements.extend(docks::build(state.stack.focused_mut_for(output_index), renderer, size, scale, &state.theme));
        }
    }

    // "Normal content" (Q1/Q2's own term for it) — the Layer-Shell Root
    // Hut's one job: composite whatever `content_elements` currently
    // produces (already built for real inside `composite_normal_content`)
    // into a single texture, map it into a `Space<HutSpaceElement>` bound
    // to the *real* output alongside every layer-shell surface, and pull
    // the whole thing together with one `space_render_elements` call —
    // which handles Background/Bottom-vs-Top/Overlay ordering
    // automatically, the same way it always has for a real, non-synthetic
    // output. Replaces every one of the three content branches' own former
    // hand-rolled `layer_elements` wrap (now deleted) with this one,
    // uniform path. Still done even if `composite_normal_content` itself
    // returns `None` (an offscreen-render failure, or a genuinely empty
    // frame) — layer-shell surfaces still need to render on their own in
    // that case, exactly like they always have.
    //
    // `size` (the real, full output size), not `usable_area()`'s smaller
    // one, and mapped at `(0, 0)`, not `usable_area()`'s own origin — see
    // `composite_normal_content`'s own doc comment on why: the content
    // inside it is already positioned in real-output-absolute coordinates,
    // so the canvas it's rendered onto (and where that canvas then lands)
    // has to match that same coordinate range, not a smaller, re-based one.
    let composited = composite_normal_content(state, renderer, size, content, output_index);
    if let Some(output) = state.stack.outputs().get(output_index).map(|slot| slot.output.clone()) {
        let mut root_space = Space::<HutSpaceElement>::default();
        root_space.map_output(&output, (0, 0));
        if let Some(composited) = composited {
            root_space.map_element(HutSpaceElement::Composited(composited), (0, 0), false);
        }
        match space_render_elements::<_, HutSpaceElement, _>(renderer, [&root_space], &output, 1.0) {
            Ok(space_elements) => {
                elements.extend(space_elements.into_iter().map(OutputRenderElements::from))
            }
            Err(err) => tracing::warn!("failed to collect root space elements: {err}"),
        }
    }

    // Once per render pass, after the pass has actually done its
    // allocating — see `malloc`'s module doc for why a fixed cadence tied
    // to real work (mirroring COSMIC's own `App::view`/`App::update` call
    // sites) matters here, not just calling this occasionally.
    crate::malloc::trim(0);

    elements
}

