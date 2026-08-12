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
                // anything.
                if self.space.elements().next().is_some() {
                    self.showing_terminal = !self.showing_terminal;
                }
            }
            // Depend on later phases — see the plan (multi-Hut Stack:
            // Phase 3; Main Window tab cycling: Phase 4; Village
            // tiling/tabbing: Phase 6). Recognized and intercepted now so
            // rebinding them already works even though they don't do
            // anything yet.
            Action::StackNext
            | Action::StackPrev
            | Action::TabNext
            | Action::TabPrev
            | Action::WrapTab
            | Action::WrapTile => {
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
                            let mode = data.hut.terminal.mode();
                            if let Some(bytes) = encode(&keysym, mods, mode) {
                                data.hut.terminal.write_input(bytes);
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

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    if let Some((window, _loc)) = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, l)| (w.clone(), l))
                    {
                        self.space.raise_element(&window, true);
                        if let (Some(keyboard), Some(toplevel)) =
                            (self.seat.get_keyboard(), window.toplevel())
                        {
                            keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                        }
                        for w in self.space.elements() {
                            if let Some(toplevel) = w.toplevel() {
                                toplevel.send_pending_configure();
                            }
                        }
                    } else {
                        for w in self.space.elements() {
                            w.set_activated(false);
                            if let Some(toplevel) = w.toplevel() {
                                toplevel.send_pending_configure();
                            }
                        }
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                        }
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
