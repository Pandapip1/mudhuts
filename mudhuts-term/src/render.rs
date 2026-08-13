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

/// Base point size used to rasterize glyphs at scale 1.0 — [`GlyphCache::new`]
/// multiplies this by the real output scale. Cell geometry is derived from
/// the scaled result.
const FONT_SIZE: f32 = 16.0;

pub struct GlyphCache {
    fc: fontconfig::Fontconfig,
    regular: fontdue::Font,
    bold: fontdue::Font,
    /// Fonts loaded on demand to cover characters the primary/bold fonts
    /// lack a glyph for (e.g. Nerd Font/Powerline symbols in a prompt,
    /// when the default monospace font doesn't include them) — otherwise
    /// those show up as tofu boxes.
    fallback_fonts: HashMap<std::path::PathBuf, fontdue::Font>,
    /// Memoizes the fontconfig charset query per character, including
    /// negative results, so an uncovered character isn't re-queried every
    /// frame it's drawn.
    fallback_for_char: HashMap<char, Option<std::path::PathBuf>>,
    cache: HashMap<(char, bool), (fontdue::Metrics, Vec<u8>)>,
    /// `FONT_SIZE * scale` this cache was built for — every glyph in
    /// `cache` was rasterized at this size, so it has to stay fixed for
    /// this instance's lifetime (a scale change means a fresh `GlyphCache`,
    /// not mutating this one — see `ConsoleHut::rescale`).
    font_size: f32,
    cell_width: usize,
    cell_height: usize,
    baseline: usize,
}

