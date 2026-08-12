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

fn default_bindings() -> HashMap<Chord, Action> {
    let mut bindings = HashMap::new();
    for (_, chord_spec, action) in DEFAULTS {
        if let Some(chord) = parse_chord(chord_spec) {
            bindings.insert(chord, *action);
        }
    }
    bindings
}

/// Apply one `action-name = "chord spec"` config entry to `bindings`,
/// replacing whatever chord that action was previously bound to. Returns a
/// description of what went wrong (unknown action name, unparseable
/// chord) without touching `bindings`, so the caller can log and skip.
fn apply_override(
    bindings: &mut HashMap<Chord, Action>,
    name: &str,
    chord_spec: &str,
) -> Result<(), String> {
    let action =
        action_by_name(name).ok_or_else(|| format!("unknown keybinding action {name:?}"))?;
    let chord = parse_chord(chord_spec)
        .ok_or_else(|| format!("invalid chord {chord_spec:?} for {name:?}"))?;
    bindings.retain(|_, a| *a != action);
    bindings.insert(chord, action);
    Ok(())
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
        let mut bindings = default_bindings();

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

        Self::apply_toml_overrides(&mut bindings, &contents, &path.display().to_string());
        Self { bindings }
    }

    fn apply_toml_overrides(bindings: &mut HashMap<Chord, Action>, contents: &str, source: &str) {
        let config: ConfigFile = match toml::from_str(contents) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("failed to parse config at {source}: {err}");
                return;
            }
        };
        for (name, chord_spec) in config.keybindings {
            if let Err(err) = apply_override(bindings, &name, &chord_spec) {
                tracing::warn!("{err} in config at {source}");
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool, logo: bool) -> ModifiersState {
        ModifiersState {
            ctrl,
            alt,
            shift,
            logo,
            ..Default::default()
        }
    }

    #[test]
    fn parses_single_modifier_chord() {
        let chord = parse_chord("Ctrl+grave").expect("should parse");
        assert!(chord.ctrl);
        assert!(!chord.alt && !chord.shift && !chord.logo);
        assert_eq!(
            chord.keysym,
            xkb::keysym_from_name("grave", xkb::KEYSYM_CASE_INSENSITIVE).raw()
        );
    }

    #[test]
    fn modifier_names_are_case_insensitive() {
        assert_eq!(parse_chord("Ctrl+grave"), parse_chord("ctrl+grave"));
        assert_eq!(parse_chord("ALT+SHIFT+Tab"), parse_chord("alt+shift+tab"));
    }

    #[test]
    fn parses_multiple_modifiers() {
        let chord = parse_chord("Alt+Shift+Tab").expect("should parse");
        assert!(chord.alt && chord.shift);
        assert!(!chord.ctrl && !chord.logo);
    }

    #[test]
    fn meta_super_logo_win_are_synonyms() {
        let a = parse_chord("Meta+T").expect("meta");
        let b = parse_chord("Super+T").expect("super");
        let c = parse_chord("Logo+T").expect("logo");
        let d = parse_chord("Win+T").expect("win");
        assert_eq!((a, b, c, d), (a, a, a, a));
    }

    #[test]
    fn rejects_unknown_modifier() {
        assert_eq!(parse_chord("Hyper+T"), None);
    }

    #[test]
    fn rejects_empty_and_unknown_key_name() {
        assert_eq!(parse_chord(""), None);
        assert_eq!(parse_chord("Ctrl+ThisIsNotAKey"), None);
    }

    #[test]
    fn all_defaults_parse_and_round_trip_to_their_action() {
        let bindings = default_bindings();
        assert_eq!(
            bindings.len(),
            DEFAULTS.len(),
            "every default chord should parse"
        );
        for (name, chord_spec, action) in DEFAULTS {
            let chord =
                parse_chord(chord_spec).unwrap_or_else(|| panic!("default {name:?} should parse"));
            assert_eq!(
                bindings.get(&chord),
                Some(action),
                "default for {name:?} maps to wrong action"
            );
        }
    }

    #[test]
    fn rebinding_an_action_replaces_its_old_chord() {
        let mut bindings = default_bindings();
        let old_chord = parse_chord("Ctrl+grave").unwrap();
        assert_eq!(bindings.get(&old_chord), Some(&Action::ToggleTerminal));

        apply_override(&mut bindings, "toggle-terminal", "Alt+grave").expect("valid rebind");

        // Old chord no longer triggers the action...
        assert_eq!(bindings.get(&old_chord), None);
        // ...the new one does.
        let new_chord = parse_chord("Alt+grave").unwrap();
        assert_eq!(bindings.get(&new_chord), Some(&Action::ToggleTerminal));
    }

    #[test]
    fn unknown_action_name_is_rejected_without_mutating_bindings() {
        let mut bindings = default_bindings();
        let before = bindings.clone();
        let err = apply_override(&mut bindings, "not-a-real-action", "Ctrl+X").unwrap_err();
        assert!(err.contains("unknown"));
        assert_eq!(bindings, before);
    }

    #[test]
    fn invalid_chord_is_rejected_without_mutating_bindings() {
        let mut bindings = default_bindings();
        let before = bindings.clone();
        let err = apply_override(&mut bindings, "toggle-terminal", "NotAModifier+Q").unwrap_err();
        assert!(err.contains("invalid chord"));
        assert_eq!(bindings, before);
    }

    #[test]
    fn full_toml_config_rebinds_an_action() {
        let mut bindings = default_bindings();
        let toml = r#"
            [keybindings]
            toggle-terminal = "Alt+grave"
            close-focused = "Ctrl+Shift+W"
        "#;
        Keymap::apply_toml_overrides(&mut bindings, toml, "<test>");

        let alt_grave = parse_chord("Alt+grave").unwrap();
        assert_eq!(bindings.get(&alt_grave), Some(&Action::ToggleTerminal));

        let ctrl_shift_w = parse_chord("Ctrl+Shift+W").unwrap();
        assert_eq!(bindings.get(&ctrl_shift_w), Some(&Action::CloseFocused));

        // Untouched actions keep their default chord.
        let alt_tab = parse_chord("Alt+Tab").unwrap();
        assert_eq!(bindings.get(&alt_tab), Some(&Action::StackNext));
    }

    #[test]
    fn malformed_toml_leaves_defaults_untouched() {
        let mut bindings = default_bindings();
        let before = bindings.clone();
        Keymap::apply_toml_overrides(&mut bindings, "this is not valid toml [[[", "<test>");
        assert_eq!(bindings, before);
    }

    #[test]
    fn chord_matches_checks_every_modifier_and_the_keysym() {
        let chord = parse_chord("Ctrl+Shift+grave").unwrap();
        let grave = xkb::keysym_from_name("grave", xkb::KEYSYM_CASE_INSENSITIVE);
        let other_key = xkb::keysym_from_name("a", xkb::KEYSYM_CASE_INSENSITIVE);

        assert!(chord.matches(&mods(true, false, true, false), grave));
        // Missing a required modifier.
        assert!(!chord.matches(&mods(true, false, false, false), grave));
        // Extra modifier not in the chord.
        assert!(!chord.matches(&mods(true, true, true, false), grave));
        // Right modifiers, wrong key.
        assert!(!chord.matches(&mods(true, false, true, false), other_key));
    }

    #[test]
    fn lookup_finds_the_bound_action_and_nothing_else() {
        let keymap = Keymap {
            bindings: default_bindings(),
        };
        let tab = xkb::keysym_from_name("Tab", xkb::KEYSYM_CASE_INSENSITIVE);
        let grave = xkb::keysym_from_name("grave", xkb::KEYSYM_CASE_INSENSITIVE);

        assert_eq!(
            keymap.lookup(&mods(false, true, false, false), tab),
            Some(Action::StackNext)
        );
        assert_eq!(
            keymap.lookup(&mods(true, false, false, false), grave),
            Some(Action::ToggleTerminal)
        );
        // Plain Tab (no modifiers) isn't bound to anything.
        assert_eq!(keymap.lookup(&mods(false, false, false, false), tab), None);
    }
}
