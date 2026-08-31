use std::sync::Arc;

use mudhuts_term::keys::{Key, Mods, NamedKey};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::desktop::layer_map_for_output;
use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState, keysyms};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, Rectangle, SERIAL_COUNTER, Scale, Serial};
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
use smithay::wayland::selection::data_device::set_data_device_selection;
use smithay::wayland::selection::primary_selection::set_primary_selection;
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer};

use crate::State;
use crate::keybindings::Action;
use crate::{chrome, docks, village_chrome};

/// Mime types mudhuts advertises for every selection it sets itself
/// (clipboard or primary) — it only ever offers plain text, so this fixed
/// list is enough; no real format negotiation needed.
fn text_mime_types() -> Vec<String> {
    vec![
        "text/plain;charset=utf-8".to_string(),
        "UTF8_STRING".to_string(),
        "text/plain".to_string(),
    ]
}

/// Translate a raw xkb keysym into mudhuts-term's neutral [`Key`], or
/// `None` for keys that don't map to a PTY-input action on their own
/// (bare modifiers, etc.) — these still update xkb's internal modifier
/// state via `keyboard.input()`, just produce no bytes.
fn named_key(sym: u32) -> Option<NamedKey> {
    Some(match sym {
        keysyms::KEY_Return | keysyms::KEY_KP_Enter => NamedKey::Enter,
        keysyms::KEY_Escape => NamedKey::Escape,
        keysyms::KEY_BackSpace => NamedKey::Backspace,
        keysyms::KEY_Tab => NamedKey::Tab,
        keysyms::KEY_ISO_Left_Tab => NamedKey::Tab,
        keysyms::KEY_Home => NamedKey::Home,
        keysyms::KEY_End => NamedKey::End,
        keysyms::KEY_Prior => NamedKey::PageUp,
        keysyms::KEY_Next => NamedKey::PageDown,
        keysyms::KEY_Insert => NamedKey::Insert,
        keysyms::KEY_Delete => NamedKey::Delete,
        keysyms::KEY_Up => NamedKey::Up,
        keysyms::KEY_Down => NamedKey::Down,
        keysyms::KEY_Left => NamedKey::Left,
        keysyms::KEY_Right => NamedKey::Right,
        keysyms::KEY_F1 => NamedKey::F(1),
        keysyms::KEY_F2 => NamedKey::F(2),
        keysyms::KEY_F3 => NamedKey::F(3),
        keysyms::KEY_F4 => NamedKey::F(4),
        keysyms::KEY_F5 => NamedKey::F(5),
        keysyms::KEY_F6 => NamedKey::F(6),
        keysyms::KEY_F7 => NamedKey::F(7),
        keysyms::KEY_F8 => NamedKey::F(8),
        keysyms::KEY_F9 => NamedKey::F(9),
        keysyms::KEY_F10 => NamedKey::F(10),
        keysyms::KEY_F11 => NamedKey::F(11),
        keysyms::KEY_F12 => NamedKey::F(12),
        _ => return None,
    })
}

fn mods_from(m: &ModifiersState) -> Mods {
    Mods {
        shift: m.shift,
        alt: m.alt,
        ctrl: m.ctrl,
        logo: m.logo,
    }
}

// Standard Linux evdev button codes (linux/input-event-codes.h), what
// `PointerButtonEvent::button_code()` reports.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// How much accumulated `PointerAxis` `vertical_amount` (the same
/// physical-pixel-ish unit libinput/Wayland's `wp_pointer.axis` always
/// uses) counts as one discrete wheel "click" — matches the 15px/click
/// convention this file already assumes elsewhere (the `amount_v120`
/// fallback that synthesizes a continuous amount from discrete-only
/// devices). Used to gate both scrollback-view line-scrolling and
/// SGR mouse-wheel reports to a TUI app that's grabbed the mouse — see
/// `PointerAxis`'s handler for why gating matters at all.
const WHEEL_CLICK_PX: f64 = 15.0;

/// Map an evdev button code to the xterm mouse-reporting button number, or
/// `None` for buttons that don't have one (reporting is just skipped then).
fn xterm_button(code: u32) -> Option<u32> {
    match code {
        BTN_LEFT => Some(mudhuts_term::mouse::BUTTON_LEFT),
        BTN_MIDDLE => Some(mudhuts_term::mouse::BUTTON_MIDDLE),
        BTN_RIGHT => Some(mudhuts_term::mouse::BUTTON_RIGHT),
        _ => None,
    }
}

/// Fire-and-forget a brightness/volume media-key command (`Action::Brightness*`/
/// `Action::Volume*`) — logs a warning rather than failing loudly if the
/// tool isn't installed (`brightnessctl`/`wpctl`, matching what most
/// minimal Wayland compositors shell out to already), same "degrade, don't
/// crash" convention as every other fallible action in this codebase.
/// `.spawn()`, not `.status()`/`.output()` — never blocks the compositor
/// waiting for the command to finish.
fn spawn_media_command(program: &str, args: &[&str]) {
    if let Err(err) = std::process::Command::new(program).args(args).spawn() {
        tracing::warn!("failed to run {program} {args:?}: {err}");
    }
}

/// Resolve a key press into terminal input bytes, using the live xkb state
/// for UTF-8 text (never the stateless `keysym_to_utf8`, which can panic on
/// pathological input — see `xkbcommon::xkb::State::key_get_utf8`).
fn encode(
    keysym: &KeysymHandle<'_>,
    mods: &ModifiersState,
    mode: alacritty_terminal::term::TermMode,
) -> Option<Vec<u8>> {
    let sym = keysym.modified_sym();
    let key = if let Some(named) = named_key(sym.raw()) {
        Key::Named(named)
    } else {
        let text = match keysym.xkb().lock() {
            // Safety: the reference returned by `state()` is used only for
            // this immediate call and does not outlive `xkb` (the mutex
            // guard), which owns it here.
            Ok(xkb) => unsafe { xkb.state() }.key_get_utf8(keysym.raw_code()),
            Err(_) => return None,
        };
        let c = text.chars().next()?;
        Key::Text(c)
    };
    mudhuts_term::keys::encode(key, mods_from(mods), mode)
}

/// Clamps `new_location` first to `bounds` (the virtual desktop's own
/// bounding hull), then — if that's not enough to land inside any real
/// output's own rect (a "dead zone" between two different-sized/
/// positioned outputs) — snaps to the nearest point on whichever real
/// output rect is geometrically closest, rather than leaving it
/// somewhere no real output actually covers. Deliberately not resolved
/// via "whichever output is currently focused": that could be a
/// completely different, geometrically distant monitor, and snapping
/// into it from a dead zone would teleport the cursor across the whole
/// virtual desktop instead of stopping at the nearest real edge. Pulled
/// out of `PointerMotion`'s relative-delta handling as a pure function
/// over `Point`/`Rectangle` — this is the densest, most failure-prone
/// geometry in this file (float comparisons, an empty/tie `min_by`, an
/// "already contained" fast path) and was previously untestable without
/// a live multi-monitor `GraphStack`.
fn clamp_to_nearest_output(
    new_location: Point<f64, Logical>,
    bounds: Rectangle<f64, Logical>,
    output_rects: impl Iterator<Item = Rectangle<f64, Logical>> + Clone,
) -> Point<f64, Logical> {
    let mut new_location = new_location;
    new_location.x = new_location.x.clamp(bounds.loc.x, (bounds.loc.x + bounds.size.w).max(bounds.loc.x));
    new_location.y = new_location.y.clamp(bounds.loc.y, (bounds.loc.y + bounds.size.h).max(bounds.loc.y));

    // Cheap common-case check first: if `new_location` is already
    // genuinely inside some real output's rect (the overwhelming
    // majority of motion samples), this is a no-op and the more
    // expensive per-output nearest-point search below never runs at all.
    // `output_rects` is a generic `impl Iterator`, not a collected `Vec`
    // — this is real hot-path code (every relative pointer-motion
    // sample), and a version of this extraction that collected into a
    // `Vec` first added a heap allocation to every single event that
    // wasn't there before (caught in review) — `Clone` lets it be walked
    // twice (this check, then the nearest-point search below) without
    // ever materializing one.
    if output_rects.clone().any(|rect| rect.contains(new_location)) {
        return new_location;
    }

    let nearest = output_rects
        .map(|rect| {
            let cx = new_location.x.clamp(rect.loc.x, (rect.loc.x + rect.size.w).max(rect.loc.x));
            let cy = new_location.y.clamp(rect.loc.y, (rect.loc.y + rect.size.h).max(rect.loc.y));
            let dist = (cx - new_location.x).hypot(cy - new_location.y);
            (dist, Point::from((cx, cy)))
        })
        .min_by(|(a, _), (b, _)| a.total_cmp(b));
    if let Some((_, clamped)) = nearest {
        new_location = clamped;
    }
    new_location
}

