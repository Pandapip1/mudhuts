use std::cell::RefCell;
use std::sync::OnceLock;

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
use crate::hut::Hut;
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

/// Stable identity for [`composite_normal_content`]'s single composited
/// wrapper element — a module-level singleton (not a `State`/per-instance
/// field) since there's only ever one "normal content" slot in the whole
/// compositor, matching every other stable-`Id` pattern in this codebase
/// (see e.g. `ConsoleHut::element_id`'s doc comment) in spirit, just
/// without an owning struct instance to hang it off of.
fn normal_content_id() -> Id {
    static ID: OnceLock<Id> = OnceLock::new();
    ID.get_or_init(Id::new).clone()
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
fn with_normal_content_damage<T>(f: impl FnOnce(&mut DamageBag<i32, Buffer>) -> T) -> T {
    thread_local! {
        static DAMAGE: RefCell<DamageBag<i32, Buffer>> = RefCell::new(DamageBag::default());
    }
    DAMAGE.with(|damage| f(&mut damage.borrow_mut()))
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

/// Whichever content this tick's focused view actually shows — a
/// Tile-Hut's panes, the terminal, or the focused Console Hut's own
/// `space` — built the same way each of `build_frame_elements`'s three
/// branches always has, in the same real-output-*absolute* physical
/// coordinates they always have (the terminal at literal `(0, 0)`; a
/// Tile-Hut's panes and a Console Hut's Main Window/Floating
/// Windows/Alerts at their own `usable_area()`-offset positions) — *not*
/// re-based to be local to some smaller canvas. See
/// [`composite_normal_content`]'s own doc comment on why that has to stay
/// true all the way through to the offscreen texture this gets rendered
/// into.
fn content_elements(state: &mut State, renderer: &mut GlesRenderer) -> Vec<Element> {
    if matches!(state.stack.focused_top_level(), Hut::Tile(tile) if tile.children.len() >= 2) {
        return build_tile_elements(state, renderer);
    }

    if state.showing_terminal_effective() {
        let scale = state.output_scale();
        let hut = state.stack.focused_mut();
        let Some(texture) = hut.redraw(renderer) else {
            return Vec::new();
        };
        // `from_texture_with_damage`, not `from_static_texture` — the
        // terminal's content genuinely changes every keystroke, and
        // `from_static_texture` is documented by Smithay as creating an
        // element with no damage tracking at all (see
        // `ConsoleHut::damage_tracker`'s doc comment).
        //
        // Buffer scale, not an explicit `size` — see
        // `texture_buffer_scale`'s doc comment for why leaving this `1`
        // double-applies the output's scale once it's non-1.0, and why an
        // explicit `size` alone isn't the right fix either.
        let element = TextureRenderElement::from_texture_with_damage(
            hut.element_id.clone(),
            renderer.context_id(),
            (0.0, 0.0),
            texture,
            texture_buffer_scale(scale),
            Transform::Normal,
            None,
            None,
            None,
            None,
            hut.element_damage_snapshot(),
            Kind::Unspecified,
        );
        return vec![OutputRenderElements::from(element)];
    }

    let hut = state.stack.focused_mut();
    match space_render_elements::<_, HutSpaceElement, _>(renderer, [&hut.space], &hut.space_output, 1.0) {
        Ok(space_elements) => space_elements.into_iter().map(OutputRenderElements::from).collect(),
        Err(err) => {
            tracing::warn!("failed to collect space elements: {err}");
            Vec::new()
        }
    }
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
) -> Option<CompositedTexture> {
    if size.0 <= 0 || size.1 <= 0 {
        return None;
    }
    let scale = state.output_scale();
    let elements = content_elements(state, renderer);
    let buffer_size: smithay::utils::Size<i32, Buffer> = size.into();

    thread_local! {
        static TARGET: RefCell<Option<NormalContentTarget>> = const { RefCell::new(None) };
    }

    let (texture, snapshot) = TARGET.with(|cell| {
        let mut slot = cell.borrow_mut();
        let stale = !matches!(slot.as_ref(), Some(t) if t.size == size && t.scale == scale);
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
            *slot = Some(NormalContentTarget { output, texture, tracker, size, scale });
        }
        let target = slot.as_mut().expect("just ensured Some above");

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
        let snapshot = with_normal_content_damage(|damage| {
            damage.add([Rectangle::from_size(buffer_size)]);
            damage.snapshot()
        });

        Some((target.texture.clone(), snapshot))
    })?;

    Some(CompositedTexture::new(normal_content_id(), texture, scale, snapshot))
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
pub fn build_frame_elements(
    state: &mut State,
    renderer: &mut GlesRenderer,
    size: (i32, i32),
) -> Vec<OutputRenderElements<GlesRenderer, HutSpaceRenderElement>> {
    // Checked before every other branch below (switcher, Hut chrome,
    // terminal/window content): a locked session must render *nothing*
    // else, not even layered underneath a lock surface — see
    // `handlers/session_lock.rs`'s module doc on why the protocol
    // requires this before it can even tell the client the lock
    // succeeded.
    if state.locked {
        return lock_screen_elements(state, renderer, size);
    }

    let show_terminal = state.showing_terminal_effective();
    let is_tile = matches!(state.stack.focused_top_level(), Hut::Tile(tile) if tile.children.len() >= 2);
    let scale = state.output_scale();
    let mut elements = Vec::new();

    // Only the focused ConsoleHut normally gets redrawn (see Phase 2.6's
    // damage-avoidance work) — but the Alt-Tab popup shows every ConsoleHut's
    // thumbnail, so while it's open they all need fresh cached textures.
    // Redundant with the focused ConsoleHut's own redraw further down (cheap: a
    // second `redraw` call in the same tick is a no-op cache hit, since
    // damage was already reset by the first).
    if state.stack.is_previewing() {
        for hut in state.stack.top_level_huts_mut() {
            hut.redraw(renderer);
        }
    }

    // Pushed first (frontmost — `render_output`/`render_frame` take
    // elements in front-to-back order) so the popup sits on top of
    // whatever's below, regardless of whether that's the terminal or a
    // client window; empty when no preview session is open.
    elements.extend(switcher::build(&state.stack, size, renderer, scale));

    // Tile-Hut (Phase 6) still bypasses the normal single-ConsoleHut
    // chrome/docks pipeline entirely — every pane is visible
    // simultaneously, side by side, each always showing its own ConsoleHut's
    // terminal (never a Main Window — see `village.rs`'s module doc on
    // why that's this pass's deliberate scope), so there's no Hut-level
    // tab strip / Main-Window tab strip / dock handle to draw. A 1-child
    // (or empty) Tile never actually exists (`Hut::collapse_if_singleton`
    // unwraps it immediately), but the length check (`is_tile`, above)
    // stays as a defensive fallback to the normal pipeline rather than
    // assumed.
    if !is_tile {
        // Hut-level tab strip(s) (Phase 6) — one per Tab-Hut along
        // the active path, stacked from the top of the screen, outermost
        // first (see `village_chrome.rs`'s module doc). Empty unless the
        // focused top-level Hut actually is a Tab-Hut with 2+
        // children; `next_y` is unchanged (0) in that case.
        let cell_w = state.stack.focused().glyphs.cell_width().max(1);
        let cell_h = state.stack.focused().glyphs.cell_height().max(1) as i32;
        let (village_tab_elements, next_y) =
            village_chrome::build(state.stack.focused_top_level_mut(), renderer, 0, cell_w, cell_h, scale);
        elements.extend(village_tab_elements);

        // Tab-strip chrome (Phase 4) — pushed below any Hut-level strips
        // above it, still on top of the terminal/window content and still
        // below the Alt-Tab popup above. Empty when the focused ConsoleHut has no
        // Main Windows.
        elements.extend(chrome::build(state.stack.focused_mut(), renderer, next_y, scale));

        // Docked Floating Window handles (Phase 5) — same z-order slot as the tab
        // strip, only shown alongside the Main Window they belong to (never
        // while the terminal itself is the visible view).
        if !show_terminal {
            elements.extend(docks::build(state.stack.focused_mut(), renderer, size, scale));
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
    let composited = composite_normal_content(state, renderer, size);
    if let Some(output) = state.output.clone() {
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

    elements
}

/// Build a Tile-Hut's panes side by side: each child's own terminal
/// texture (already sized to its pane by `Hut::resize_to_pixels` — see
/// that method and [`crate::hut::pane_rects`]), composited at its pane's
/// screen position via a private `Space<HutSpaceElement>` (composable Hut
/// hierarchy RFC migration step 5 sub-step 3 — each pane's texture is a
/// `HutSpaceElement::Composited`, mapped and rendered the same generic way
/// `ConsoleHut::space` already composites its own content, rather than
/// hand-rolled `TextureRenderElement` construction), plus a highlight
/// border around whichever pane currently has keyboard focus. Only called
/// once the caller's confirmed the focused top-level Hut really is a
/// `Hut::Tile` with 2+ children.
///
/// The `Space` here is a fresh, per-call local, *not* a persistent
/// `TileHut` field the way `ConsoleHut::space` is — unlike a Console Hut's
/// Main Window (a real, persistent `Window` `sync_visible_main_window`
/// only remaps on focus/visibility changes), every pane's content here is
/// an ephemeral, single-use `CompositedTexture` rebuilt fresh every frame
/// regardless (v1 scope: a pane only ever shows its Console Hut's
/// terminal, never a real Main Window — see `hut.rs`'s module doc), so
/// there's no frame-to-frame state actually worth keeping — mirrors
/// `hut_space.rs`'s original step-3 prototype's own local-`Space` pattern,
/// now for real.
///
/// Panes are normal content — like the terminal-visible branch above,
/// sized and positioned against [`State::usable_area`], not the raw
/// output rect. Gets its actual rects from [`crate::hut::TileHut::absolute_pane_rects`],
/// the same computation `State::active_pane_offset` and
/// `input.rs::try_click_chrome`'s pane hit-test share (composable Hut
/// hierarchy RFC's Q3) — the three can never disagree about where a pane
/// actually is, since there's only one computation left to disagree with
/// itself. The pane `Space`'s own synthetic output is always mapped at
/// `(0, 0)` within it (same reasoning as `ConsoleHut::space_output`), so
/// mapping each pane's element at its *absolute* screen position (not
/// re-derived relative to the pane `Space`'s own origin) still comes out
/// byte-identical to what direct `TextureRenderElement` construction
/// produced before — confirmed against `Space::render_elements_for_region`'s
/// own math (it subtracts the output's own location, which is zero here).
fn build_tile_elements(
    state: &mut State,
    renderer: &mut GlesRenderer,
) -> Vec<OutputRenderElements<GlesRenderer, HutSpaceRenderElement>> {
    let area = state.usable_area();
    let scale = state.output_scale();
    let Hut::Tile(tile) = state.stack.focused_top_level_mut() else {
        return Vec::new();
    };
    let rects = tile.absolute_pane_rects(area);
    let active = tile.active;
    let highlight_ids = tile.highlight_ids.clone();

    let mut elements = Vec::new();

    // Pushed first — frontmost, per this module's front-to-back push
    // order (index 0 renders on top) — so the border actually renders
    // *above* the active pane's content instead of being hidden behind
    // it. A border, not a filled rect: four thin solid-color strips
    // around the pane's edges, since a single filled rectangle would
    // just hide its content instead of framing it.
    if let Some(&(x, y, w, h)) = rects.get(active) {
        const BASE_BORDER: i32 = 3;
        let border = scaled(BASE_BORDER, scale).max(1);
        let color = [0.3, 0.6, 1.0, 1.0];
        let strips = [
            (x, y, w, border),                // top
            (x, y + h - border, w, border),   // bottom
            (x, y, border, h),                // left
            (x + w - border, y, border, h),   // right
        ];
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
            elements.push(OutputRenderElements::from(background));
        }
    }

    let (_, _, area_w, area_h) = area;
    // Cached rather than rebuilt every call — same reasoning as
    // `NormalContentTarget`'s doc comment (a fresh `Output` every frame
    // means a real allocation plus a Debug-formatted `tracing::info!` span
    // on every single Tile-Hut frame); this one has no GL texture or damage
    // tracker riding on it, just the `Output` itself.
    thread_local! {
        static PANE_OUTPUT: RefCell<Option<(Output, (i32, i32))>> = const { RefCell::new(None) };
    }
    let pane_output = PANE_OUTPUT.with(|cell| {
        let mut slot = cell.borrow_mut();
        if !matches!(slot.as_ref(), Some((_, size)) if *size == (area_w, area_h)) {
            *slot = Some((synthetic_output("tile-hut-space", (area_w, area_h), 1.0), (area_w, area_h)));
        }
        slot.as_ref().expect("just ensured Some above").0.clone()
    });
    let mut pane_space = Space::<HutSpaceElement>::default();
    pane_space.map_output(&pane_output, (0, 0));
    for ((child, _), (x, y, _, _)) in tile.children.iter_mut().zip(rects) {
        let hut = child.focused_hut_mut();
        let Some(texture) = hut.redraw(renderer) else {
            continue;
        };
        let composited = CompositedTexture::new(
            hut.element_id.clone(),
            texture,
            scale,
            hut.element_damage_snapshot(),
        );
        // Absolute (real screen) coordinates, not re-derived relative to
        // `pane_space`'s own origin — see this function's doc comment on
        // why that's still correct: `pane_output` is always mapped at
        // `(0, 0)` within `pane_space`.
        pane_space.map_element(HutSpaceElement::Composited(composited), (x, y), false);
    }
    match space_render_elements::<_, HutSpaceElement, _>(renderer, [&pane_space], &pane_output, 1.0) {
        Ok(space_elements) => {
            elements.extend(space_elements.into_iter().map(OutputRenderElements::from))
        }
        Err(err) => tracing::warn!("failed to collect tile-pane space elements: {err}"),
    }

    elements
}
