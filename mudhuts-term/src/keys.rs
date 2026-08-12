//! Translate compositor-level key events into the byte sequences a PTY
//! client expects, following the standard xterm/VT100 encoding (the same
//! sequences alacritty's `input/keyboard.rs` builds). Kept independent of
//! xkbcommon/smithay so this crate has no compositor dependency.

use alacritty_terminal::term::TermMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub logo: bool,
}

impl Mods {
    fn is_empty(self) -> bool {
        !self.shift && !self.alt && !self.ctrl && !self.logo
    }

    /// xterm modifier parameter: 1 + (shift=1, alt=2, ctrl=4, super=8).
    fn code(self) -> u8 {
        1 + self.shift as u8 + self.alt as u8 * 2 + self.ctrl as u8 * 4 + self.logo as u8 * 8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Named(NamedKey),
    /// A printable/control character already resolved by xkbcommon
    /// (e.g. via `xkb_state_key_get_utf8`), including Ctrl-modified
    /// control codes such as 0x03 for Ctrl+C.
    Text(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Escape,
    Backspace,
    Tab,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Up,
    Down,
    Left,
    Right,
    F(u8),
}

/// Encode a key press into the bytes that should be written to the PTY,
/// or `None` if this key produces no PTY input (e.g. a bare modifier).
pub fn encode(key: Key, mods: Mods, mode: TermMode) -> Option<Vec<u8>> {
    match key {
        Key::Named(NamedKey::Up) => Some(app_or_csi(mods, mode, 'A')),
        Key::Named(NamedKey::Down) => Some(app_or_csi(mods, mode, 'B')),
        Key::Named(NamedKey::Right) => Some(app_or_csi(mods, mode, 'C')),
        Key::Named(NamedKey::Left) => Some(app_or_csi(mods, mode, 'D')),
        Key::Named(NamedKey::Home) => Some(app_or_csi(mods, mode, 'H')),
        Key::Named(NamedKey::End) => Some(app_or_csi(mods, mode, 'F')),
        Key::Named(NamedKey::PageUp) => Some(csi_tilde(mods, "5")),
        Key::Named(NamedKey::PageDown) => Some(csi_tilde(mods, "6")),
        Key::Named(NamedKey::Insert) => Some(csi_tilde(mods, "2")),
        Key::Named(NamedKey::Delete) => Some(csi_tilde(mods, "3")),
        Key::Named(NamedKey::F(n)) => Some(function_key(n, mods)),
        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Escape) => Some(b"\x1b".to_vec()),
        Key::Named(NamedKey::Backspace) => Some(if mods.alt {
            b"\x1b\x7f".to_vec()
        } else {
            b"\x7f".to_vec()
        }),
        Key::Named(NamedKey::Tab) => Some(if mods.shift {
            b"\x1b[Z".to_vec()
        } else {
            b"\t".to_vec()
        }),
        Key::Text(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            let bytes = if mods.alt {
                let mut v = vec![0x1b];
                v.extend_from_slice(s.as_bytes());
                v
            } else {
                s.as_bytes().to_vec()
            };
            Some(bytes)
        }
    }
}

/// Arrow keys and Home/End: SS3 sequences in application-cursor mode when
/// unmodified, otherwise the general CSI form (`CSI <letter>` bare, or
/// `CSI 1;<mods><letter>` when modified).
fn app_or_csi(mods: Mods, mode: TermMode, letter: char) -> Vec<u8> {
    if mods.is_empty() {
        if mode.contains(TermMode::APP_CURSOR) {
            return format!("\x1bO{letter}").into_bytes();
        }
        return format!("\x1b[{letter}").into_bytes();
    }
    format!("\x1b[1;{}{}", mods.code(), letter).into_bytes()
}

/// F1-F4: always SS3 when unmodified (independent of application-cursor
/// mode), else `CSI 1;<mods><letter>`.
fn ss3_or_csi(mods: Mods, letter: char) -> Vec<u8> {
    if mods.is_empty() {
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[1;{}{}", mods.code(), letter).into_bytes()
    }
}

/// `CSI <n>[;<mods>]~` form used by PageUp/PageDown/Insert/Delete.
fn csi_tilde(mods: Mods, n: &str) -> Vec<u8> {
    if mods.is_empty() {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{}~", mods.code()).into_bytes()
    }
}

fn function_key(n: u8, mods: Mods) -> Vec<u8> {
    match n {
        1..=4 => {
            let letter = (b'P' + (n - 1)) as char;
            ss3_or_csi(mods, letter)
        }
        5..=12 => {
            let code = match n {
                5 => "15",
                6 => "17",
                7 => "18",
                8 => "19",
                9 => "20",
                10 => "21",
                11 => "23",
                _ => "24",
            };
            csi_tilde(mods, code)
        }
        _ => Vec::new(),
    }
}