impl GlyphCache {
    /// `scale` is the output's real DPI scale (`1.0` on a standard-density
    /// display) — multiplied into `FONT_SIZE` so text renders at the same
    /// *apparent* size regardless of the panel's pixel density, not just
    /// the same pixel count. See `mudhuts::hut::ConsoleHut::rescale`'s doc
    /// comment for why the caller may not know the real value yet at
    /// construction time and might rebuild this later.
    pub fn new(scale: f64) -> Result<Self, String> {
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

        let font_size = FONT_SIZE * (scale.max(0.0) as f32);
        let advance = regular.metrics('M', font_size).advance_width;
        let line_metrics = regular
            .horizontal_line_metrics(font_size)
            .ok_or("font has no horizontal line metrics")?;
        let cell_width = advance.ceil().max(1.0) as usize;
        let cell_height = line_metrics.new_line_size.ceil().max(1.0) as usize;
        let baseline = line_metrics.ascent.round() as usize;

        Ok(Self {
            fc,
            regular,
            bold,
            fallback_fonts: HashMap::new(),
            fallback_for_char: HashMap::new(),
            cache: HashMap::new(),
            font_size,
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

    /// Baseline offset (from the top of a cell) glyphs should be drawn at.
    pub fn baseline(&self) -> usize {
        self.baseline
    }

    /// The primary/bold font if it covers `c`, otherwise a fontconfig-found
    /// fallback that does (loaded and cached on first use), otherwise the
    /// primary/bold font anyway (nothing on the system covers `c`).
    fn font_for_char(&mut self, c: char, bold: bool) -> &fontdue::Font {
        let primary_has_it = if bold {
            self.bold.has_glyph(c)
        } else {
            self.regular.has_glyph(c)
        };
        if primary_has_it {
            return if bold { &self.bold } else { &self.regular };
        }

        let fallback_path = match self.fallback_for_char.get(&c) {
            Some(cached) => cached.clone(),
            None => {
                let found = find_fallback_font_path(&self.fc, c);
                self.fallback_for_char.insert(c, found.clone());
                found
            }
        };

        if let Some(path) = &fallback_path
            && !self.fallback_fonts.contains_key(path)
        {
            let loaded = std::fs::read(path).ok().and_then(|bytes| {
                fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
            });
            if let Some(font) = loaded {
                self.fallback_fonts.insert(path.clone(), font);
            }
        }

        match fallback_path
            .as_ref()
            .and_then(|p| self.fallback_fonts.get(p))
        {
            Some(font) if font.has_glyph(c) => font,
            _ => {
                if bold {
                    &self.bold
                } else {
                    &self.regular
                }
            }
        }
    }

    /// The rasterized glyph (coverage bitmap) for `(c, bold)`, cached after
    /// the first call. Used both by the CPU [`render`] path and the GPU
    /// atlas path (`mudhuts::gpu_term`), which uploads this bitmap into a
    /// texture once per unique glyph instead of blitting it every frame.
    pub fn glyph(&mut self, c: char, bold: bool) -> &(fontdue::Metrics, Vec<u8>) {
        if !self.cache.contains_key(&(c, bold)) {
            let font_size = self.font_size;
            let rasterized = self.font_for_char(c, bold).rasterize(c, font_size);
            self.cache.insert((c, bold), rasterized);
        }
        // Unreachable in practice (just inserted above if missing), but
        // avoid any panic path: render blank rather than unwrap/expect.
        static EMPTY: std::sync::OnceLock<(fontdue::Metrics, Vec<u8>)> = std::sync::OnceLock::new();
        self.cache
            .get(&(c, bold))
            .unwrap_or_else(|| EMPTY.get_or_init(|| (fontdue::Metrics::default(), Vec::new())))
    }
}

/// Ask fontconfig (via `FcFontMatch` with a charset requirement, which
/// applies the system's normal font-substitution/fallback rules) for a font
/// that actually contains `c`.
fn find_fallback_font_path(fc: &fontconfig::Fontconfig, c: char) -> Option<std::path::PathBuf> {
    let mut pattern = fontconfig::Pattern::new(fc).ok()?;
    let mut charset = fontconfig::CharSet::new(fc).ok()?;
    charset.add_char(c).ok()?;
    pattern.add_charset(charset).ok()?;
    let matched = pattern.font_match().ok()?;
    Some(std::path::PathBuf::from(matched.filename().ok()?))
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

/// One visible cell's resolved (post cursor/selection/inverse) colors and
/// character, for renderers that don't rasterize to a CPU pixel buffer
/// themselves (the GPU atlas path — see `mudhuts::gpu_term`). Shares the
/// exact same color-resolution logic as [`render`] so the two backends
/// produce identical output.
#[derive(Debug, Clone, Copy)]
pub struct CellInfo {
    pub col: usize,
    pub row: usize,
    pub c: char,
    pub bold: bool,
    pub fg: Rgb,
    pub bg: Rgb,
}

/// Collect every visible cell's resolved colors/character. Unlike
/// [`render`], this always covers the whole viewport — the GPU path
/// redraws every cell whenever *anything* changed rather than tracking
/// fine-grained per-line damage, since instanced GPU draws make that
/// optimization unnecessary (see the Phase 2.6 plan notes).
pub fn collect_cells(content: RenderableContent<'_>) -> Vec<CellInfo> {
    let colors: &Colors = content.colors;
    let cursor_point = content.cursor.point;
    let cursor_shape = content.cursor.shape;
    let cursor_visible = !matches!(cursor_shape, CursorShape::Hidden);
    let selection = content.selection;

    let display_offset = content.display_offset as i32;
    let mut cells = Vec::new();
    for indexed in content.display_iter {
        // A display-iterator point's `line` is in *grid* space, not
        // viewport space — line 0 there is the top of the non-scrolled
        // active region, not necessarily the top of what's currently
        // visible. The actual on-screen row is `line + display_offset`
        // (see `alacritty_terminal::term::point_to_viewport`); using
        // `line` directly meant most/every row went negative and got
        // dropped as soon as the view was scrolled at all.
        let row = (indexed.point.line.0 + display_offset) as usize;
        let col = indexed.point.column.0;
        let cell = indexed.cell;

        let is_cursor_cell = cursor_visible && indexed.point == cursor_point;
        let is_selected =
            selection.is_some_and(|sel| sel.contains_cell(&indexed, cursor_point, cursor_shape));

        let mut fg = palette::resolve_fg(cell.fg, cell.flags, colors);
        let mut bg = palette::resolve_bg(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) || is_selected || is_cursor_cell {
            std::mem::swap(&mut fg, &mut bg);
        }

        cells.push(CellInfo {
            col,
            row,
            c: cell.c,
            bold: cell.flags.contains(Flags::BOLD),
            fg,
            bg,
        });
    }
    cells
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
    let cursor_shape = content.cursor.shape;
    let cursor_visible = !matches!(cursor_shape, CursorShape::Hidden);
    let selection = content.selection;
    let display_offset = content.display_offset as i32;

    for indexed in content.display_iter {
        // See `collect_cells`'s identical fix: a display-iterator point's
        // `line` is grid space, not viewport space — the on-screen row
        // needs `line + display_offset`.
        let line = (indexed.point.line.0 + display_offset) as usize;
        let col = indexed.point.column.0;

        if !is_full {
            match damaged_cols_for_line.get(&line) {
                Some(&(left, right)) if col >= left && col <= right => {}
                _ => continue,
            }
        }

        let cell = indexed.cell;

        let is_cursor_cell = cursor_visible && indexed.point == cursor_point;
        let is_selected =
            selection.is_some_and(|sel| sel.contains_cell(&indexed, cursor_point, cursor_shape));

        let mut fg = palette::resolve_fg(cell.fg, cell.flags, colors);
        let mut bg = palette::resolve_bg(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) || is_selected || is_cursor_cell {
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
