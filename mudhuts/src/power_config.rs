//! Power-button handling settings overridable via
//! `~/.config/mudhuts/config.toml`'s `[power]` section — same file/
//! mechanism `perf_config.rs`'s `[performance]` section,
//! `chrome_config.rs`'s `[chrome]` section, and `theme.rs`'s `[theme]`
//! section use (see [`crate::config::config_path`]).

use crate::config::ConfigFileContents;

pub struct PowerConfig {
    /// Whether mudhuts takes systemd-logind's `handle-power-key`
    /// inhibitor lock under a real `--tty` session (see
    /// `logind::spawn_power_key_inhibitor`) so `mudhuts_power_button_v1`
    /// clients (or mudhuts' own logout fallback) are the only thing that
    /// ever acts on the power button, instead of logind's own default
    /// (an immediate poweroff). **On by default** — unlike
    /// `PerfConfig::sched_fifo`, this isn't a hardware-disruptive
    /// modeset-class feature (nothing here is visible under normal
    /// operation, and the lock releases the instant the process exits,
    /// cleanly or not) — but it does have one real, narrow cost worth an
    /// opt-out for: if mudhuts ever *hangs* rather than exiting (a
    /// deadlock, not a crash), the lock stays held for as long as the
    /// hung process is alive, meaning the power button does nothing at
    /// either level until it's killed some other way — logind's own
    /// immediate-poweroff fallback, which existed before this feature and
    /// would otherwise still work, is unavailable for that window too.
    /// Set `inhibit-power-key = false` to keep that fallback available at
    /// the cost of never being able to run `mudhuts_power_button_v1`
    /// clients or the graceful-logout fallback reliably (logind's own
    /// action always races them).
    pub inhibit_power_key: bool,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self { inhibit_power_key: true }
    }
}

impl PowerConfig {
    /// Load the default power config, then apply overrides from
    /// `~/.config/mudhuts/config.toml`'s `[power]` section if present —
    /// same "any problem is logged and skipped, never fatal" convention
    /// as `PerfConfig::load`/`ChromeConfig::load`. `config_file` is read
    /// once by the caller (`State::new`) and shared across all
    /// `*Config::load()`s — see `crate::config::read_config_file`'s own
    /// doc comment for why this doesn't read it itself.
    pub(crate) fn load(config_file: &ConfigFileContents) -> Self {
        let mut config = Self::default();
        Self::apply_toml_overrides(&mut config, &config_file.contents, &config_file.source);
        config
    }

    fn apply_toml_overrides(config: &mut PowerConfig, contents: &str, source: &str) {
        let file: ConfigFile = match toml::from_str(contents) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!("failed to parse config at {source}: {err}");
                return;
            }
        };
        if let Some(value) = file.power.and_then(|p| p.inhibit_power_key) {
            config.inhibit_power_key = value;
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    power: Option<PowerToml>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct PowerToml {
    #[serde(default)]
    inhibit_power_key: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_enabled_with_no_config_section() {
        let mut config = PowerConfig::default();
        PowerConfig::apply_toml_overrides(&mut config, "", "test");
        assert!(config.inhibit_power_key);
    }

    #[test]
    fn disables_via_the_power_section() {
        let mut config = PowerConfig::default();
        PowerConfig::apply_toml_overrides(&mut config, "[power]\ninhibit-power-key = false\n", "test");
        assert!(!config.inhibit_power_key);
    }

    #[test]
    fn malformed_toml_leaves_the_default_in_place() {
        let mut config = PowerConfig::default();
        PowerConfig::apply_toml_overrides(&mut config, "not valid toml [[[", "test");
        assert!(config.inhibit_power_key);
    }
}
