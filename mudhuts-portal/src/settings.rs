//! `org.freedesktop.impl.portal.Settings` — the cheapest, highest
//! compatibility-payoff portal: no window, no Wayland dependency at all,
//! just a `Read`/`ReadAll` D-Bus call returning a small dict of
//! namespace -> key -> value settings. Nearly every modern
//! GTK4/libadwaita/Qt6 app queries `org.freedesktop.appearance`'s
//! `color-scheme` at startup, so that's the one key this backend actually
//! answers for in v1 (plus `contrast`/`reduced-motion`, which cost
//! nothing extra since they're the same `u32` shape).
//!
//! `color-scheme` is read from `~/.config/mudhuts/config.toml`'s
//! `[appearance]` section (`color-scheme = "dark"/"light"/"no-preference"`)
//! — the same file/mechanism every other mudhuts setting (`theme.rs`'s
//! `[theme]`, `chrome_config.rs`'s `[chrome]`, ...) uses, rather than the
//! `MUDHUTS_PORTAL_COLOR_SCHEME` env var an earlier version used (nobody
//! would think to look for a compositor-wide preference in a process env
//! var, and it couldn't survive a portal restart without being re-set
//! externally every time).
//!
//! Deliberately not built: `accent-color` (a `(ddd)` tuple — more
//! conversion ceremony for a key far fewer apps check), and any kind of
//! live-settings-sync (`SettingChanged` is never emitted — this table is
//! fixed for the process's lifetime, matching every other mudhuts config
//! value's "read once at startup" convention). Both are easy follow-ups,
//! not required for a v1 that just needs to answer honestly and
//! consistently.

use std::collections::HashMap;

use zbus::interface;
use zbus::zvariant::OwnedValue;

use crate::config::ConfigFileContents;

/// The one namespace this backend knows about.
const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";

/// The `u32` enum values the interface spec defines for `color-scheme`.
mod color_scheme {
    pub const NO_PREFERENCE: u32 = 0;
    pub const PREFER_DARK: u32 = 1;
    pub const PREFER_LIGHT: u32 = 2;
}

pub struct SettingsBackend {
    /// Keys within [`APPEARANCE_NAMESPACE`], built once at construction
    /// and never mutated — see this module's doc on why live sync is out
    /// of scope for v1.
    appearance: HashMap<String, OwnedValue>,
}

impl SettingsBackend {
    /// Load `color-scheme` from `config_file`'s `[appearance]` section —
    /// same "any problem is logged and skipped, never fatal" convention
    /// as every `mudhuts::*Config::load()`. Defaults to "prefer dark",
    /// matching mudhuts' own dark-leaning built-in terminal/UI look, if
    /// the section/key is missing or its value isn't one of
    /// `"dark"`/`"light"`/`"no-preference"`.
    pub fn load(config_file: &ConfigFileContents) -> Self {
        // An earlier version read this from `MUDHUTS_PORTAL_COLOR_SCHEME`
        // instead — warn rather than silently ignore it, in case anyone
        // still has it set in a shell profile or systemd user unit from
        // before this switch to `config.toml` (caught in review).
        if std::env::var_os("MUDHUTS_PORTAL_COLOR_SCHEME").is_some() {
            tracing::warn!(
                "mudhuts-portal: MUDHUTS_PORTAL_COLOR_SCHEME is set but no longer used — set \
                 color-scheme in ~/.config/mudhuts/config.toml's [appearance] section instead"
            );
        }
        let color_scheme = parse_color_scheme(&config_file.contents, &config_file.source);

        let mut appearance = HashMap::new();
        appearance.insert("color-scheme".to_string(), OwnedValue::from(color_scheme));
        // No high-contrast/reduced-motion support to actually report on
        // yet, so both are reported at their "off" default rather than
        // guessed at.
        appearance.insert("contrast".to_string(), OwnedValue::from(0u32));
        appearance.insert("reduced-motion".to_string(), OwnedValue::from(0u32));

        Self { appearance }
    }
}

#[derive(serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    appearance: Option<AppearanceToml>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct AppearanceToml {
    #[serde(default)]
    color_scheme: Option<String>,
}

/// The `[appearance]` section's own parse step, pulled out of
/// `SettingsBackend::load` as a pure function over the raw file contents
/// so it's directly testable with literal TOML strings, matching
/// `mudhuts::*Config`'s own `apply_toml_overrides` test convention.
fn parse_color_scheme(contents: &str, source: &str) -> u32 {
    let file: ConfigFile = match toml::from_str(contents) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!("mudhuts-portal: failed to parse config at {source}: {err}");
            return color_scheme::PREFER_DARK;
        }
    };
    match file.appearance.and_then(|a| a.color_scheme).as_deref() {
        Some("dark") | None => color_scheme::PREFER_DARK,
        Some("light") => color_scheme::PREFER_LIGHT,
        Some("no-preference") => color_scheme::NO_PREFERENCE,
        Some(other) => {
            tracing::warn!(
                "mudhuts-portal: unknown color-scheme {other:?} in config at {source}, \
                 expected \"dark\", \"light\", or \"no-preference\" — using \"dark\""
            );
            color_scheme::PREFER_DARK
        }
    }
}

/// Namespace glob matching for `ReadAll`, per the interface's own doc:
/// only a single trailing `*` is supported (e.g. `"org.freedesktop.*"`),
/// not general globbing.
fn namespace_matches(pattern: &str, namespace: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => namespace.starts_with(prefix),
        None => pattern == namespace,
    }
}

#[interface(name = "org.freedesktop.impl.portal.Settings")]
impl SettingsBackend {
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }

    async fn read_all(&self, namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedValue>> {
        let matches = namespaces.is_empty()
            || namespaces
                .iter()
                .any(|pattern| namespace_matches(pattern, APPEARANCE_NAMESPACE));

        let mut out = HashMap::new();
        if matches {
            out.insert(APPEARANCE_NAMESPACE.to_string(), self.appearance.clone());
        }
        out
    }

    async fn read(&self, namespace: String, key: String) -> zbus::fdo::Result<OwnedValue> {
        if namespace != APPEARANCE_NAMESPACE {
            return Err(zbus::fdo::Error::Failed(format!("unknown namespace {namespace:?}")));
        }
        self.appearance
            .get(&key)
            .cloned()
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("unknown key {key:?} in namespace {namespace:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_dark_with_no_config_section() {
        assert_eq!(parse_color_scheme("", "test"), color_scheme::PREFER_DARK);
    }

    #[test]
    fn reads_dark_light_and_no_preference() {
        assert_eq!(
            parse_color_scheme("[appearance]\ncolor-scheme = \"dark\"\n", "test"),
            color_scheme::PREFER_DARK
        );
        assert_eq!(
            parse_color_scheme("[appearance]\ncolor-scheme = \"light\"\n", "test"),
            color_scheme::PREFER_LIGHT
        );
        assert_eq!(
            parse_color_scheme("[appearance]\ncolor-scheme = \"no-preference\"\n", "test"),
            color_scheme::NO_PREFERENCE
        );
    }

    #[test]
    fn unknown_value_falls_back_to_dark() {
        assert_eq!(
            parse_color_scheme("[appearance]\ncolor-scheme = \"midnight\"\n", "test"),
            color_scheme::PREFER_DARK
        );
    }

    #[test]
    fn malformed_toml_falls_back_to_dark() {
        assert_eq!(parse_color_scheme("not valid toml [[[", "test"), color_scheme::PREFER_DARK);
    }
}
