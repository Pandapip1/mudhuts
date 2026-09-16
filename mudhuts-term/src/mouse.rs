//! Encoding for terminal mouse reporting (the escape sequences apps like
//! `btop`/`vim`/`tmux` request via `DECSET ?1000h` and friends) and for
//! driving `alacritty_terminal`'s built-in click-drag text selection.
//!
//! Only the modern SGR encoding (`DECSET ?1006h`) is supported — virtually
//! everything that still cares about mouse reporting pairs it with SGR
//! these days, and the legacy X10/UTF-8 encodings have awkward coordinate
//! limits that aren't worth the extra code path.

use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;

use crate::keys::Mods;

/// xterm mouse button numbers (before modifier bits are added in).
pub const BUTTON_LEFT: u32 = 0;
pub const BUTTON_MIDDLE: u32 = 1;
pub const BUTTON_RIGHT: u32 = 2;
pub const BUTTON_WHEEL_UP: u32 = 64;
pub const BUTTON_WHEEL_DOWN: u32 = 65;

/// Whether `mode` indicates the app wants SGR-encoded mouse reports at all
/// (some combination of click/drag/motion reporting).
pub fn wants_reports(mode: TermMode) -> bool {
    mode.contains(TermMode::SGR_MOUSE) && mode.intersects(TermMode::MOUSE_MODE)
}

/// Whether `mode` indicates motion should be reported while `button` is
/// currently held (`DECSET ?1002h`/`?1003h`).
pub fn wants_drag_reports(mode: TermMode) -> bool {
    mode.contains(TermMode::SGR_MOUSE)
        && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
}

/// `CSI < Cb ; Cx ; Cy M` (press) or `...m` (release). `col`/`row` are
/// 1-based cell coordinates.
pub fn encode_button(button: u32, mods: Mods, pressed: bool, col: usize, row: usize) -> Vec<u8> {
    let mut code = button;
    if mods.shift {
        code += 4;
    }
    if mods.alt {
        code += 8;
    }
    if mods.ctrl {
        code += 16;
    }
    let suffix = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{code};{col};{row}{suffix}").into_bytes()
}

/// Motion while a button is held reports the same button code plus 32,
/// always terminated `M` (there's no separate "release" for motion).
pub fn encode_drag(button: u32, mods: Mods, col: usize, row: usize) -> Vec<u8> {
    encode_button(button + 32, mods, true, col, row)
}

/// `row` is screen-relative (0 = the top of whatever's currently
/// visible, matching `pixel_to_cell`'s own contract), but `Selection`'s
/// `Point`/`Line` is *not* — per `alacritty_terminal`'s own grid module
/// doc, `Line(0)` is fixed to the top of the live screen regardless of
/// scroll, and scrolled-into-history content has *negative* `Line`
/// values; the current viewport only lines up with `Line(0)` when
/// `display_offset` is `0` (not scrolled back at all). `display_offset`
/// bridges the two, the same conversion `render.rs`'s own
/// `line + display_offset` makes in the opposite direction. Missing this
/// was a real, shipped bug (caught after the fact, not in review): with
/// no adjustment, dragging a selection while scrolled into history
/// landed `display_offset` lines below where the cursor actually was,
/// and the error grew every time the view scrolled further while
/// dragging.
fn point(col: usize, row: usize, display_offset: usize) -> Point {
    Point::new(Line(row as i32 - display_offset as i32), Column(col))
}

/// Start a new simple (character-granularity) selection anchored at
/// `(col, row)`. `left_half` indicates which half of the cell was clicked,
/// which affects selection boundary precision (matches how real terminals
/// behave when you start dragging from partway into a cell). `row` and
/// `display_offset` are both screen-relative — see `point`'s own doc
/// comment.
pub fn start_selection(col: usize, row: usize, display_offset: usize, left_half: bool) -> Selection {
    let side = if left_half { Side::Left } else { Side::Right };
    Selection::new(SelectionType::Simple, point(col, row, display_offset), side)
}