/// Accumulates `delta` into `accum`, extracts however many whole `unit`-
/// sized steps have built up (truncated toward zero), and carries the
/// remainder back — the same "accumulate continuous motion into discrete
/// steps, don't just treat every single event as one step" shape backs
/// both SGR mouse-wheel click reporting and this ConsoleHut's own
/// scrollback stepping (see `PointerAxis`'s own comments on why a naive
/// per-event step made a gentle trackpad swipe register as dozens of
/// clicks). Returns `(new_accum, whole_units)`. Pure over primitives, so
/// this carry-the-remainder arithmetic — easy to get subtly wrong,
/// e.g. forgetting to subtract the *consumed* portion back out, or with
/// the wrong sign — is directly testable without a live terminal/pointer.
fn accumulate_discrete_units(accum: f64, delta: f64, unit: f64) -> (f64, i32) {
    let accum = accum + delta;
    let whole_units = (accum / unit).trunc() as i32;
    (accum - whole_units as f64 * unit, whole_units)
}

/// Which pane index (if any) `pixel` lands inside, given `rects` — each
/// `(x, y, w, h)`, the same shape `hut::pane_rects` returns — in the same
/// order they're enumerated. Pulled out of `try_click_chrome`'s Tile-pane
/// hit-test as a pure function so the round-to-pixel-then-contains logic
/// that turns a raw click into a pane index is directly testable, same
/// reasoning as `chrome.rs`'s `tab_row_layout` already being shared
/// between drawing and hit-testing (`village_chrome.rs`'s `build`/
/// `handle_click`, `chrome.rs`'s own `tab_layout`).
fn hit_pane_index(pixel: Point<i32, Physical>, rects: impl Iterator<Item = (i32, i32, i32, i32)>) -> Option<usize> {
    rects
        .enumerate()
        .find_map(|(i, (x, y, w, h))| (pixel.x >= x && pixel.x < x + w && pixel.y >= y && pixel.y < y + h).then_some(i))
}

/// The order `exclusive_layer_surface` checks outputs in: the focused one
/// first (real keystrokes — including an on-screen lock/PIN entry — must
/// go to whichever output the user is actually looking at, not whichever
/// client happened to request `exclusive` first), then every other output
/// in its own original relative order. `focused_index >= count` (should
/// never happen in practice, but not assumed) just means nothing is
/// checked first. Pulled out as a pure function over indices so this
/// ordering guarantee is directly testable without constructing real
/// layer-shell surfaces/outputs.
fn exclusive_search_order(focused_index: usize, count: usize) -> impl Iterator<Item = usize> {
    // A lazy chain, not a collected `Vec` — matches `clamp_to_nearest_
    // output`'s own reasoning (see its doc comment on a prior version of
    // this whole extraction pass adding needless allocations to
    // input-dispatch paths that never had any before).
    std::iter::once(focused_index).filter(move |&i| i < count).chain((0..count).filter(move |&i| i != focused_index))
}

/// The tab-strip auto-hide state machine's core decision (see
/// `update_tab_strip_reveal`'s own doc comment for the feature this
/// backs): reveal instantly on touching the top edge; while already
/// revealed, stay revealed only as long as the pointer remains within
/// the strip's own height; otherwise stay hidden. `strip_height` is
/// `None` exactly when it's irrelevant to the outcome (the edge-touch
/// branch always wins regardless of it, and the not-currently-revealed
/// branch never reads it either) — `update_tab_strip_reveal`'s own
/// caller only computes the real height (a real graph traversal, not
/// free) on the one branch that actually needs it, preserved here as
/// `None` rather than forcing every caller to compute it unconditionally
/// just to satisfy this function's own signature.
fn tab_strip_reveal_state(pointer_y: f64, edge_reveal_px: f64, currently_revealed: bool, strip_height: Option<f64>) -> bool {
    if pointer_y <= edge_reveal_px {
        true
    } else if currently_revealed {
        strip_height.is_some_and(|h| pointer_y <= h)
    } else {
        false
    }
}

impl State {
    /// Current keyboard modifier state, for pointer-driven actions (mouse
    /// reporting, wheel reporting) that don't get it for free the way
    /// keyboard events do.
    fn current_mods(&self) -> Mods {
        let raw = self
            .seat
            .get_keyboard()
            .map(|kb| kb.modifier_state())
            .unwrap_or_default();
        mods_from(&raw)
    }

    /// Convert a genuinely-Logical seat position (`pointer.current_location()`,
    /// or anything already headed into `self.surface_under`/`pointer.motion`)
    /// into mudhuts' own physical-pixel rendering space — the space
    /// `try_click_chrome`, `docks::start_drag`, and the terminal's
    /// `pixel_to_cell` all expect, matching `output_size`/`focused_usable_area()`.
    /// See `handle_pointer_motion`'s doc comment for why both spaces are
    /// needed at once rather than picking just one.
    fn to_physical(&self, pos: Point<f64, Logical>) -> Point<f64, Physical> {
        pos.to_physical(Scale::from(self.focused_output_scale()))
    }

