use mudhuts_term::keys::{Key, Mods, NamedKey};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
};
use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState, keysyms};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;

use crate::State;
use crate::docks;
use crate::keybindings::Action;

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
                // anything. Per-Hut now (Phase 4): toggling in one Hut
                // doesn't disturb what any other Hut was last showing.
                let hut = self.stack.focused_mut();
                if hut.main_window_count() > 0 {
                    hut.showing_terminal = !hut.showing_terminal;
                    self.sync_visible_main_window();
                    // Keyboard focus has to follow the visible view:
                    // clients only get key events via `set_focus`, and the
                    // terminal only gets them via `showing_terminal`
                    // itself (see `process_input_event`), so the window
                    // needs *no* stale focus lingering while it's hidden,
                    // and *does* need focus the moment it's shown.
                    let target = if self.stack.focused().showing_terminal {
                        None
                    } else {
                        self.stack
                            .focused()
                            .active_window()
                            .and_then(|w| w.toplevel())
                            .map(|t| t.wl_surface().clone())
                    };
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        keyboard.set_focus(
                            self,
                            target,
                            smithay::utils::SERIAL_COUNTER.next_serial(),
                        );
                    }
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
                    tracing::error!("failed to advance the Hut stack: {err}");
                }
                if instant {
                    self.sync_visible_main_window();
                }
                // The newly-focused (or newly-previewed) Hut gets resized
                // to the real output size as part of the redraw this
                // triggers (see `winit_backend.rs`'s `resize_all` call,
                // which runs before that frame's texture is generated) —
                // a freshly-spawned one starts at the placeholder default
                // grid size otherwise.
                self.request_redraw();
            }
            Action::StackPrev => {
                let instant = self.keymap.stack_hold().is_empty();
                if instant {
                    self.stack.prev();
                    self.sync_visible_main_window();
                } else {
                    self.stack.preview_prev();
                }
                self.request_redraw();
            }
            Action::TabNext => {
                self.stack.focused_mut().cycle_tab(true);
                self.sync_visible_main_window();
                self.request_redraw();
            }
            Action::TabPrev => {
                self.stack.focused_mut().cycle_tab(false);
                self.sync_visible_main_window();
                self.request_redraw();
            }
            // Depends on the Village management layer (Phase 6).
            // Recognized and intercepted now so rebinding it already
            // works even though it doesn't do anything yet.
            Action::WrapTab | Action::WrapTile => {
                tracing::debug!("{action:?} triggered (not implemented yet)");
            }
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let keycode = event.key_code();
                let key_state = event.state();

                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
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
                            data.stack.commit_preview();
                            data.sync_visible_main_window();
                            data.request_redraw();
                        }

                        // Global keybindings always win, regardless of
                        // whether the terminal or a client window is the
                        // active view — otherwise there'd be no way to
                        // toggle back to the terminal once a client
                        // window takes over the screen.
                        if key_state == KeyState::Pressed {
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
            InputEvent::PointerMotion { .. } => {}
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let serial = SERIAL_COUNTER.next_serial();

                if self.dock_drag.is_some() {
                    docks::advance_drag(self, pos);
                }

                if self.showing_terminal_effective() {
                    let (col, row, left_half) = self.stack.focused().pixel_to_cell(pos.x, pos.y);
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
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
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

                if self.showing_terminal_effective() {
                    let pos = pointer.current_location();
                    let (col, row, left_half) = self.stack.focused().pixel_to_cell(pos.x, pos.y);
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
                            }
                        }
                    }
                } else if pressed && !pointer.is_grabbed() {
                    let pos = pointer.current_location();
                    if docks::start_drag(self, pos) {
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
                        // the floating Sub-Window/Alert system (Phase 5)
                        // adds real per-window focus, every window is
                        // permanently hinted `Activated` (set once in
                        // `new_toplevel`) and nothing else in this handler
                        // changes a window's pending state — resending
                        // configures on every click would just be
                        // unnecessary client-side redraws.
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
                    if self.stack.focused().terminal.wants_mouse_reports() {
                        if let Some(pointer) = self.seat.get_pointer() {
                            let pos = pointer.current_location();
                            let (col, row, _) = self.stack.focused().pixel_to_cell(pos.x, pos.y);
                            let mods = self.current_mods();
                            let wheel_button = if vertical_amount > 0.0 {
                                mudhuts_term::mouse::BUTTON_WHEEL_DOWN
                            } else {
                                mudhuts_term::mouse::BUTTON_WHEEL_UP
                            };
                            self.stack.focused().terminal.report_mouse_button(
                                wheel_button,
                                mods,
                                true,
                                col + 1,
                                row + 1,
                            );
                        }
                    } else {
                        // No app has grabbed the mouse — scroll this
                        // Hut's own scrollback instead of doing nothing.
                        // `vertical_amount > 0.0` is "scroll down" (same
                        // convention as the wheel-report mapping above);
                        // `Terminal::scroll`'s sign is the opposite
                        // (positive moves further *up* into history).
                        let lines = -(vertical_amount / 15.0 * 3.0).round() as i32;
                        let lines = if lines != 0 {
                            lines
                        } else {
                            -vertical_amount.signum() as i32
                        };
                        self.stack.focused().terminal.scroll(lines);
                        self.request_redraw();
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
    }
}
