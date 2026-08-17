//! Configurable chrome colors — the Main Window tab strip
//! (`chrome.rs`), the Hut-level tab strip (`village_chrome.rs`), docked
//! Floating Window handles (`docks.rs`), and a genuinely-tiled Tile-Hut's
//! active-pane border (`render.rs`'s `build_tile_elements`) — overridable
//! via `~/.config/mudhuts/config.toml`'s `[theme]` section, the same
//! file/mechanism `keybindings.rs`'s `[keybindings]` section uses (see
//! [`crate::config::config_path`]).
//!
//! Fonts are deliberately out of scope here, despite being raised
//! alongside colors in the wishlist this came from: swapping a color is a
//! pure data change, but a real font override means loading a different
//! font file via fontconfig and re-deriving `GlyphCache`'s cell metrics
//! from it — a materially bigger, separately-risky change, not attempted
//! in this pass.

use std::collections::HashMap;

use mudhuts_term::palette::Rgb;

use crate::config::ConfigFileContents;

pub struct Theme {
    pub tab_active_fg: Rgb,
    pub tab_active_bg: Rgb,
    pub tab_inactive_fg: Rgb,
    pub tab_inactive_bg: Rgb,
    pub hut_tab_active_fg: Rgb,
    pub hut_tab_active_bg: Rgb,
    pub hut_tab_inactive_fg: Rgb,
    pub hut_tab_inactive_bg: Rgb,
    pub dock_fg: Rgb,
    pub dock_bg: Rgb,
    pub tile_border: Rgb,
}

impl Default for Theme {
    /// The exact colors every one of these was hardcoded to before this
    /// module existed — a config with no `[theme]` section (or no config
    /// file at all) looks identical to before.
    fn default() -> Self {
        Self {
            tab_active_fg: [255, 255, 255],
            tab_active_bg: [64, 115, 191],
            tab_inactive_fg: [190, 190, 190],
            tab_inactive_bg: [30, 30, 30],
            hut_tab_active_fg: [255, 255, 255],
            hut_tab_active_bg: [140, 90, 191],
            hut_tab_inactive_fg: [190, 190, 190],
            hut_tab_inactive_bg: [40, 30, 50],
            dock_fg: [220, 220, 220],
            dock_bg: [50, 50, 60],
            tile_border: [76, 153, 255],
        }
    }
}

impl Theme {
    /// Load the default theme, then apply overrides from
    /// `~/.config/mudhuts/config.toml`'s `[theme]` section if present —
    /// same "any problem is logged and skipped, never fatal" convention
    /// as `Keymap::load`. `config_file` is read once by the caller
    /// (`State::new`) and shared across all four `*Config::load()`s —
    /// see `crate::config::read_config_file`'s own doc comment for why
    /// this doesn't read it itself.
    pub(crate) fn load(config_file: &ConfigFileContents) -> Self {
        let mut theme = Self::default();
        Self::apply_toml_overrides(&mut theme, &config_file.contents, &config_file.source);
        theme
    }

    fn apply_toml_overrides(theme: &mut Theme, contents: &str, source: &str) {
        let config: ConfigFile = match toml::from_str(contents) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("failed to parse config at {source}: {err}");
                return;
            }
        };
        for (name, value) in config.theme {
            let slot = match name.as_str() {
                "tab-active-fg" => &mut theme.tab_active_fg,
                "tab-active-bg" => &mut theme.tab_active_bg,
                "tab-inactive-fg" => &mut theme.tab_inactive_fg,
                "tab-inactive-bg" => &mut theme.tab_inactive_bg,
                "hut-tab-active-fg" => &mut theme.hut_tab_active_fg,
                "hut-tab-active-bg" => &mut theme.hut_tab_active_bg,
                "hut-tab-inactive-fg" => &mut theme.hut_tab_inactive_fg,
                "hut-tab-inactive-bg" => &mut theme.hut_tab_inactive_bg,
                "dock-fg" => &mut theme.dock_fg,
                "dock-bg" => &mut theme.dock_bg,
                "tile-border" => &mut theme.tile_border,
                _ => {
                    tracing::warn!("unknown theme key {name:?} in config at {source}");
                    continue;
                }
            };
            match parse_hex_color(&value) {
                Some(rgb) => *slot = rgb,
                None => tracing::warn!("invalid color {value:?} for {name:?} in config at {source}"),
            }
        }
    }
}

/// Parse a `"#rrggbb"` (or bare `"rrggbb"`) hex color into an [`Rgb`].
/// `None` for anything else — no alpha, no 3-digit shorthand, no named
/// colors; keeps the config format small and unambiguous rather than
/// guessing at what a partial/shorthand value meant.
fn parse_hex_color(s: &str) -> Option<Rgb> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b])
}

#[derive(serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    theme: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colors_with_and_without_a_hash() {
        assert_eq!(parse_hex_color("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_hex_color("ff8000"), Some([255, 128, 0]));
    }

    #[test]
    fn rejects_malformed_colors() {
        assert_eq!(parse_hex_color("#fff"), None);
        assert_eq!(parse_hex_color("#gggggg"), None);
        assert_eq!(parse_hex_color(""), None);
    }

    #[test]
    fn unknown_theme_key_is_ignored_without_touching_known_ones() {
        let mut theme = Theme::default();
        let default_tab_bg = theme.tab_active_bg;
        Theme::apply_toml_overrides(
            &mut theme,
            "[theme]\nnot-a-real-key = \"#000000\"\ntab-active-bg = \"#112233\"\n",
            "test",
        );
        assert_eq!(theme.tab_active_bg, [0x11, 0x22, 0x33]);
        assert_ne!(theme.tab_active_bg, default_tab_bg);
    }

    #[test]
    fn invalid_color_value_leaves_the_default_in_place() {
        let mut theme = Theme::default();
        let default_tab_bg = theme.tab_active_bg;
        Theme::apply_toml_overrides(&mut theme, "[theme]\ntab-active-bg = \"not-a-color\"\n", "test");
        assert_eq!(theme.tab_active_bg, default_tab_bg);
    }
}
