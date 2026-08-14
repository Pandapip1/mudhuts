//! Display-output settings overridable via
//! `~/.config/mudhuts/config.toml`'s `[display]` section — the same
//! file/mechanism `theme.rs`'s `[theme]` section and `keybindings.rs`'s
//! `[keybindings]` section use (see [`crate::config::config_path`]).

use crate::config::config_path;

#[derive(Default)]
pub struct DisplayConfig {
    /// See `udev_backend.rs`'s module doc, "Adaptive refresh rate for
    /// non-VRR connectors" section. Off by default (`bool`'s own
    /// `Default`): even with real, deliberately generous hysteresis, a
    /// DRM mode switch is a visibly disruptive thing to do to someone's
    /// screen (a real hardware-level blank while the display controller
    /// retimes/retrains, not something software can hide) — not
    /// something to turn on for anyone who didn't specifically ask for
    /// it.
    pub adaptive_refresh_rate: bool,
}

impl DisplayConfig {
    /// Load the default display config, then apply overrides from
    /// `~/.config/mudhuts/config.toml`'s `[display]` section if present —
    /// same "any problem is logged and skipped, never fatal" convention
    /// as `Theme::load`/`Keymap::load`.
    pub fn load() -> Self {
        let mut config = Self::default();

        let Some(path) = config_path() else {
            return config;
        };
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return config,
            Err(err) => {
                tracing::warn!("failed to read config at {}: {err}", path.display());
                return config;
            }
        };

        Self::apply_toml_overrides(&mut config, &contents, &path.display().to_string());
        config
    }

    fn apply_toml_overrides(config: &mut DisplayConfig, contents: &str, source: &str) {
        let file: ConfigFile = match toml::from_str(contents) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!("failed to parse config at {source}: {err}");
                return;
            }
        };
        if let Some(value) = file.display.and_then(|d| d.adaptive_refresh_rate) {
            config.adaptive_refresh_rate = value;
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    display: Option<DisplayToml>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct DisplayToml {
    #[serde(default)]
    adaptive_refresh_rate: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_disabled_with_no_config_section() {
        let mut config = DisplayConfig::default();
        DisplayConfig::apply_toml_overrides(&mut config, "", "test");
        assert!(!config.adaptive_refresh_rate);
    }

    #[test]
    fn enables_via_the_display_section() {
        let mut config = DisplayConfig::default();
        DisplayConfig::apply_toml_overrides(
            &mut config,
            "[display]\nadaptive-refresh-rate = true\n",
            "test",
        );
        assert!(config.adaptive_refresh_rate);
    }

    #[test]
    fn malformed_toml_leaves_the_default_in_place() {
        let mut config = DisplayConfig::default();
        DisplayConfig::apply_toml_overrides(&mut config, "not valid toml [[[", "test");
        assert!(!config.adaptive_refresh_rate);
    }
}
