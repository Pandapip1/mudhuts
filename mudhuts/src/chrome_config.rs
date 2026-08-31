//! Chrome (on-screen UI, as opposed to client window content) settings
//! overridable via `~/.config/mudhuts/config.toml`'s `[chrome]` section —
//! same file/mechanism `perf_config.rs`'s `[performance]` section,
//! `theme.rs`'s `[theme]` section, and `keybindings.rs`'s `[keybindings]`
//! section use (see [`crate::config::config_path`]).

use crate::config::ConfigFileContents;

pub struct ChromeConfig {
    /// Whether the combined Hut-level + Main-Window tab strip (see
    /// `village_chrome.rs`'s/`chrome.rs`'s own module docs) auto-hides
    /// until the pointer touches the top edge of its output, instead of
    /// always being drawn — see `input.rs`'s `update_tab_strip_reveal`
    /// for the reveal/hide logic. On by default: unlike
    /// `PerfConfig::sched_fifo` (a real, genuinely risky system-level
    /// change — see its own doc comment — so off by default), this is a
    /// purely software overlay change with real, deliberate hysteresis
    /// of its own (stays shown for as long as the pointer is anywhere
    /// within the strip's own rect, not just the few-pixel-wide edge band
    /// that revealed it — `input.rs`'s `EDGE_REVEAL_PX`) — explicitly
    /// requested on by default rather than defaulting
    /// to this codebase's usual "disruptive features default off"
    /// posture, which is really about hardware disruption specifically.
    pub auto_hide_tab_strip: bool,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self { auto_hide_tab_strip: true }
    }
}

impl ChromeConfig {
    /// Load the default chrome config, then apply overrides from
    /// `~/.config/mudhuts/config.toml`'s `[chrome]` section if present —
    /// same "any problem is logged and skipped, never fatal" convention
    /// as `PerfConfig::load`/`Theme::load`/`Keymap::load`. `config_file`
    /// is read once by the caller (`State::new`) and shared across all
    /// four `*Config::load()`s — see `crate::config::read_config_file`'s
    /// own doc comment for why this doesn't read it itself.
    pub(crate) fn load(config_file: &ConfigFileContents) -> Self {
        let mut config = Self::default();
        Self::apply_toml_overrides(&mut config, &config_file.contents, &config_file.source);
        config
    }

    fn apply_toml_overrides(config: &mut ChromeConfig, contents: &str, source: &str) {
        let file: ConfigFile = match toml::from_str(contents) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!("failed to parse config at {source}: {err}");
                return;
            }
        };
        if let Some(value) = file.chrome.and_then(|c| c.auto_hide_tab_strip) {
            config.auto_hide_tab_strip = value;
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    chrome: Option<ChromeToml>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct ChromeToml {
    #[serde(default)]
    auto_hide_tab_strip: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_enabled_with_no_config_section() {
        let mut config = ChromeConfig::default();
        ChromeConfig::apply_toml_overrides(&mut config, "", "test");
        assert!(config.auto_hide_tab_strip);
    }

    #[test]
    fn disables_via_the_chrome_section() {
        let mut config = ChromeConfig::default();
        ChromeConfig::apply_toml_overrides(&mut config, "[chrome]\nauto-hide-tab-strip = false\n", "test");
        assert!(!config.auto_hide_tab_strip);
    }

    #[test]
    fn malformed_toml_leaves_the_default_in_place() {
        let mut config = ChromeConfig::default();
        ChromeConfig::apply_toml_overrides(&mut config, "not valid toml [[[", "test");
        assert!(config.auto_hide_tab_strip);
    }
}