    /// Shared tail of pointer-motion handling, called from both
    /// `InputEvent::PointerMotionAbsolute` (winit: the host already gives
    /// an absolute position) and `InputEvent::PointerMotion` (real
    /// hardware under the udev/libinput backend: relative deltas,
    /// accumulated and clamped by the caller before reaching here) —
    /// everything past "here's the new absolute position" is identical
    /// either way.
    ///
    /// `pos` is genuinely Logical (both callers now derive it from
    /// `State::focused_real_output_geometry`/its own scale-divided bounds — see
    /// `InputEvent::PointerMotionAbsolute`/`PointerMotion` below), which
    /// is what `self.surface_under`/`pointer.motion` need: Smithay's own
    /// `Space`/layer-shell hit-testing, and the position a client's
    /// `wl_pointer.motion` ultimately gets, are both Logical throughout.
    /// But this function *also* drives mudhuts' own physical-pixel-native
    /// hit-testing (the terminal's `pixel_to_cell`, `docks::advance_drag`)
    /// — those get a locally-converted physical copy instead of Logical
    /// `pos` directly, rather than picking just one space for everything
    /// (see `State::focused_usable_area`'s doc comment on why physical is what
    /// mudhuts' own rendering needs).
    fn handle_pointer_motion(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Logical>, time: u32) {
        // `pos` arrives genuinely global (`State::pointer_location`'s own
        // doc comment — real multi-monitor's shared compositor space):
        // `PointerMotionAbsolute` derives it from `focused_real_output_geometry`,
        // always `(0, 0)`-rooted since winit is genuinely single-output
        // there (global and local coincide); `PointerMotion` derives it
        // from `GraphStack::virtual_bounding_box`, genuinely global for
        // real multi-monitor hardware.
        self.pointer_location = pos;
        // Real multi-monitor: focus follows the mouse across outputs (the
        // user's resolved policy — see `GraphStack::output_index_at`'s doc
        // comment). A no-op the moment the pointer stays within the
        // already-focused output's own rect, which is the overwhelming
        // majority of motion events.
        let output_index = self.stack.output_index_at(pos);
        if output_index != self.stack.focused_output_index() {
            self.stack.set_focused_output(output_index);
            self.sync_focused_output(); // also resets `tab_strip_revealed` — see its own doc comment
            // `sync_visible_main_window`'s own doc comment: call after
            // anything that changes which ConsoleHut is focused. Focus-
            // follows-mouse does exactly that (`self.stack.focused()`
            // now resolves through the newly-focused output), and the
            // very next thing this function does is hit-test against
            // that Hut's `space` (`self.surface_under(pos)` below) — a
            // backgrounded Hut's `space` can be stale, so skipping this
            // left the first hit-test after landing on a new monitor
            // liable to resolve against a stale mapped-window set.
            self.sync_visible_main_window();
        }
        // Everything past this point — `self.surface_under`, the
        // terminal's own physical-pixel-native hit-testing, and the real
        // `wl_pointer.motion` event itself (which also becomes
        // `pointer.current_location()`, what `try_click_chrome`/
        // `try_click_layer_surface` read on the next button press) —
        // needs a position *local* to the now-focused output's own
        // `(0, 0)` origin, matching how `hut.space`/`layer_map_for_output`
        // position their own elements (see `GraphStack::virtual_bounding_box`'s
        // doc comment for the mirror-image concern this pairs with).
        // Feeding it the raw global `pos` instead — on any output not
        // sitting at global `(0, 0)` — hit-tested/mapped every one of
        // these against coordinates far outside that output's own real
        // bounds, breaking clicks/hover/selection there entirely.
        let output_position = self.stack.output_position(output_index);
        // Subtract once, in Logical space, then convert what's already
        // rebased — not a second copy of `docks::rebase_to_output_
        // physical`'s fragile subtract-*then*-convert formula (see its
        // own doc comment on why getting that order backwards is the
        // real risk): chaining a plain `.to_physical` onto an already-
        // rebased `pos` can't independently re-derive the wrong order,
        // since there's nothing left here to combine — unlike calling
        // that shared function with the still-global `pos`, which would
        // subtract `output_position` a second time internally (caught in
        // review on an earlier version of this fix).
        let pos = pos - output_position.to_f64();
        let pos_physical = self.to_physical(pos);
        // Under winit this is a no-op ping (the host draws the cursor,
        // and `winit_backend.rs`'s own input handler already force-
        // redraws on every input event regardless) — but the udev
        // backend draws its own compositor-side cursor (`cursor.rs`) at
        // `pointer_location`, and its render loop is otherwise purely
        // demand-driven (see `udev_backend.rs`'s module doc): without
        // this, plain pointer motion with no other side effect (no
        // button/text-selection/PTY activity) never triggers a redraw at
        // all, so the drawn cursor only catches up to its real position
        // whenever something unrelated happens to force one.
        self.request_redraw();
        if self.chrome_config.auto_hide_tab_strip {
            self.update_tab_strip_reveal(pos_physical.y);
        }
        let serial = SERIAL_COUNTER.next_serial();

        if self.dock_drag.is_some() {
            // Genuinely global (`self.pointer_location`, not the
            // just-rebased-local `pos`/`pos_physical`) — `advance_drag`
            // itself rebases to Physical local to the *drag's own*
            // output, which can differ from whichever output currently
            // has focus mid-drag. See its own doc comment.
            let global_pos = self.pointer_location;
            docks::advance_drag(self, global_pos);
        }

        if self.focused_showing_terminal_effective() {
            let (ox, oy) = self.active_pane_offset();
            let (col, row, left_half) = self
                .stack
                .focused()
                .pixel_to_cell(pos_physical.x - ox, pos_physical.y - oy);
            if let Some(held) = self.mouse_report_button_held {
                if self.stack.focused().terminal.wants_drag_reports()
                    && let Some(xbutton) = xterm_button(held)
                {
                    let mods = self.current_mods();
                    self.stack.focused().terminal.report_mouse_drag(
                        xbutton,
                        mods,
                        col + 1,
                        row + 1,
                    );
                }
            } else if self.text_selecting {
                self.stack
                    .focused()
                    .terminal
                    .extend_selection(col, row, left_half);
                self.text_selection_dragged = true;
            }
        }

        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let under = self.surface_under(pos);

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    /// Keyboard focus has to follow the visible view: clients only get
    /// key events via `set_focus`, and the terminal only gets them via
    /// `focused_showing_terminal_effective()` itself (see `process_input_event`),
    /// so the window needs *no* stale focus lingering while it's hidden,
    /// and *does* need focus the moment it's shown.
    ///
    /// Called automatically from `state.rs`'s `sync_visible_main_window`/
    /// `sync_hut_space` themselves (immediate correction right at the
    /// mutation site), *and* from the two real chokepoints every core-
    /// side mutation ultimately flows through: [`Self::process_input_
    /// event`] (the *only* public way to feed in a real input event —
    /// its private `_unsynced` half does the actual work, so there's no
    /// way to process an event from outside this module without also
    /// running this backstop afterward) and `state.rs`'s Wayland-
    /// dispatch closure (same shape, after `Display::dispatch_clients`
    /// returns). Deliberately *not* a call every focus-changing mutation
    /// site has to remember to pair with `sync_hut_space`/
    /// `sync_visible_main_window` — several turned up missing exactly
    /// that pairing across review rounds (most recently `GraphStack::
    /// remove_exited` — a shell exit shifting/collapsing focus; see its
    /// own doc comment) — so the two chokepoints above exist specifically
    /// so a *new* mutation path can't reintroduce that bug class: it has
    /// no way to run at all except through one of them, and both already
    /// repair focus once they're done. Redundant calls are logically
    /// safe regardless — this method only ever *repairs* a genuinely
    /// stale focus, never force-overrides a still-legitimate one — so
    /// running it after every single input event/dispatch batch, rather
    /// than gating it to once per rendered frame the way an earlier
    /// version did (when the only chokepoint was inside `render.rs`'s
    /// own per-output frame-building loop, gating mattered to avoid an
    /// O(outputs^2) cost — see that version's history if it's ever worth
    /// revisiting), costs an O(outputs) layer-map scan per event instead
    /// of per frame; negligible at this codebase's realistic output/
    /// layer-surface counts, and neither chokepoint here sits inside a
    /// per-output loop the way that old call site did, so there's
    /// nothing left to gate against.
    ///
    /// **This must NOT unconditionally force-set focus to "the terminal or
    /// the active Main Window" every time it runs** — an earlier version
    /// of this method did exactly that, and running it on every redraw
    /// (rather than only at a specific mutation site) turned a rare stomp
    /// into a near-guaranteed one, caught in review: this method only
    /// knows how to compute *two* targets (terminal, or the focused Hut's
    /// Main Window), but real keyboard focus can legitimately be a
    /// Floating Window/Alert (`this file`'s click-to-focus, a few lines
    /// away) or a mapped layer-shell surface (`try_click_layer_surface`,
    /// also this file) — neither of which this method's own `target`
    /// below ever produces. Force-setting anyway reverted a just-clicked
    /// Floating Window or a just-focused panel/launcher back to the Main
    /// Window on the very next redraw (which a click's own
    /// `request_redraw()`, or even just the newly-focused surface's own
    /// commit in response to its `Enter` event, triggers almost
    /// immediately) — making it functionally impossible to keep keyboard
    /// focus on anything this method doesn't itself know about. Fixed by
    /// checking whether the *current* real focus is already a legitimate
    /// target first, and leaving it alone if so; only a focus that's
    /// neither — genuinely stale, pointing at something that isn't part
    /// of the current view at all — gets reset to this method's own
    /// fallback. (NOT session-lock's own PIN entry — that uses a
    /// completely separate `ext-session-lock`/`LockSurface` role, never a
    /// `wlr-layer-shell` one, and stays correct for an unrelated reason:
    /// `process_locked_input_event` re-asserts `keyboard.set_focus` on
    /// every single locked keystroke, a wholly separate mechanism this
    /// method never runs alongside — [`Self::process_input_event`]
    /// itself skips calling this while `state.locked` is set.)
    ///
    /// "Legitimate" means one of:
    /// - a real, *currently mapped* layer-shell surface — checked via
    ///   `layer_map_for_output` across every output, not just whether the
    ///   surface's data map has ever held `LayerSurfaceData` (assigned
    ///   once at role-creation and never removed again, even after
    ///   `layer_destroyed`'s `unmap_layer` — checking presence alone
    ///   would treat a real-but-unmapped former layer surface as
    ///   permanently exempt from ever being corrected again);
    /// - anything currently mapped as a `Window` in the *focused* Hut's
    ///   own `space` (Main Window, Floating Windows, and Alerts are all
    ///   mapped there together — see `sync_main_window_space`'s own doc
    ///   comment).
    ///
    /// Two known, accepted limitations of the second check, neither new
    /// (both predate this method existing at all, in the sense that
    /// nothing before this checked `space` for a stale-focus repair
    /// either — this method's whole *reason* for existing is a stronger
    /// guarantee than "nothing", not a weaker one than some prior
    /// mechanism):
    /// - `space()` (not the self-syncing `space_mut`, deliberately — see
    ///   its own doc comment on why forcing a sync in a read path this
    ///   frequent risks discarding a live in-progress drag) can itself be
    ///   stale if some future mutation site changes what should be
    ///   focused without also syncing `space` for that Hut first — the
    ///   same "mutation site forgot the pairing" class of bug this method
    ///   exists to catch elsewhere, just one level further down. Not
    ///   fixable here without reintroducing the forced-sync hazard.
    /// - When a Main Window entry has *multiple* Floating Windows/Alerts
    ///   mapped simultaneously, this only asks "is the current focus
    ///   *some* element of `space`", not "is it *the* one that should
    ///   presently hold focus" — it can't distinguish between them. Only
    ///   actually matters for a mutation path that changes which one
    ///   *should* be focused without itself calling `keyboard.set_focus`
    ///   for the new one — `handlers/shell.rs`'s `retag` (tagging a
    ///   toplevel as a Floating Window/Alert via `mudhuts_shell_v1`) is
    ///   exactly this, caught in review (this method's earlier claim here
    ///   that "no such path exists" was simply wrong) and fixed at the
    ///   source: `retag` now explicitly focuses its own newly-tagged
    ///   window itself, the same way this method can't. Any *future*
    ///   mutation path that forgets to do the same would hit this exact
    ///   limitation again — this method still can't be the backstop for
    ///   that specific case, only for "focus points at something no
    ///   longer part of the view at all."
    pub(crate) fn sync_keyboard_focus_to_view(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if let Some(current) = keyboard.current_focus() {
            let is_mapped_layer_shell_surface = self
                .stack
                .outputs()
                .iter()
                .any(|slot| layer_map_for_output(&slot.output).layers().any(|l| l.wl_surface() == &current));
            if is_mapped_layer_shell_surface {
                return;
            }
            if crate::space_element::window_in_space(self.stack.focused().space(), &current).is_some() {
                return;
            }
        }
        let target = if self.focused_showing_terminal_effective() {
            None
        } else {
            self.stack
                .focused()
                .active_window()
                .and_then(|w| w.toplevel())
                .map(|t| t.wl_surface().clone())
        };
        keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
    }

    /// Auto-hide tab strip (`chrome_config.auto_hide_tab_strip`): called
    /// from `handle_pointer_motion` with the pointer's own physical-pixel
    /// Y, already local to the now-focused output's origin (real
    /// multi-monitor's focus-follows-mouse means that's always the output
    /// the pointer is actually over — see that function's own doc
    /// comment). Reveals the combined Hut-level + Main-Window tab strip
    /// (`render.rs`'s `combined_tab_strip_height`) the instant the
    /// pointer touches the very top edge of the output, and keeps it
    /// shown for as long as the pointer stays anywhere within the strip's
    /// own rect afterward — only actually hiding again once the pointer
    /// leaves that whole rect, not the moment it moves off the few-pixel
    /// edge that revealed it in the first place. `self.tab_strip_revealed`
    /// itself is a plain `bool`, not a `Signal` (see its own doc comment
    /// on why) — doesn't request its own redraw on a flip, unlike a
    /// `Signal` write would: `handle_pointer_motion`, this method's only
    /// caller, already calls `request_redraw()` unconditionally on every
    /// motion event before reaching here (see that call's own doc
    /// comment on why it's unconditional), so a second one here would
    /// only ever be a redundant no-op.
    ///
    /// Known narrow gap, not fixed: the hysteresis check below only
    /// re-evaluates `combined_tab_strip_height` on a pointer-motion
    /// event, so if the strip's real height shrinks (a tab/Main Window
    /// closing) while the pointer stays perfectly still, it can stay
    /// revealed slightly past where its own current rect actually ends
    /// until the next motion event corrects it — self-healing on the
    /// very next real one, matching this codebase's existing tolerance
    /// for similarly narrow staleness windows elsewhere (see
    /// `sync_keyboard_focus_to_view`'s own doc comment). A fully
    /// event-driven invalidation (recomputing on every tab-count-changing
    /// mutation, not just motion) would also happen to remove the
    /// repeated per-motion-event recomputation this does while hovering
    /// the revealed strip — not done here as its own thing given how
    /// cheap that recomputation already is (bounded by the active Tab-Hut
    /// path's own depth, no I/O).
    fn update_tab_strip_reveal(&mut self, pointer_y_physical: f64) {
        const EDGE_REVEAL_PX: f64 = 4.0;
        // Only computed on the one branch that actually needs it — see
        // `tab_strip_reveal_state`'s own doc comment on why `None`
        // elsewhere isn't just a placeholder.
        let strip_height = (pointer_y_physical > EDGE_REVEAL_PX && self.tab_strip_revealed)
            .then(|| crate::render::combined_tab_strip_height(self, self.stack.focused_output_index()) as f64);
        self.tab_strip_revealed =
            tab_strip_reveal_state(pointer_y_physical, EDGE_REVEAL_PX, self.tab_strip_revealed, strip_height);
    }

    /// Try to handle a left-click as a chrome interaction — a
    /// Hut-level tab (any nesting level), a ConsoleHut-level Main-Window tab,
    /// or clicking into a Tile-Hut pane — rather than a normal
    /// terminal/window click. On a hit, switches focus accordingly and
    /// returns `true`, so the caller can skip its normal click handling
    /// for this press.
    ///
    /// A genuinely tiled Tile-Hut (2+ panes) is checked *exclusively*
    /// — mirroring `render.rs`'s `build_frame_elements`, which skips
    /// building the Hut-tab/ConsoleHut-tab chrome pipeline entirely while
    /// tiled (its `is_tile` check): neither strip is ever actually drawn
    /// there, so hit-testing against them too would risk a click inside a tile
    /// pane spuriously landing on some *other* ConsoleHut's tab layout that
    /// happens to overlap the same screen position but was never
    /// visible. Otherwise, checked in front-to-back z-order matching that
    /// same function's element push order: Hut-level tabs first
    /// (topmost), then the ConsoleHut-level strip below them.
    ///
    /// `pos` is physical — every rect this checks against (tile panes,
    /// Hut/ConsoleHut tab strips) is mudhuts' own drawn chrome, sized
    /// against `focused_usable_area()`/`output_size`, not a real Wayland surface
    /// Smithay tracks in Logical space. The caller converts from the
    /// seat's Logical position before calling this.
    fn try_click_chrome(&mut self, pos: Point<f64, Physical>) -> bool {
        let pixel = Point::<i32, Physical>::from((pos.x.round() as i32, pos.y.round() as i32));

        // Rects come from `hut::pane_rects`, the single computation
        // shared with `render.rs`'s `content_elements`/`TileNode::
        // resolve` and `State::active_pane_offset` (composable Hut
        // hierarchy RFC's Q3) — a click can't land on the wrong pane by
        // disagreeing with what's actually drawn, whenever a layer-shell
        // surface reserves part of the output, since there's only one
        // place left to get that wrong.
        let area = self.focused_usable_area();
        let top = self.stack.focused_top_level();
        // The `if let Some(tile) = ...` below is deliberately its own,
        // self-contained check rather than a call to
        // `crate::graph_nodes::is_effectively_tiled` (the shared `bool`
        // `render.rs`'s `build_frame_elements`/`combined_tab_strip_height`
        // both use instead — see that function's own doc comment): this
        // branch needs the real `TileNode` (its `axis`/`fracs`), not just
        // a yes/no answer, and `downcast::<TileNode>` already hands that
        // back as a plain, safely-matchable `Option` in one call — no
        // `.expect()`/`.unwrap()` anywhere in this branch. Calling
        // `is_effectively_tiled` first and then downcasting *again* to
        // get `tile` would need one of those on the second call (already
        // confirmed safe by the first, but not provably so to the
        // compiler) purely to avoid this one line looking like a
        // duplicate — not a trade worth making in the input path.
        // `children.len() >= 2` here is the same condition
        // `is_effectively_tiled` checks (a 1-child/empty Tile never
        // actually exists — see its own doc comment), just not a call to
        // the same function.
        if let Some(tile) = self.stack.graph().downcast::<crate::graph_nodes::TileNode>(top) {
            let children = self.stack.graph().hut_list_input(top, "children");
            if children.len() >= 2 {
                let fracs = crate::graph_nodes::fracs_for(&children, &tile.fracs);
                let axis = tile.axis;
                let rects = crate::hut::pane_rects(axis, fracs.into_iter(), (area.size.w, area.size.h))
                    .into_iter()
                    .map(|(x, y, w, h)| (x + area.loc.x, y + area.loc.y, w, h));
                let Some(i) = hit_pane_index(pixel, rects) else {
                    return false;
                };
                // Writing through the `Signal` requests its own redraw
                // (see `redraw::Signal`'s doc comment) — no
                // `request_redraw()` needed here.
                if let Some(tile) = self.stack.graph_mut().downcast_mut::<crate::graph_nodes::TileNode>(top) {
                    *tile.active = i;
                }
                self.sync_visible_main_window();
                return true;
            }
        }

        // `render::tab_strip_visible` — the same function `render.rs`'s
        // own draw gate calls — so a hidden (not drawn) strip can never
        // independently end up clickable, or vice versa: a click that
        // would have hit it just falls through to the normal
        // terminal/window handling below instead, exactly as if nothing
        // were there, matching what the user actually sees on screen.
        if !crate::render::tab_strip_visible(self, self.stack.focused_output_index()) {
            return false;
        }

        let cell_w = self.stack.focused().glyphs.cell_width().max(1);
        let cell_h = self.stack.focused().glyphs.cell_height().max(1) as i32;
        let scale = self.focused_output_scale();

        if village_chrome::handle_click(
            self.stack.graph_mut(),
            top,
            (pixel.x, pixel.y),
            0,
            cell_w,
            cell_h,
            scale,
        ) {
            // Same as the Tile-pane branch above — `handle_click` goes
            // through `TabNode::active`, which already requested the
            // redraw.
            self.sync_visible_main_window();
            return true;
        }

        let strip_y = village_chrome::stack_height(self.stack.graph(), top, cell_h, scale);
        let hit = chrome::tab_layout(self.stack.focused(), strip_y, scale)
            .into_iter()
            .find(|t| t.rect.contains(pixel));
        if let Some(hit) = hit {
            let hut = self.stack.focused_mut();
            if hit.index == 0 {
                *hut.showing_terminal = true;
            } else {
                *hut.showing_terminal = false;
                hut.set_active_main_window(hit.index - 1);
            }
            self.sync_visible_main_window();
            return true;
        }

        false
    }

    /// Try to claim a click for a `wlr-layer-shell` surface — a status
    /// bar, launcher, notification popup, etc. — giving it real keyboard
    /// focus if it asked for any (`set_keyboard_interactivity`'s
    /// `on_demand`/`exclusive`; a plain `none` surface, e.g. a
    /// wallpaper-style Background layer, still claims the click for
    /// z-order purposes but never takes focus).
    ///
    /// `above` picks which half of the layer stack to check: `true` for
    /// Top/Overlay (checked *before* normal terminal/window content, since
    /// those render above it — see `render.rs`'s `composite_normal_content`),
    /// `false` for Bottom/Background (checked only once nothing else —
    /// chrome, Top/Overlay, normal content — already claimed the click).
    ///
    /// Shares `state.rs`'s `layer_surface_under`/`under_layer` with
    /// `State::surface_under` (composable Hut hierarchy RFC migration step
    /// 5 sub-step 4's hit-test consolidation) — this used to independently
    /// re-derive the exact same upper/lower split and per-layer surface
    /// resolution by hand.
    fn try_click_layer_surface(&mut self, pos: Point<f64, Logical>, serial: Serial, above: bool) -> bool {
        let Some(output) = self.output.clone() else {
            return false;
        };
        let focus_target = {
            let layers = layer_map_for_output(&output);
            let Some(layer) = crate::state::layer_surface_under(&layers, pos, above) else {
                return false;
            };
            if crate::state::under_layer(&layers, layer, pos).is_none() {
                return false;
            }
            layer.can_receive_keyboard_focus().then(|| layer.wl_surface().clone())
        };
        if let Some(wl_surface) = focus_target
            && let Some(keyboard) = self.seat.get_keyboard()
        {
            keyboard.set_focus(self, Some(wl_surface), serial);
        }
        true
    }

    /// Any currently mapped `wlr-layer-shell` surface on the Top or
    /// Overlay layer that's requested `exclusive` keyboard access
    /// (typically a lock screen or similarly modal surface) — if one
    /// exists, it should receive every key event unconditionally, ahead
    /// of even mudhuts' own global keybindings (mirrors
    /// `.smithay-ref/anvil`'s own `keyboard_key_to_action` reference
    /// behavior).
    fn exclusive_layer_surface(&self) -> Option<WlSurface> {
        // Every real output's own layer map, not just the focused one
        // (`self.output`) — keyboard input is seat-wide, not per-output,
        // and `handlers/layer_shell.rs`'s own multi-monitor rework
        // already lets an exclusive-interactivity Top/Overlay surface
        // map on any output. Scoping this to the focused output alone
        // meant such a surface on a backgrounded monitor never actually
        // won the keyboard grab its own doc comment promises. The
        // focused output is still checked *first*, though, not scanned
        // in arbitrary Vec order: nothing in the wlr-layer-shell protocol
        // stops two different clients from each requesting `exclusive` on
        // two different outputs at once, and every keystroke (including
        // an on-screen password/PIN entry) has to go to whichever one the
        // user is actually looking at, not whichever happened to connect
        // first.
        fn exclusive_on(output: &smithay::output::Output) -> Option<WlSurface> {
            layer_map_for_output(output)
                .layers()
                .find(|l| {
                    matches!(l.layer(), WlrLayer::Top | WlrLayer::Overlay)
                        && l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
                })
                .map(|l| l.wl_surface().clone())
        }
        exclusive_search_order(self.stack.focused_output_index(), self.stack.outputs().len())
            .find_map(|i| self.stack.outputs().get(i).and_then(|slot| exclusive_on(&slot.output)))
    }

    /// Where every input event goes while `self.locked` (see
    /// `handlers/session_lock.rs`) — checked as the very first thing in
    /// [`Self::process_input_event`], ahead of even
    /// [`Self::exclusive_layer_surface`] (a lock wins over an `exclusive`
    /// layer-shell surface too, the same way it wins over everything
    /// else). A keyboard event is forwarded to the lock surface's own
    /// `wl_surface` if one exists and is still alive (focusing it first —
    /// harmless to repeat every event, since `set_focus` is a no-op once
    /// already focused there); every other event, including *every*
    /// pointer variant and any keyboard event with no lock surface to
    /// receive it, is dropped outright — no keymap lookup, no
    /// `try_click_chrome`, no terminal mouse-report/selection, no
    /// `space.element_under`, nothing.
    fn process_locked_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        let InputEvent::Keyboard { event } = event else {
            return;
        };
        // A single seat only ever has one keyboard, so it can only
        // forward to one output's own lock surface at a time — the
        // *focused* output is the natural (and only sensible) choice,
        // matching every other focus-scoped keyboard path in this file.
        // Real multi-monitor: `self.lock_surfaces` now holds one entry
        // per output (see `State::lock_surfaces`'s own doc comment), not
        // a single shared slot.
        let Some(surface) = self
            .stack
            .outputs()
            .get(self.stack.focused_output_index())
            .and_then(|slot| self.lock_surfaces.iter().find(|(o, _)| *o == slot.output))
            .map(|(_, s)| s)
            .filter(|s| s.alive())
            .map(|s| s.wl_surface().clone())
        else {
            return;
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let keycode = event.key_code();
        let key_state = event.state();
        keyboard.set_focus(self, Some(surface), serial);
        keyboard.input::<(), _>(self, keycode, key_state, serial, time, |_, _, _| {
            FilterResult::Forward
        });
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::CloseFocused => {
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
                let Some(focused) = keyboard.current_focus() else {
                    return;
                };
                // `space()`, not the self-syncing `space_mut` — see
                // `state.rs`'s `surface_under`'s own doc comment on why
                // forcing a sync risks discarding a live in-progress
                // drag write elsewhere in the same Hut's `space`.
                let toplevel = crate::space_element::window_in_space(self.stack.focused().space(), &focused)
                    .and_then(|w| w.toplevel().cloned());
                if let Some(toplevel) = toplevel {
                    toplevel.send_close();
                }
            }
            Action::ToggleTerminal => {
                // No-op with nothing to toggle to (matches the original
                // "except when there are no windows open" rule) — checked
                // via the un-forced `showing_terminal` field, not
                // `focused_showing_terminal_effective()`, since that would always
                // report true here and this toggle would never do
                // anything. Per-ConsoleHut now (Phase 4): toggling in one ConsoleHut
                // doesn't disturb what any other ConsoleHut was last showing.
                let hut = self.stack.focused_mut();
                if hut.main_window_count() > 0 {
                    // `showing_terminal` is a `Signal` (see
                    // `redraw::Signal`'s doc comment) — this write alone
                    // now guarantees the redraw a manual
                    // `self.request_redraw()` used to have to remember
                    // right here, and once genuinely shipped without
                    // (masked under winit by that backend's own "redraw
                    // on every input event regardless" behavior, but not
                    // under the purely demand-driven udev backend).
                    *hut.showing_terminal = !*hut.showing_terminal;
                    self.sync_visible_main_window();
                }
            }
            Action::StackNext => {
                // With no `stack-hold` configured, there's nothing to
                // preview-and-commit-on-release with, so this commits
                // immediately (Phase 3's original behavior) — synced right
                // away. Otherwise it opens/advances a preview session
                // instead (background/frozen until release — see
                // `Keymap::stack_hold`'s doc and the keyboard-input
                // closure below, which watches for that modifier's release
                // to commit and sync there instead).
                let instant = self.keymap.stack_hold().is_empty();
                let result = if instant {
                    self.stack.next()
                } else {
                    self.stack.preview_next()
                };
                if let Err(err) = result {
                    tracing::error!("failed to advance the ConsoleHut stack: {err}");
                }
                if instant {
                    self.sync_visible_main_window();
                }
                // The newly-focused (or newly-previewed) ConsoleHut gets resized
                // to the real output size as part of the redraw this
                // triggers (see `winit_backend.rs`'s `resize_all` call,
                // which runs before that frame's texture is generated) —
                // a freshly-spawned one starts at the placeholder default
                // grid size otherwise. `Stack::next`/`preview_next` already
                // called `mark_dirty()` on their own redraw handle above
                // (composable Hut hierarchy RFC migration step 2) — no
                // `request_redraw()` needed here.
            }
            Action::StackPrev => {
                let instant = self.keymap.stack_hold().is_empty();
                if instant {
                    self.stack.prev();
                    self.sync_visible_main_window();
                } else {
                    self.stack.preview_prev();
                }
                // See the `StackNext` arm above — `Stack::prev`/
                // `preview_prev` already triggered the redraw.
            }
            // Innermost-first resolution (see the plan's Meta+Left/Right
            // notes): the focused ConsoleHut's own Main Window tabs win if it
            // has 2+; only then does this bubble up to the nearest
            // ancestor Tab/Tile-Hut and cycle *its* children instead
            // (`Stack::cycle_innermost` recurses to find that level on
            // its own — a no-op if there isn't one, e.g. a lone ConsoleHut).
            Action::TabNext => {
                // `cycle_tab` is a ConsoleHut-internal change (not part of
                // this migration step's Redrawable wiring — see
                // `hut::Hut::attach_redraw_handle`'s doc comment), so it
                // still needs an explicit redraw; `cycle_innermost`
                // requests its own via `TabbedHut`/`TileHut::set_active`
                // whenever it actually changes anything, so no
                // unconditional call is needed for that branch.
                if self.stack.focused().main_window_count() >= 2 {
                    self.stack.focused_mut().cycle_tab(true);
                    self.request_redraw();
                } else {
                    self.stack.cycle_innermost(crate::hut::Direction::Next);
                }
                self.sync_visible_main_window();
            }
            Action::TabPrev => {
                if self.stack.focused().main_window_count() >= 2 {
                    self.stack.focused_mut().cycle_tab(false);
                    self.request_redraw();
                } else {
                    self.stack.cycle_innermost(crate::hut::Direction::Prev);
                }
                self.sync_visible_main_window();
            }
            Action::WrapTab => {
                // `Stack::wrap_tab` already requests its own redraw
                // (composable Hut hierarchy RFC migration step 4) — no
                // `request_redraw()` needed here.
                if let Err(err) = self.stack.wrap_tab() {
                    tracing::error!("failed to spawn a new ConsoleHut for wrap-tab: {err}");
                }
                self.sync_visible_main_window();
            }
            Action::WrapTile => {
                if let Err(err) = self.stack.wrap_tile() {
                    tracing::error!("failed to spawn a new ConsoleHut for wrap-tile: {err}");
                }
                self.sync_visible_main_window();
            }
            Action::CopySelection => {
                // Deliberately separate from the primary-selection commit
                // in the `PointerButton` handler below: X11 convention
                // keeps the two independent — every drag-selection updates
                // primary immediately, but only an explicit copy touches
                // the regular clipboard.
                if let Some(text) = self.stack.focused().terminal.selection_text() {
                    set_data_device_selection::<Self>(
                        &self.display_handle,
                        &self.seat,
                        text_mime_types(),
                        Arc::new(text),
                    );
                }
            }
            Action::BrightnessUp => spawn_media_command("brightnessctl", &["set", "+5%"]),
            Action::BrightnessDown => spawn_media_command("brightnessctl", &["set", "5%-"]),
            Action::VolumeUp => {
                spawn_media_command("wpctl", &["set-volume", "-l", "1", "@DEFAULT_AUDIO_SINK@", "5%+"])
            }
            Action::VolumeDown => {
                spawn_media_command("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", "5%-"])
            }
            Action::VolumeMute => spawn_media_command("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]),
        }
    }

