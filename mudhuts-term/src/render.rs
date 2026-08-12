//! CPU rasterization of a terminal grid into an RGBA8 buffer, using
//! `fontdue` for glyph rendering and the system's fontconfig-resolved
//! monospace font. The resulting buffer is meant to be handed to
//! `smithay::backend::renderer::element::memory::MemoryRenderBuffer`.

use std::collections::HashMap;

use alacritty_terminal::term::RenderableContent;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::CursorShape;

use crate::palette::{self, Rgb};

/// Point size used to rasterize glyphs. Cell geometry is derived from this.
const FONT_SIZE: f32 = 16.0;

pub struct GlyphCache {
    regular: fontdue::Font,
    bold: fontdue::Font,
    cache: HashMap<(char, bool), (fontdue::Metrics, Vec<u8>)>,
    cell_width: usize,
    cell_height: usize,
    baseline: usize,
}

impl GlyphCache {
    pub fn new() -> Result<Self, String> {
        let fc = fontconfig::Fontconfig::new().ok_or("failed to initialize fontconfig")?;
        let regular_path = fc.find("monospace", None).map_err(|e| e.to_string())?.path;
        let bold_path = fc
            .find("monospace", Some("Bold"))
            .map(|f| f.path)
            .unwrap_or_else(|_| regular_path.clone());

        let load = |path: &std::path::Path| -> Result<fontdue::Font, String> {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                .map_err(|e| e.to_string())
        };

        let regular = load(&regular_path)?;
        let bold = load(&bold_path).or_else(|_| load(&regular_path))?;

        let advance = regular.metrics('M', FONT_SIZE).advance_width;
        let line_metrics = regular
            .horizontal_line_metrics(FONT_SIZE)
            .ok_or("font has no horizontal line metrics")?;
        let cell_width = advance.ceil().max(1.0) as usize;
        let cell_height = line_metrics.new_line_size.ceil().max(1.0) as usize;
        let baseline = line_metrics.ascent.round() as usize;

        Ok(Self {
            regular,
            bold,
            cache: HashMap::new(),
            cell_width,
            cell_height,
            baseline,
        })
    }

    pub fn cell_width(&self) -> usize {
        self.cell_width
    }

    pub fn cell_height(&self) -> usize {
        self.cell_height
    }

    fn glyph(&mut self, c: char, bold: bool) -> &(fontdue::Metrics, Vec<u8>) {
        self.cache
            .entry((c, bold))
            .or_insert_with_key(|&(c, bold)| {
                let font = if bold { &self.bold } else { &self.regular };
                font.rasterize(c, FONT_SIZE)
            })
    }
}

fn put_pixel(buf: &mut [u8], width: usize, height: usize, x: i64, y: i64, rgb: Rgb) {
    if x < 0 || y < 0 || x as usize >= width || y as usize >= height {
        return;
    }
    let idx = (y as usize * width + x as usize) * 4;
    let Some(px) = buf.get_mut(idx..idx + 4) else {
        return;
    };
    px[0] = rgb[0];
    px[1] = rgb[1];
    px[2] = rgb[2];
    px[3] = 255;
}

/// Read back a pixel previously written by [`put_pixel`], or black if the
/// coordinates (or a caller-side buffer-size mismatch) put it out of range.
fn get_pixel(buf: &[u8], width: usize, height: usize, x: i64, y: i64) -> Rgb {
    if x < 0 || y < 0 || x as usize >= width || y as usize >= height {
        return [0, 0, 0];
    }
    let idx = (y as usize * width + x as usize) * 4;
    match buf.get(idx..idx + 3) {
        Some(px) => [px[0], px[1], px[2]],
        None => [0, 0, 0],
    }
}

fn fill_cell(
    buf: &mut [u8],
    width: usize,
    height: usize,
    cell_x: usize,
    cell_y: usize,
    cw: usize,
    ch: usize,
    rgb: Rgb,
) {
    for dy in 0..ch {
        for dx in 0..cw {
            put_pixel(
                buf,
                width,
                height,
                (cell_x + dx) as i64,
                (cell_y + dy) as i64,
                rgb,
            );
        }
    }
}

fn blend(bg: Rgb, fg: Rgb, coverage: u8) -> Rgb {
    let a = coverage as u32;
    let inv = 255 - a;
    [
        ((fg[0] as u32 * a + bg[0] as u32 * inv) / 255) as u8,
        ((fg[1] as u32 * a + bg[1] as u32 * inv) / 255) as u8,
        ((fg[2] as u32 * a + bg[2] as u32 * inv) / 255) as u8,
    ]
}

