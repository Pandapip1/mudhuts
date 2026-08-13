//! `org.freedesktop.impl.portal.Settings` — the cheapest, highest
//! compatibility-payoff portal: no window, no Wayland dependency at all,
//! just a `Read`/`ReadAll` D-Bus call returning a small dict of
//! namespace -> key -> value settings. Nearly every modern
//! GTK4/libadwaita/Qt6 app queries `org.freedesktop.appearance`'s
//! `color-scheme` at startup, so that's the one key this backend actually
//! answers for in v1 (plus `contrast`/`reduced-motion`, which cost
//! nothing extra since they're the same `u32` shape).
//!
//! Deliberately not built: `accent-color` (a `(ddd)` tuple — more
//! conversion ceremony for a key far fewer apps check), and any kind of
//! live-settings-sync (`SettingChanged` is never emitted — this table is
//! fixed for the process's lifetime). Both are easy follow-ups, not
//! required for a v1 that just needs to answer honestly and consistently.

use std::collections::HashMap;

use zbus::interface;
use zbus::zvariant::OwnedValue;

/// The one namespace this backend knows about.
const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";

/// The `u32` enum values the interface spec defines for `color-scheme`.
mod color_scheme {
    pub const PREFER_DARK: u32 = 1;
}

pub struct SettingsBackend {
    /// Keys within [`APPEARANCE_NAMESPACE`], built once at construction
    /// and never mutated — see this module's doc on why live sync is out
    /// of scope for v1.
    appearance: HashMap<String, OwnedValue>,
}

impl SettingsBackend {
    /// `color-scheme` defaults to "prefer dark", matching mudhuts' own
    /// dark-leaning built-in terminal/UI look, but can be overridden
    /// without a rebuild via `MUDHUTS_PORTAL_COLOR_SCHEME` (0 = no
    /// preference, 1 = prefer dark, 2 = prefer light — the same values
    /// the portal spec itself uses). A single env var felt like the right
    /// amount of "config-driven" for one knob in v1; a real config file
    /// is an easy follow-up if more keys show up.
    pub fn new() -> Self {
        let color_scheme = std::env::var("MUDHUTS_PORTAL_COLOR_SCHEME")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v <= 2)
            .unwrap_or(color_scheme::PREFER_DARK);

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

impl Default for SettingsBackend {
    fn default() -> Self {
        Self::new()
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