    /// The only sanctioned entry point for a real input event — thin on
    /// purpose. [`Self::process_input_event_unsynced`] (private, can't be
    /// reached any other way from outside this module) does the actual
    /// work; every one of its own early-return paths still runs through
    /// here first, so the keyboard-focus backstop below can't be skipped
    /// by a future new branch the way a bare call at the end of a long
    /// `match` could be forgotten — see `sync_keyboard_focus_to_view`'s
    /// own doc comment for the bug class this closes for good (the same
    /// reasoning `state.rs`'s Wayland-dispatch closure applies for
    /// client requests). Skipped while locked: `process_locked_input_
    /// event` (called from inside the unsynced half above) already
    /// re-asserts real focus on every single locked keystroke through a
    /// wholly separate mechanism this one must never run alongside — see
    /// `sync_keyboard_focus_to_view`'s own doc comment.
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        self.process_input_event_unsynced(event);
        if !self.locked {
            self.sync_keyboard_focus_to_view();
        }
    }

    fn process_input_event_unsynced<I: InputBackend>(&mut self, event: InputEvent<I>) {
        if self.locked {
            // Checked before the match below even starts, not threaded
            // into each of its arms individually — see
            // `Self::process_locked_input_event`'s doc comment for why a
            // locked session has to sit above literally everything else
            // this function does, including `exclusive_layer_surface()`'s
            // own check further down (which only ever runs for the
            // Keyboard arm, and only once we already know we're not
            // locked).
            self.process_locked_input_event(event);
            let _ = self.display_handle.flush_clients();
            return;
        }

        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let keycode = event.key_code();
                let key_state = event.state();

                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };

                if let Some(surface) = self.exclusive_layer_surface() {
                    // An `exclusive` layer-shell surface (e.g. a lock
                    // screen) wins even over mudhuts' own global
                    // keybindings — every key goes straight to it,
                    // skipping the keymap lookup and terminal-input path
                    // below entirely.
                    keyboard.set_focus(self, Some(surface), serial);
                    keyboard.input::<(), _>(self, keycode, key_state, serial, time, |_, _, _| {
                        FilterResult::Forward
                    });
                    let _ = self.display_handle.flush_clients();
                    return;
                }

                // `keyboard-shortcuts-inhibit-unstable-v1`: a client (a VM
                // viewer, remote-desktop app, ...) can ask not to have its
                // surface's key events intercepted by mudhuts' own global
                // keybindings, so it can forward them raw to whatever it's
                // showing instead. mudhuts keeps keyboard focus in exact
                // 1:1 sync with "the one visible view" (see
                // `sync_keyboard_focus_to_view`'s doc comment), so
                // `current_focus()` is exactly the right — and only —
                // surface to check here; it's `None` while the terminal
                // shows (which has no inhibitor to look up anyway).
                let inhibited = keyboard
                    .current_focus()
                    .and_then(|s| self.seat.keyboard_shortcuts_inhibitor_for_surface(&s))
                    .is_some_and(|i| i.is_active());

                keyboard.input::<(), _>(
                    self,
                    keycode,
                    key_state,
                    serial,
                    time,
                    |data, mods, keysym| {
                        // Checked on *every* event, press or release —
                        // releasing the `stack-hold` modifier itself is
                        // what commits an open preview session, and that
                        // release is a plain modifier keyup, not
                        // something any chord matches.
                        if data.stack.is_previewing()
                            && !data.keymap.stack_hold().satisfied_by(mods)
                        {
                            // `commit_preview` already triggers its own
                            // redraw (see `Action::StackNext`'s arm above).
                            data.stack.commit_preview();
                            data.sync_visible_main_window();
                        }

                        // Global keybindings always win, regardless of
                        // whether the terminal or a client window is the
                        // active view — otherwise there'd be no way to
                        // toggle back to the terminal once a client
                        // window takes over the screen. Except when the
                        // focused surface holds an active shortcuts
                        // inhibitor (`inhibited`, computed above): then it
                        // wants raw key events itself, so the lookup is
                        // skipped entirely and this falls through to the
                        // `Forward` branch just below.
                        if key_state == KeyState::Pressed && !inhibited {
                            let base_keysym = keysym
                                .raw_latin_sym_or_raw_current_sym()
                                .unwrap_or(keysym.modified_sym());
                            if let Some(action) = data.keymap.lookup(mods, base_keysym) {
                                data.handle_action(action);
                                return FilterResult::Intercept(());
                            }
                        }

                        if !data.focused_showing_terminal_effective() {
                            // A client window is the active view; let it
                            // receive the key via its own wl_keyboard
                            // (focus was set on click).
                            return FilterResult::Forward;
                        }

                        if key_state == KeyState::Pressed {
                            let hut = data.stack.focused_mut();
                            let mode = hut.terminal.mode();
                            if let Some(bytes) = encode(&keysym, mods, mode) {
                                hut.terminal.write_input(bytes);
                                hut.mark_touched();
                            }
                        }
                        FilterResult::Intercept(())
                    },
                );
            }
            InputEvent::PointerMotion { event } => {
                // Real mice/touchpads (under the udev/libinput backend)
                // report relative deltas, not an absolute position the
                // way a nested winit window's host compositor does for
                // `PointerMotionAbsolute` below — accumulate into the
                // persisted `pointer_location` and clamp to the *whole
                // virtual desktop's* bounds (mirrors `.smithay-ref/anvil/
                // src/input_handler.rs`'s `clamp_coords`, generalized to
                // real multi-monitor via `GraphStack::virtual_bounding_box`).
                // Clamping against just the focused output's own bounds
                // (this used to, back when mudhuts was single-output)
                // pinned a real pointer device at that output's edge —
                // it could never actually cross onto a second monitor,
                // no matter how far the mouse moved, unlike every other
                // multi-monitor piece around it. `pointer_location` and
                // `event.delta()` are both genuinely Logical (anvil's own
                // reference clamps against `space.output_geometry`, not a
                // raw physical mode size, for the same reason).
                let bounds = self.stack.virtual_bounding_box();
                // `bounds` is only a bounding *hull*, not a true union
                // (see `GraphStack::virtual_bounding_box`'s doc comment),
                // so clamping to it alone can leave a point in the "dead
                // zone" between two different-height/positioned outputs,
                // not actually contained by any real output's rect (which
                // would otherwise get rebased in `handle_pointer_motion`
                // as if it were safely inside) — see
                // `clamp_to_nearest_output`'s own doc comment for the rest.
                let output_rects = (0..self.stack.outputs().len()).filter_map(|i| self.stack.output_rect(i));
                let new_location =
                    clamp_to_nearest_output(self.pointer_location + event.delta(), bounds, output_rects);
                self.handle_pointer_motion(new_location, event.time_msec());
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output_geo) = self.focused_real_output_geometry() else {
                    return;
                };
                // Genuinely global (see `handle_pointer_motion`'s own doc
                // comment on why it needs to be) — `output_geo.loc` is
                // always `(0, 0)` (`focused_real_output_geometry`'s own doc
                // comment: local to the focused output, not its real
                // multi-monitor position), so adding it back wouldn't
                // rebase anything. An absolute-positioning device
                // (touchscreen, drawing tablet) isn't winit-only — real
                // hardware under the udev/libinput backend can emit these
                // too, on any output, not just one sitting at the virtual
                // desktop's own `(0, 0)` origin.
                let output_position = self.stack.output_position(self.stack.focused_output_index());
                let pos = event.position_transformed(output_geo.size) + output_position.to_f64();
                self.handle_pointer_motion(pos, event.time_msec());
            }
            InputEvent::PointerButton { event, .. } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();
                let button_state = event.state();
                let pressed = button_state == ButtonState::Pressed;

                if !pressed && self.dock_drag.is_some() {
                    docks::finish_drag(self);
                }

                if pressed
                    && button == BTN_LEFT
                    && self.try_click_chrome(self.to_physical(pointer.current_location()))
                {
                    // Handled as a chrome click (a Hut-level tab, a
                    // ConsoleHut-level Main-Window tab, or a Tile-Hut pane) —
                    // skip the normal terminal/window click handling
                    // below for this press.
                } else if pressed
                    && self.try_click_layer_surface(pointer.current_location(), serial, true)
                {
                    // Handled by a Top/Overlay layer-shell surface (a
                    // status bar, launcher, etc.) — those render above
                    // normal content (see `render.rs`'s
                    // `composite_normal_content`), so they're checked
                    // before the terminal/window branches below for this
                    // press.
                } else if self.focused_showing_terminal_effective() {
                    // Physical, like `active_pane_offset()`/`pixel_to_cell`
                    // — mudhuts' own terminal grid, not a real surface.
                    let pos = self.to_physical(pointer.current_location());
                    let (ox, oy) = self.active_pane_offset();
                    let (col, row, left_half) = self.stack.focused().pixel_to_cell(pos.x - ox, pos.y - oy);
                    let mods = self.current_mods();

                    if self.stack.focused().terminal.wants_mouse_reports() {
                        if let Some(xbutton) = xterm_button(button) {
                            self.stack.focused().terminal.report_mouse_button(
                                xbutton,
                                mods,
                                pressed,
                                col + 1,
                                row + 1,
                            );
                        }
                        self.mouse_report_button_held = pressed.then_some(button);
                    } else if button == BTN_LEFT {
                        if pressed {
                            self.stack
                                .focused()
                                .terminal
                                .start_selection(col, row, left_half);
                            self.text_selecting = true;
                            self.text_selection_dragged = false;
                        } else if self.text_selecting {
                            self.text_selecting = false;
                            if !self.text_selection_dragged {
                                self.stack.focused().terminal.clear_selection();
                            } else if let Some(text) = self.stack.focused().terminal.selection_text()
                            {
                                // X11 convention: a completed drag-
                                // selection commits to the primary
                                // selection automatically, with no
                                // explicit copy action needed — see
                                // `Action::CopySelection` for the separate,
                                // explicit path to the regular clipboard.
                                set_primary_selection::<Self>(
                                    &self.display_handle,
                                    &self.seat,
                                    text_mime_types(),
                                    Arc::new(text),
                                );
                            }
                        }
                    }
                } else if pressed && !pointer.is_grabbed() {
                    // `pos` (Logical) is what `space.element_under`/
                    // `try_click_layer_surface` below need — `docks.rs`'s
                    // handle hit-testing needs physical instead (its own
                    // drawn chrome, not a real surface), so it gets a
                    // separately-converted copy rather than sharing `pos`.
                    let pos = pointer.current_location();
                    if docks::start_drag(self, self.to_physical(pos)) {
                        // Handled as a docked handle's drag-start instead
                        // of a normal click-to-focus below.
                    } else if let Some(window) = self
                        .stack
                        .focused()
                        .space()
                        .element_under(pos)
                        .and_then(|(e, _loc)| match e {
                            crate::space_element::HutSpaceElement::Window(w) => Some(w.clone()),
                            crate::space_element::HutSpaceElement::Composited(_) => None,
                        })
                    {
                        // Raw, not the self-syncing `space_mut` — see
                        // `state.rs`'s `surface_under`'s own doc comment:
                        // a forced sync here would risk discarding a live
                        // in-progress drag position or (for the
                        // `raise_element` call right below) an earlier
                        // `raise_element`'s own z-order adjustment, since
                        // `space_mut` always rebuilds in the model's
                        // fixed iteration order.
                        self.stack.focused_mut().space_raw_mut().raise_element(
                            &crate::space_element::HutSpaceElement::Window(window.clone()),
                            true,
                        );
                        if let (Some(keyboard), Some(toplevel)) =
                            (self.seat.get_keyboard(), window.toplevel())
                        {
                            keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                        }
                        // No `send_pending_configure()` sweep here: until
                        // the floating Floating Window/Alert system (Phase 5)
                        // adds real per-window focus, every window is
                        // permanently hinted `Activated` (set once in
                        // `new_toplevel`) and nothing else in this handler
                        // changes a window's pending state — resending
                        // configures on every click would just be
                        // unnecessary client-side redraws.
                    } else if self.try_click_layer_surface(pos, serial, false) {
                        // Nothing else claimed it — falls through to a
                        // Bottom/Background layer-shell surface (e.g. a
                        // wallpaper-style widget), if one's actually there.
                    } else if let Some(keyboard) = self.seat.get_keyboard() {
                        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                    }
                }

                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }
                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                if self.focused_showing_terminal_effective() && vertical_amount != 0.0 {
                    if self.stack.focused().terminal.wants_mouse_reports() {
                        // Accumulate raw `vertical_amount` (the same
                        // physical-pixel-ish unit libinput/Wayland's
                        // `wp_pointer.axis` always uses) and only act once
                        // a full `WHEEL_CLICK_PX` has built up, carrying
                        // the remainder — rather than treating every
                        // single `PointerAxis` event as "at least one
                        // click." A real discrete mouse wheel already
                        // sends ~15px (one click) per event, so this
                        // changes nothing for it; a trackpad's continuous
                        // `AxisSource::Finger` events are much smaller and
                        // much more frequent, and treating each of those
                        // as its own click was making a single gentle
                        // swipe register as dozens of clicks, scrolling a
                        // TUI app's (vim/less/btop/...) own scrollback far
                        // faster than the same physical motion should.
                        let hut = self.stack.focused_mut();
                        let (new_accum, clicks) =
                            accumulate_discrete_units(hut.wheel_click_accum, vertical_amount, WHEEL_CLICK_PX);
                        hut.wheel_click_accum = new_accum;
                        if clicks != 0
                            && let Some(pointer) = self.seat.get_pointer()
                        {
                            let pos = self.to_physical(pointer.current_location());
                            let (ox, oy) = self.active_pane_offset();
                            let (col, row, _) = self.stack.focused().pixel_to_cell(pos.x - ox, pos.y - oy);
                            let mods = self.current_mods();
                            let wheel_button = if clicks > 0 {
                                mudhuts_term::mouse::BUTTON_WHEEL_DOWN
                            } else {
                                mudhuts_term::mouse::BUTTON_WHEEL_UP
                            };
                            for _ in 0..clicks.abs() {
                                self.stack.focused().terminal.report_mouse_button(
                                    wheel_button,
                                    mods,
                                    true,
                                    col + 1,
                                    row + 1,
                                );
                            }
                        }
                    } else {
                        // No app has grabbed the mouse — scroll this
                        // ConsoleHut's own scrollback instead. Thresholded
                        // against the terminal's *real* cell height, not
                        // `WHEEL_CLICK_PX` (an unrelated wheel-click
                        // convention that only matters for the SGR-report
                        // branch above) — one line per full cell-height's
                        // worth of accumulated motion, so a scroll gesture
                        // moves the view by roughly the distance it
                        // visually covered, rather than jumping a fixed
                        // multiple of lines per 15px regardless of how
                        // tall a line actually is.
                        let cell_h = self.stack.focused().glyphs.cell_height() as f64;
                        let hut = self.stack.focused_mut();
                        let (new_accum, lines) =
                            accumulate_discrete_units(hut.scroll_line_accum, vertical_amount, cell_h);
                        hut.scroll_line_accum = new_accum;
                        if lines != 0 {
                            // `lines > 0` is "scroll down" (same convention
                            // as the wheel-report mapping above);
                            // `Terminal::scroll`'s sign is the opposite
                            // (positive moves further *up* into history).
                            self.stack.focused().terminal.scroll(-lines);
                            self.request_redraw();
                        }
                    }
                }

                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }

        // Forwarding a key/button/axis event to a client (`wl_keyboard`/
        // `wl_pointer`) only queues the protocol message in its outgoing
        // buffer — nothing else flushes client sockets between redraw
        // passes (see `udev_backend.rs`'s `render_surface`, the only other
        // `flush_clients()` call site). Flushing here is deliberately
        // decoupled from `request_redraw()`: forcing a full render pass
        // (damage check, GPU composite) just to deliver bytes already
        // sitting in a buffer would be real, avoidable work for something
        // that's really just "finish this write()". Under winit this is a
        // harmless no-op call (its own event loop iteration already
        // flushes via the host's normal dispatch cycle).
        let _ = self.display_handle.flush_clients();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_to_nearest_output_is_a_no_op_when_already_inside_an_output() {
        let bounds = Rectangle::new(Point::from((0.0, 0.0)), smithay::utils::Size::from((2000.0, 800.0)));
        let outputs = [Rectangle::new(Point::from((0.0, 0.0)), smithay::utils::Size::from((1000.0, 800.0)))];
        let point = Point::from((500.0, 400.0));
        assert_eq!(clamp_to_nearest_output(point, bounds, outputs.into_iter()), point);
    }

    #[test]
    fn clamp_to_nearest_output_clamps_to_the_hull_first() {
        let bounds = Rectangle::new(Point::from((0.0, 0.0)), smithay::utils::Size::from((1000.0, 800.0)));
        let outputs = [Rectangle::new(Point::from((0.0, 0.0)), smithay::utils::Size::from((1000.0, 800.0)))];
        // Far outside the hull entirely — clamped into the hull, which is
        // itself a real output here, so no further nearest-point search
        // is needed.
        let point = Point::from((5000.0, -200.0));
        assert_eq!(clamp_to_nearest_output(point, bounds, outputs.into_iter()), Point::from((1000.0, 0.0)));
    }

    #[test]
    fn clamp_to_nearest_output_snaps_into_the_dead_zone_between_two_outputs() {
        // Two side-by-side outputs, B shorter than A — a "dead zone"
        // exists below B but to the right of A.
        let a = Rectangle::new(Point::from((0.0, 0.0)), smithay::utils::Size::from((1000.0, 800.0)));
        let b = Rectangle::new(Point::from((1000.0, 0.0)), smithay::utils::Size::from((1000.0, 600.0)));
        let bounds = Rectangle::new(Point::from((0.0, 0.0)), smithay::utils::Size::from((2000.0, 800.0)));
        let point = Point::from((1500.0, 700.0));
        // Nearest point on A: (1000, 700), distance 500.
        // Nearest point on B: (1500, 600), distance 100 — genuinely closer.
        assert_eq!(clamp_to_nearest_output(point, bounds, [a, b].into_iter()), Point::from((1500.0, 600.0)));
    }

    #[test]
    fn clamp_to_nearest_output_with_no_real_outputs_falls_back_to_the_hull_clamp() {
        let bounds = Rectangle::new(Point::from((0.0, 0.0)), smithay::utils::Size::from((1000.0, 800.0)));
        let point = Point::from((5000.0, 5000.0));
        assert_eq!(clamp_to_nearest_output(point, bounds, std::iter::empty()), Point::from((1000.0, 800.0)));
    }

    #[test]
    fn accumulate_discrete_units_stays_at_zero_below_one_unit() {
        let (accum, units) = accumulate_discrete_units(0.0, 5.0, 15.0);
        assert_eq!(units, 0);
        assert_eq!(accum, 5.0);
    }

    #[test]
    fn accumulate_discrete_units_extracts_exact_multiples_with_no_remainder() {
        let (accum, units) = accumulate_discrete_units(0.0, 30.0, 15.0);
        assert_eq!(units, 2);
        assert_eq!(accum, 0.0);
    }

    #[test]
    fn accumulate_discrete_units_carries_the_remainder_across_calls() {
        let (accum, units) = accumulate_discrete_units(0.0, 20.0, 15.0);
        assert_eq!(units, 1);
        assert_eq!(accum, 5.0);
        // A second small delta combines with the carried remainder to
        // produce a second unit exactly when the total crosses 15.
        let (accum, units) = accumulate_discrete_units(accum, 10.0, 15.0);
        assert_eq!(units, 1);
        assert_eq!(accum, 0.0);
    }

    #[test]
    fn accumulate_discrete_units_handles_negative_deltas_symmetrically() {
        let (accum, units) = accumulate_discrete_units(0.0, -20.0, 15.0);
        assert_eq!(units, -1);
        assert_eq!(accum, -5.0);
    }

    #[test]
    fn hit_pane_index_finds_the_containing_rect() {
        let rects = vec![(0, 0, 100, 100), (100, 0, 100, 100)];
        assert_eq!(hit_pane_index(Point::from((50, 50)), rects.clone().into_iter()), Some(0));
        assert_eq!(hit_pane_index(Point::from((150, 50)), rects.into_iter()), Some(1));
    }

    #[test]
    fn hit_pane_index_outside_every_rect_is_none() {
        let rects = vec![(0, 0, 100, 100), (100, 0, 100, 100)];
        assert_eq!(hit_pane_index(Point::from((500, 500)), rects.into_iter()), None);
    }

    #[test]
    fn hit_pane_index_upper_bound_is_exclusive() {
        // (0,0,100,100)'s own far edge (x=100) belongs to the *next*
        // pane, not this one — an inclusive upper bound here would let a
        // click land in two panes' rects at once.
        let rects = vec![(0, 0, 100, 100), (100, 0, 100, 100)];
        assert_eq!(hit_pane_index(Point::from((100, 50)), rects.into_iter()), Some(1));
    }

    #[test]
    fn exclusive_search_order_puts_the_focused_output_first() {
        assert_eq!(exclusive_search_order(0, 3).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(exclusive_search_order(2, 3).collect::<Vec<_>>(), vec![2, 0, 1]);
    }

    #[test]
    fn exclusive_search_order_with_a_single_output() {
        assert_eq!(exclusive_search_order(0, 1).collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn exclusive_search_order_with_an_out_of_range_focused_index_is_still_complete() {
        assert_eq!(exclusive_search_order(5, 3).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn tab_strip_reveal_state_reveals_on_edge_touch_regardless_of_prior_state() {
        assert!(tab_strip_reveal_state(0.0, 4.0, false, None));
        assert!(tab_strip_reveal_state(4.0, 4.0, false, None));
    }

    #[test]
    fn tab_strip_reveal_state_stays_hidden_until_the_edge_is_touched() {
        // Past the edge threshold, but never revealed to begin with —
        // must stay hidden regardless of how tall the strip is.
        assert!(!tab_strip_reveal_state(10.0, 4.0, false, Some(1000.0)));
    }

    #[test]
    fn tab_strip_reveal_state_stays_revealed_within_the_strip_and_hides_past_it() {
        assert!(tab_strip_reveal_state(10.0, 4.0, true, Some(20.0)));
        assert!(!tab_strip_reveal_state(30.0, 4.0, true, Some(20.0)));
    }
}
