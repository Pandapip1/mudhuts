//! Configurable keybindings: parses chords like `"Ctrl+grave"` into
//! (modifiers, base keysym) pairs and maps them to [`Action`]s, with
//! defaults overridable via `~/.config/mudhuts/config.toml`.
//!
//! Matching uses the *unshifted* base keysym (via
//! [`KeysymHandle::raw_latin_sym_or_raw_current_sym`]), not the
//! shift-modified one used for terminal text input — so a binding is
//! "the T key, plus these modifiers" regardless of case, the same way most
//! window managers treat keybindings.

use std::collections::HashMap;
use std::path::PathBuf;

use smithay::input::keyboard::{ModifiersState, xkb};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Toggle the focused Hut's terminal vs. its last-focused Main Window.
    /// Stubbed until Main Windows land (Phase 4).
    ToggleTerminal,
    /// Move forward/backward through The Stack (MRU). Stubbed until
    /// multi-Hut support lands (Phase 3).
    StackNext,
    StackPrev,
    /// Innermost-first tab cycle (Hut's Main Windows, else ancestor
    /// Tab/Tile-Village). Stubbed until Main Windows/Villages land
    /// (Phase 4/6).
    TabNext,
    TabPrev,
    /// Wrap the focused Village with a sibling into a new Tab/Tile-Village.
    /// Stubbed until the Village management layer lands (Phase 6).
    WrapTab,
    WrapTile,
    /// Close the focused client window. Implemented now — doesn't depend
    /// on later phases.
    CloseFocused,
}

/// `(config key, default chord, action)` — the single source of truth for
/// both the default keymap and the config file's action-name parsing.
const DEFAULTS: &[(&str, &str, Action)] = &[
    ("toggle-terminal", "Ctrl+grave", Action::ToggleTerminal),
    ("stack-next", "Alt+Tab", Action::StackNext),
    ("stack-prev", "Alt+Shift+Tab", Action::StackPrev),
    ("tab-next", "Meta+Right", Action::TabNext),
    ("tab-prev", "Meta+Left", Action::TabPrev),
    ("wrap-tab", "Meta+Shift+T", Action::WrapTab),
    ("wrap-tile", "Meta+Shift+V", Action::WrapTile),
    ("close-focused", "Meta+Shift+Q", Action::CloseFocused),
];

fn action_by_name(name: &str) -> Option<Action> {
    DEFAULTS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, a)| *a)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
    keysym: u32,
}

impl Chord {
    pub fn matches(&self, mods: &ModifiersState, base_keysym: xkb::Keysym) -> bool {
        self.ctrl == mods.ctrl
            && self.alt == mods.alt
            && self.shift == mods.shift
            && self.logo == mods.logo
            && self.keysym == base_keysym.raw()
    }
}

/// Parse a chord spec like `"Ctrl+Shift+T"` into a [`Chord`]. Returns
/// `None` for anything that doesn't parse (unknown modifier, missing/
/// unrecognized key name) rather than panicking, so a bad config value
/// can be reported and skipped instead of crashing the compositor.
fn parse_chord(spec: &str) -> Option<Chord> {
    let parts: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let (key_name, mod_names) = parts.split_last()?;

    let mut chord = Chord {
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
        keysym: 0,
    };
    for m in mod_names {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => chord.ctrl = true,
            "alt" => chord.alt = true,
            "shift" => chord.shift = true,
            "meta" | "super" | "logo" | "win" => chord.logo = true,
            _ => return None,
        }
    }

    let keysym = xkb::keysym_from_name(key_name, xkb::KEYSYM_CASE_INSENSITIVE);
    if keysym.raw() == xkb::keysyms::KEY_NoSymbol {
        return None;
    }
    chord.keysym = keysym.raw();
    Some(chord)
}

pub struct Keymap {
    bindings: HashMap<Chord, Action>,
}

impl Keymap {
    /// Load the default keymap, then apply overrides from
    /// `~/.config/mudhuts/config.toml` if present. Any problem reading or
    /// parsing the config (missing file, bad TOML, unknown action name,
    /// unparseable chord) is logged and skipped rather than treated as
    /// fatal — the compositor always ends up with at least the defaults.
    pub fn load() -> Self {
        let mut bindings: HashMap<Chord, Action> = HashMap::new();
        for (_, chord_spec, action) in DEFAULTS {
            if let Some(chord) = parse_chord(chord_spec) {
                bindings.insert(chord, *action);
            }
        }

        let Some(path) = config_path() else {
            return Self { bindings };
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self { bindings },
            Err(err) => {
                tracing::warn!("failed to read config at {}: {err}", path.display());
                return Self { bindings };
            }
        };

        let config: ConfigFile = match toml::from_str(&contents) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("failed to parse config at {}: {err}", path.display());
                return Self { bindings };
            }
        };

        for (name, chord_spec) in config.keybindings {
            let Some(action) = action_by_name(&name) else {
                tracing::warn!("unknown keybinding action {name:?} in config");
                continue;
            };
            let Some(chord) = parse_chord(&chord_spec) else {
                tracing::warn!("invalid chord {chord_spec:?} for {name:?} in config");
                continue;
            };
            bindings.retain(|_, a| *a != action);
            bindings.insert(chord, action);
        }

        Self { bindings }
    }

    pub fn lookup(&self, mods: &ModifiersState, base_keysym: xkb::Keysym) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(chord, _)| chord.matches(mods, base_keysym))
            .map(|(_, a)| *a)
    }
}

#[derive(serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    keybindings: HashMap<String, String>,
}

fn config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("mudhuts/config.toml"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/mudhuts/config.toml"))
}