/// A rectangular region of the RGBA buffer that changed and needs
/// re-uploading to the GPU.
#[derive(Debug, Clone, Copy)]
pub struct PixelRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Which grid lines changed since the last render, mirroring
/// `alacritty_terminal::term::TermDamage` but as owned data (so it can
/// outlive the `Term` lock needed to compute [`RenderableContent`]).
#[derive(Debug, Clone)]
pub enum Damage {
    /// Redraw everything (first frame, or after a resize).
    Full,
    /// Only these lines (and, within a line, only these columns) changed.
    Lines(Vec<LineDamage>),
}

#[derive(Debug, Clone, Copy)]
pub struct LineDamage {
    pub line: usize,
    pub left: usize,
    pub right: usize,
}

/// Render the current terminal viewport into an RGBA8 `buf` of size
/// `width * height * 4`, only touching cells covered by `damage`. Returns
/// the pixel-space rectangles that were actually redrawn, for damage
/// tracking further up the pipeline. Cells beyond the buffer are clipped
/// silently.
pub fn render(
    content: RenderableContent<'_>,
    glyphs: &mut GlyphCache,
    buf: &mut [u8],
    width: usize,
    height: usize,
    damage: &Damage,
) -> Vec<PixelRect> {
    let cw = glyphs.cell_width;
    let ch = glyphs.cell_height;

    if matches!(damage, Damage::Full) {
        // Also covers any leftover margin pixels a previous, larger size
        // may have left behind when width/height isn't an exact multiple
        // of the cell size.
        buf.fill(0);
    }

    let damaged_cols_for_line: std::collections::HashMap<usize, (usize, usize)> = match damage {
        Damage::Full => std::collections::HashMap::new(),
        Damage::Lines(lines) => lines.iter().map(|l| (l.line, (l.left, l.right))).collect(),
    };
    let is_full = matches!(damage, Damage::Full);

    let baseline = glyphs.baseline;
    let colors: &Colors = content.colors;
    let cursor_point = content.cursor.point;
    let cursor_visible = !matches!(content.cursor.shape, CursorShape::Hidden);

    for indexed in content.display_iter {
        let line = indexed.point.line.0;
        if line < 0 {
            continue;
        }
        let line = line as usize;
        let col = indexed.point.column.0;

        if !is_full {
            match damaged_cols_for_line.get(&line) {
                Some(&(left, right)) if col >= left && col <= right => {}
                _ => continue,
            }
        }

        let cell = indexed.cell;

        let is_cursor_cell = cursor_visible && indexed.point == cursor_point;

        let mut fg = palette::resolve_fg(cell.fg, cell.flags, colors);
        let mut bg = palette::resolve_bg(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) || is_cursor_cell {
            std::mem::swap(&mut fg, &mut bg);
        }

        let px_x = col * cw;
        let px_y = line * ch;
        fill_cell(buf, width, height, px_x, px_y, cw, ch, bg);

        if cell.c != ' ' && cell.c != '\0' {
            let bold = cell.flags.contains(Flags::BOLD);
            let (metrics, bitmap) = glyphs.glyph(cell.c, bold);
            let glyph_x = px_x as i64 + metrics.xmin as i64;
            let glyph_y =
                px_y as i64 + baseline as i64 - metrics.height as i64 - metrics.ymin as i64;
            for gy in 0..metrics.height {
                for gx in 0..metrics.width {
                    let Some(&coverage) = bitmap.get(gy * metrics.width + gx) else {
                        continue;
                    };
                    if coverage == 0 {
                        continue;
                    }
                    let x = glyph_x + gx as i64;
                    let y = glyph_y + gy as i64;
                    let existing = get_pixel(buf, width, height, x, y);
                    let blended = blend(existing, fg, coverage);
                    put_pixel(buf, width, height, x, y, blended);
                }
            }
        }
    }

    match damage {
        Damage::Full => vec![PixelRect {
            x: 0,
            y: 0,
            width,
            height,
        }],
        Damage::Lines(lines) => lines
            .iter()
            .map(|l| PixelRect {
                x: l.left * cw,
                y: l.line * ch,
                width: (l.right.saturating_sub(l.left) + 1) * cw,
                height: ch,
            })
            .collect(),
    }
}
