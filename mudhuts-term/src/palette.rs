//! Default 256-color ANSI palette + resolution of alacritty_terminal's
//! [`Color`] values (which may reference user-set overrides in a live
//! [`Colors`] table) down to concrete RGB, mirroring what a real terminal
//! frontend (e.g. the `alacritty` binary's `display/content.rs`) does.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

pub type Rgb = [u8; 3];

const BASE16: [Rgb; 16] = [
    [0x00, 0x00, 0x00], // Black
    [0xcd, 0x00, 0x00], // Red
    [0x00, 0xcd, 0x00], // Green
    [0xcd, 0xcd, 0x00], // Yellow
    [0x00, 0x00, 0xee], // Blue
    [0xcd, 0x00, 0xcd], // Magenta
    [0x00, 0xcd, 0xcd], // Cyan
    [0xe5, 0xe5, 0xe5], // White
    [0x7f, 0x7f, 0x7f], // BrightBlack
    [0xff, 0x00, 0x00], // BrightRed
    [0x00, 0xff, 0x00], // BrightGreen
    [0xff, 0xff, 0x00], // BrightYellow
    [0x5c, 0x5c, 0xff], // BrightBlue
    [0xff, 0x00, 0xff], // BrightMagenta
    [0x00, 0xff, 0xff], // BrightCyan
    [0xff, 0xff, 0xff], // BrightWhite
];

const FOREGROUND: Rgb = [0xe5, 0xe5, 0xe5];
const BACKGROUND: Rgb = [0x00, 0x00, 0x00];
const CURSOR: Rgb = [0xe5, 0xe5, 0xe5];

fn dim(c: Rgb) -> Rgb {
    [
        (c[0] as f32 * 0.66) as u8,
        (c[1] as f32 * 0.66) as u8,
        (c[2] as f32 * 0.66) as u8,
    ]
}

/// The built-in fallback palette, indexed exactly like
/// `alacritty_terminal::term::color::Colors` (0..=255 is the standard
/// 16-color + 6x6x6 cube + grayscale-ramp layout; 256.. are the named
/// extras such as `Foreground`/`Background`/`Cursor`).
fn default_entry(index: usize) -> Rgb {
    match index {
        0..=15 => BASE16[index],
        16..=231 => {
            let i = index - 16;
            let to_level = |v: usize| if v == 0 { 0 } else { (v * 40 + 55) as u8 };
            let r = to_level(i / 36);
            let g = to_level((i / 6) % 6);
            let b = to_level(i % 6);
            [r, g, b]
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            [v, v, v]
        }
        _ if index == NamedColor::Foreground as usize => FOREGROUND,
        _ if index == NamedColor::Background as usize => BACKGROUND,
        _ if index == NamedColor::Cursor as usize => CURSOR,
        _ if index == NamedColor::BrightForeground as usize => FOREGROUND,
        _ if index == NamedColor::DimForeground as usize => dim(FOREGROUND),
        _ if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&index) => {
            dim(BASE16[index - NamedColor::DimBlack as usize])
        }
        _ => FOREGROUND,
    }
}

fn lookup(colors: &Colors, index: usize) -> Rgb {
    colors[index]
        .map(|rgb| [rgb.r, rgb.g, rgb.b])
        .unwrap_or_else(|| default_entry(index))
}

/// Resolve a cell's foreground color, honoring dim/bold-as-bright the way
/// real terminals do (see alacritty's `compute_fg_rgb`).
pub fn resolve_fg(color: Color, flags: Flags, colors: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => {
            let rgb = [rgb.r, rgb.g, rgb.b];
            if flags.contains(Flags::DIM) {
                dim(rgb)
            } else {
                rgb
            }
        }
        Color::Named(named) => match (flags.contains(Flags::DIM), flags.contains(Flags::BOLD)) {
            (true, _) => lookup(colors, named.to_dim() as usize),
            (false, true) => lookup(colors, named.to_bright() as usize),
            (false, false) => lookup(colors, named as usize),
        },
        Color::Indexed(idx) => {
            let idx = match (flags.contains(Flags::BOLD), idx) {
                (true, 0..=7) => idx as usize + 8,
                _ => idx as usize,
            };
            lookup(colors, idx)
        }
    }
}

/// Resolve a cell's background color.
pub fn resolve_bg(color: Color, colors: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Named(named) => lookup(colors, named as usize),
        Color::Indexed(idx) => lookup(colors, idx as usize),
    }
}