/// Extend an in-progress selection to `(col, row)` — see `point`'s own
/// doc comment for `display_offset`.
pub fn extend_selection(selection: &mut Selection, col: usize, row: usize, display_offset: usize, left_half: bool) {
    let side = if left_half { Side::Left } else { Side::Right };
    selection.update(point(col, row, display_offset), side);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool, alt: bool, ctrl: bool) -> Mods {
        Mods {
            shift,
            alt,
            ctrl,
            logo: false,
        }
    }

    #[test]
    fn encodes_plain_press_and_release() {
        assert_eq!(
            encode_button(BUTTON_LEFT, mods(false, false, false), true, 5, 10),
            b"\x1b[<0;5;10M".to_vec()
        );
        assert_eq!(
            encode_button(BUTTON_LEFT, mods(false, false, false), false, 5, 10),
            b"\x1b[<0;5;10m".to_vec()
        );
    }

    #[test]
    fn modifier_bits_add_to_the_button_code() {
        // shift=4, alt=8, ctrl=16, matching the xterm SGR mouse spec.
        assert_eq!(
            encode_button(0, mods(true, false, false), true, 1, 1),
            b"\x1b[<4;1;1M".to_vec()
        );
        assert_eq!(
            encode_button(0, mods(false, true, false), true, 1, 1),
            b"\x1b[<8;1;1M".to_vec()
        );
        assert_eq!(
            encode_button(0, mods(false, false, true), true, 1, 1),
            b"\x1b[<16;1;1M".to_vec()
        );
        assert_eq!(
            encode_button(0, mods(true, true, true), true, 1, 1),
            b"\x1b[<28;1;1M".to_vec()
        );
    }

    #[test]
    fn different_buttons_get_different_codes() {
        assert_eq!(
            encode_button(BUTTON_RIGHT, mods(false, false, false), true, 1, 1),
            b"\x1b[<2;1;1M".to_vec()
        );
        assert_eq!(
            encode_button(BUTTON_MIDDLE, mods(false, false, false), true, 1, 1),
            b"\x1b[<1;1;1M".to_vec()
        );
    }

    #[test]
    fn wheel_uses_the_high_button_codes() {
        assert_eq!(
            encode_button(BUTTON_WHEEL_UP, mods(false, false, false), true, 3, 4),
            b"\x1b[<64;3;4M".to_vec()
        );
        assert_eq!(
            encode_button(BUTTON_WHEEL_DOWN, mods(false, false, false), true, 3, 4),
            b"\x1b[<65;3;4M".to_vec()
        );
    }

    #[test]
    fn drag_adds_32_and_is_always_a_press() {
        let dragged = encode_drag(BUTTON_LEFT, mods(false, false, false), 7, 8);
        assert_eq!(dragged, b"\x1b[<32;7;8M".to_vec());
    }

    #[test]
    fn no_reports_wanted_without_sgr_mode() {
        // MOUSE_REPORT_CLICK alone, no SGR_MOUSE: legacy X10 encoding,
        // which we don't support, so this should not claim to want reports.
        assert!(!wants_reports(TermMode::MOUSE_REPORT_CLICK));
    }

    #[test]
    fn no_reports_wanted_with_sgr_alone_and_no_mouse_mode() {
        // SGR_MOUSE without any of click/drag/motion enabled: nothing
        // actually asked to be reported.
        assert!(!wants_reports(TermMode::SGR_MOUSE));
    }

    #[test]
    fn reports_wanted_once_both_sgr_and_a_mouse_mode_are_set() {
        assert!(wants_reports(
            TermMode::SGR_MOUSE | TermMode::MOUSE_REPORT_CLICK
        ));
        assert!(wants_reports(TermMode::SGR_MOUSE | TermMode::MOUSE_MOTION));
        assert!(wants_reports(TermMode::SGR_MOUSE | TermMode::MOUSE_DRAG));
    }

    #[test]
    fn drag_reports_require_drag_or_motion_specifically() {
        // Click-only reporting shouldn't trigger continuous drag reports.
        assert!(!wants_drag_reports(
            TermMode::SGR_MOUSE | TermMode::MOUSE_REPORT_CLICK
        ));
        assert!(wants_drag_reports(
            TermMode::SGR_MOUSE | TermMode::MOUSE_DRAG
        ));
        assert!(wants_drag_reports(
            TermMode::SGR_MOUSE | TermMode::MOUSE_MOTION
        ));
    }

    #[test]
    fn selection_start_and_extend_produce_a_range() {
        // Smoke test of the thin Selection wrapper against a real Term via
        // alacritty_terminal's own test helper, mostly to catch a
        // Line/Column mixup in `point()`. Not scrolled (`display_offset`
        // 0), so screen row and grid line coincide.
        let term = alacritty_terminal::term::test::mock_term("hello\nworld");
        let mut selection = start_selection(0, 0, 0, true);
        extend_selection(&mut selection, 4, 0, 0, false);
        let range = selection.to_range(&term).expect("should produce a range");
        assert_eq!(range.start, Point::new(Line(0), Column(0)));
        assert_eq!(range.end, Point::new(Line(0), Column(4)));
    }

    #[test]
    fn point_subtracts_display_offset_from_the_screen_relative_row() {
        // The exact bug this pins: `row` is screen-relative (0 = top of
        // the current viewport), but `Point`'s own `Line` isn't — it's
        // fixed to the live screen regardless of scroll, with history
        // living at negative lines. Not scrolled: they coincide.
        assert_eq!(point(0, 5, 0), Point::new(Line(5), Column(0)));
        // Scrolled 3 lines into history: the same screen row now refers
        // to a grid line 3 further back than its own row number.
        assert_eq!(point(0, 5, 3), Point::new(Line(2), Column(0)));
        // Scrolled back further than the row itself — lands in negative
        // (historical) grid-line territory, exactly like a real
        // scrolled-back terminal.
        assert_eq!(point(0, 2, 5), Point::new(Line(-3), Column(0)));
    }
}
