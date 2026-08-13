use std::sync::Arc;

use mudhuts_term::keys::{Key, Mods, NamedKey};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState, keysyms};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, SERIAL_COUNTER, Scale, Serial};
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
use smithay::wayland::selection::data_device::set_data_device_selection;
use smithay::wayland::selection::primary_selection::set_primary_selection;
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer};

use crate::State;
use crate::keybindings::Action;
use crate::hut::Hut;
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
    /// `pixel_to_cell` all expect, matching `output_size`/`usable_area()`.
    /// See `handle_pointer_motion`'s doc comment for why both spaces are
    /// needed at once rather than picking just one.
    fn to_physical(&self, pos: Point<f64, Logical>) -> Point<f64, Physical> {
        pos.to_physical(Scale::from(self.output_scale()))
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
    /// `self.space.output_geometry`/its own scale-divided bounds — see
    /// `InputEvent::PointerMotionAbsolute`/`PointerMotion` below), which
    /// is what `self.surface_under`/`pointer.motion` need: Smithay's own
    /// `Space`/layer-shell hit-testing, and the position a client's
    /// `wl_pointer.motion` ultimately gets, are both Logical throughout.
    /// But this function *also* drives mudhuts' own physical-pixel-native
    /// hit-testing (the terminal's `pixel_to_cell`, `docks::advance_drag`)
    /// — those get a locally-converted physical copy instead of Logical
    /// `pos` directly, rather than picking just one space for everything
    /// (see `State::usable_area`'s doc comment on why physical is what
    /// mudhuts' own rendering needs).
    fn handle_pointer_motion(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Logical>, time: u32) {
        self.pointer_location = pos;
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
        let serial = SERIAL_COUNTER.next_serial();

        if self.dock_drag.is_some() {
            docks::advance_drag(self, pos_physical);
        }

        if self.showing_terminal_effective() {
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
    /// `showing_terminal_effective()` itself (see `process_input_event`),
    /// so the window needs *no* stale focus lingering while it's hidden,
    /// and *does* need focus the moment it's shown. Shared by
    /// `Action::ToggleTerminal` and every chrome click that can change
    /// which ConsoleHut/tab/pane is now showing (`try_click_chrome`).
    fn sync_keyboard_focus_to_view(&mut self) {
        let target = if self.showing_terminal_effective() {
            None
        } else {
            self.stack
                .focused()
                .active_window()
                .and_then(|w| w.toplevel())
                .map(|t| t.wl_surface().clone())
        };
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
    }

    /// Try to handle a left-click as a chrome interaction — a
    /// Hut-level tab (any nesting level), a ConsoleHut-level Main-Window tab,
    /// or clicking into a Tile-Hut pane — rather than a normal
    /// terminal/window click. On a hit, switches focus accordingly and
    /// returns `true`, so the caller can skip its normal click handling
    /// for this press.
    ///
    /// A genuinely tiled Tile-Hut (2+ panes) is checked *exclusively*
    /// — mirroring `render.rs`'s `build_frame_elements`, which bypasses
    /// the Hut-tab/ConsoleHut-tab chrome pipeline entirely while tiled (see
    /// its own early return): neither strip is ever actually drawn there,
    /// so hit-testing against them too would risk a click inside a tile
    /// pane spuriously landing on some *other* ConsoleHut's tab layout that
    /// happens to overlap the same screen position but was never
    /// visible. Otherwise, checked in front-to-back z-order matching that
    /// same function's element push order: Hut-level tabs first
    /// (topmost), then the ConsoleHut-level strip below them.
    ///
    /// `pos` is physical — every rect this checks against (tile panes,
    /// Hut/ConsoleHut tab strips) is mudhuts' own drawn chrome, sized
    /// against `usable_area()`/`output_size`, not a real Wayland surface
    /// Smithay tracks in Logical space. The caller converts from the
    /// seat's Logical position before calling this.
    fn try_click_chrome(&mut self, pos: Point<f64, Physical>) -> bool {
        let pixel = Point::<i32, Physical>::from((pos.x.round() as i32, pos.y.round() as i32));

        // Rects come from `TileHut::absolute_pane_rects`, the single
        // computation shared with `render.rs`'s `build_tile_elements` and
        // `State::active_pane_offset` (composable Hut hierarchy RFC's
        // Q3) — a click can't land on the wrong pane by disagreeing with
        // what's actually drawn, whenever a layer-shell surface reserves
        // part of the output, since there's only one place left to get
        // that wrong.
        let area = self.usable_area();
        if let Hut::Tile(tile) = self.stack.focused_top_level_mut()
            && tile.children.len() >= 2
        {
            let rects = tile.absolute_pane_rects(area);
            let Some(i) = rects.into_iter().position(|(x, y, w, h)| {
                pixel.x >= x && pixel.x < x + w && pixel.y >= y && pixel.y < y + h
            }) else {
                return false;
            };
            // `set_active` requests its own redraw (composable Hut
            // hierarchy RFC migration step 4) — no `request_redraw()`
            // needed here.
            tile.set_active(i);
            self.sync_visible_main_window();
            self.sync_keyboard_focus_to_view();
            return true;
        }

        let cell_w = self.stack.focused().glyphs.cell_width().max(1);
        let cell_h = self.stack.focused().glyphs.cell_height().max(1) as i32;
        let scale = self.output_scale();

        if village_chrome::handle_click(
            self.stack.focused_top_level_mut(),
            (pixel.x, pixel.y),
            0,
            cell_w,
            cell_h,
            scale,
        ) {
            // Same as the Tile-pane branch above — `handle_click` goes
            // through `TabbedHut::set_active`, which already requested
            // the redraw.
            self.sync_visible_main_window();
            self.sync_keyboard_focus_to_view();
            return true;
        }

        let strip_y = village_chrome::stack_height(self.stack.focused_top_level(), cell_h, scale);
        let hit = chrome::tab_layout(self.stack.focused(), strip_y, scale)
            .into_iter()
            .find(|t| t.rect.contains(pixel));
        if let Some(hit) = hit {
            let hut = self.stack.focused_mut();
            if hit.index == 0 {
                hut.showing_terminal = true;
            } else {
                hut.showing_terminal = false;
                hut.set_active_main_window(hit.index - 1);
            }
            self.sync_visible_main_window();
            self.sync_keyboard_focus_to_view();
            self.request_redraw();
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
    /// those render above it — see `render.rs`'s `layer_elements`), `false`
    /// for Bottom/Background (checked only once nothing else — chrome,
    /// Top/Overlay, normal content — already claimed the click).
    fn try_click_layer_surface(&mut self, pos: Point<f64, Logical>, serial: Serial, above: bool) -> bool {
        let Some(output) = self.space.outputs().next().cloned() else {
            return false;
        };
        let focus_target = {
            let layers = layer_map_for_output(&output);
            let hit = if above {
                layers
                    .layer_under(WlrLayer::Overlay, pos)
                    .or_else(|| layers.layer_under(WlrLayer::Top, pos))
            } else {
                layers
                    .layer_under(WlrLayer::Bottom, pos)
                    .or_else(|| layers.layer_under(WlrLayer::Background, pos))
            };
            let Some(layer) = hit else {
                return false;
            };
            let Some(layer_loc) = layers.layer_geometry(layer).map(|geo| geo.loc) else {
                return false;
            };
            if layer
                .surface_under(pos - layer_loc.to_f64(), WindowSurfaceType::ALL)
                .is_none()
            {
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
        let output = self.space.outputs().next()?;
        let layers = layer_map_for_output(output);
        layers
            .layers()
            .find(|l| {
                matches!(l.layer(), WlrLayer::Top | WlrLayer::Overlay)
                    && l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
            })
            .map(|l| l.wl_surface().clone())
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
        let Some(surface) = self
            .lock_surface
            .as_ref()
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
                let window = self
                    .space
                    .elements()
                    .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &focused))
                    .cloned();
                if let Some(toplevel) = window.and_then(|w| w.toplevel().cloned()) {
                    toplevel.send_close();
                }
            }
            Action::ToggleTerminal => {
                // No-op with nothing to toggle to (matches the original
                // "except when there are no windows open" rule) — checked
                // via the un-forced `showing_terminal` field, not
                // `showing_terminal_effective()`, since that would always
                // report true here and this toggle would never do
                // anything. Per-ConsoleHut now (Phase 4): toggling in one ConsoleHut
                // doesn't disturb what any other ConsoleHut was last showing.
                let hut = self.stack.focused_mut();
                if hut.main_window_count() > 0 {
                    hut.showing_terminal = !hut.showing_terminal;
                    self.sync_visible_main_window();
                    self.sync_keyboard_focus_to_view();
                    // Missing until a previous fix — under winit this was
                    // masked by that backend's own "redraw on every input
                    // event regardless" behavior (see
                    // `handle_pointer_motion`'s doc comment), but the udev
                    // backend is purely demand-driven: without this,
                    // toggling never actually repaints until some
                    // unrelated redraw comes along.
                    self.request_redraw();
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
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
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

                        if !data.showing_terminal_effective() {
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
                // persisted `pointer_location` and clamp to the output's
                // bounds (mirrors `.smithay-ref/anvil/src/
                // input_handler.rs`'s `clamp_coords`, simplified since
                // mudhuts is single-output). `pointer_location` and
                // `event.delta()` are both genuinely Logical (anvil's own
                // reference clamps against `space.output_geometry`, not a
                // raw physical mode size, for the same reason) — clamped
                // against the output's *Logical* bounds, not
                // `self.output_size` (physical), to keep both sides of
                // the clamp in the same space.
                let (max_x, max_y) = self.output_size_logical();
                let mut new_location = self.pointer_location + event.delta();
                new_location.x = new_location.x.clamp(0.0, max_x.max(0) as f64);
                new_location.y = new_location.y.clamp(0.0, max_y.max(0) as f64);
                self.handle_pointer_motion(new_location, event.time_msec());
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
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
                    // normal content (see `render.rs`'s `layer_elements`),
                    // so they're checked before the terminal/window
                    // branches below for this press.
                } else if self.showing_terminal_effective() {
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
                    } else if let Some((window, _loc)) = self
                        .space
                        .element_under(pos)
                        .map(|(w, l)| (w.clone(), l))
                    {
                        self.space.raise_element(&window, true);
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

                if self.showing_terminal_effective() && vertical_amount != 0.0 {
                    // Accumulate raw `vertical_amount` (the same
                    // physical-pixel-ish unit libinput/Wayland's
                    // `wp_pointer.axis` always uses) and only act once a
                    // full `WHEEL_CLICK_PX` has built up, carrying the
                    // remainder — rather than treating every single
                    // `PointerAxis` event as "at least one click," which
                    // is what both branches below used to do. A real
                    // discrete mouse wheel already sends ~15px (one
                    // click) per event, so this changes nothing for it;
                    // a trackpad's continuous `AxisSource::Finger` events
                    // are much smaller and much more frequent, and
                    // treating each of those as its own click was making
                    // a single gentle swipe register as dozens of clicks
                    // — both scrolling a TUI app's (vim/less/btop/...)
                    // own scrollback far faster than the same physical
                    // motion should, and (when no app has grabbed the
                    // mouse) mudhuts' own terminal scrollback the same
                    // way.
                    let hut = self.stack.focused_mut();
                    hut.scroll_accum += vertical_amount;
                    let clicks = (hut.scroll_accum / WHEEL_CLICK_PX).trunc() as i32;
                    if clicks != 0 {
                        hut.scroll_accum -= clicks as f64 * WHEEL_CLICK_PX;

                        if self.stack.focused().terminal.wants_mouse_reports() {
                            if let Some(pointer) = self.seat.get_pointer() {
                                let pos = self.to_physical(pointer.current_location());
                                let (ox, oy) = self.active_pane_offset();
                                let (col, row, _) =
                                    self.stack.focused().pixel_to_cell(pos.x - ox, pos.y - oy);
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
                            // ConsoleHut's own scrollback instead of doing
                            // nothing, 3 lines per click (unchanged from
                            // before this fix). `clicks > 0` is "scroll
                            // down" (same convention as the wheel-report
                            // mapping above); `Terminal::scroll`'s sign is
                            // the opposite (positive moves further *up*
                            // into history).
                            self.stack.focused().terminal.scroll(-clicks * 3);
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
