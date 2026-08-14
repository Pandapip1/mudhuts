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
    /// Toggle the focused ConsoleHut's terminal vs. its last-focused Main Window.
    /// Stubbed until Main Windows land (Phase 4).
    ToggleTerminal,
    /// Move forward/backward through The Stack (MRU). Stubbed until
    /// multi-ConsoleHut support lands (Phase 3).
    StackNext,
    StackPrev,
    /// Innermost-first tab cycle (ConsoleHut's Main Windows, else ancestor
    /// Tab/Tile-Hut). Stubbed until Main Windows/Huts land
    /// (Phase 4/6).
    TabNext,
    TabPrev,
    /// Wrap the focused Hut with a sibling into a new Tab/Tile-Hut.
    /// Stubbed until the Hut management layer lands (Phase 6).
    WrapTab,
    WrapTile,
    /// Close the focused client window. Implemented now — doesn't depend
    /// on later phases.
    CloseFocused,
    /// Copy the terminal's current text selection (if any) to the regular
    /// clipboard (`wl_data_device`/`ext_data_control`), not primary — see
    /// `input.rs`'s `PointerButton` handler for the separate, automatic
    /// "selecting = copy to primary" path that needs no keybinding at all.
    /// Bound to `Ctrl+Shift+C` rather than plain `Ctrl+C`, which is already
    /// SIGINT inside the terminal.
    CopySelection,
    /// Brightness/volume media keys — see `input.rs`'s `handle_action` for
    /// why these shell out to `brightnessctl`/`wpctl` rather than talking
    /// to backlight sysfs or a mixer directly: matches what most minimal
    /// Wayland compositors actually do (sway/hyprland's own documented
    /// default configs), avoids a new D-Bus/PipeWire-client dependency in
    /// the main compositor binary just for this, and degrades to "logged,
    /// does nothing" rather than a crash if the tool isn't installed.
    BrightnessUp,
    BrightnessDown,
    VolumeUp,
    VolumeDown,
    VolumeMute,
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
    ("copy-selection", "Ctrl+Shift+C", Action::CopySelection),
    // Bare keysym, no modifier — a real dedicated media key already sends
    // exactly this XF86 keysym on its own; `parse_chord` handles a
    // modifier-less spec with no special-casing needed (see its own doc
    // comment).
    ("brightness-up", "XF86MonBrightnessUp", Action::BrightnessUp),
    ("brightness-down", "XF86MonBrightnessDown", Action::BrightnessDown),
    ("volume-up", "XF86AudioRaiseVolume", Action::VolumeUp),
    ("volume-down", "XF86AudioLowerVolume", Action::VolumeDown),
    ("volume-mute", "XF86AudioMute", Action::VolumeMute),
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

    /// Like [`Self::matches`], but any modifier bit set in `hold` is
    /// exempted from the usual exact-equality check (whatever this chord
    /// itself specifies for that bit is ignored) and instead required to
    /// actually be held right now — see [`Keymap::stack_hold`] for why:
    /// `stack-next`/`stack-prev` need to keep matching regardless of a
    /// separately-configured "hold" modifier's state in the chord
    /// definition itself, while still requiring it to be physically down.
    pub fn matches_gated(
        &self,
        hold: &ModMask,
        mods: &ModifiersState,
        base_keysym: xkb::Keysym,
    ) -> bool {
        (hold.ctrl || self.ctrl == mods.ctrl)
            && (hold.alt || self.alt == mods.alt)
            && (hold.shift || self.shift == mods.shift)
            && (hold.logo || self.logo == mods.logo)
            && self.keysym == base_keysym.raw()
            && hold.satisfied_by(mods)
    }
}

/// A pure modifier mask — no base key, unlike [`Chord`]. Used for
/// `stack-hold` (see [`Keymap::stack_hold`]), which gates other chords
/// rather than triggering an action on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ModMask {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
}

impl ModMask {
    pub fn is_empty(&self) -> bool {
        !(self.ctrl || self.alt || self.shift || self.logo)
    }

    /// Whether every bit set in this mask is currently held in `mods`.
    /// Vacuously true for an empty mask.
    pub fn satisfied_by(&self, mods: &ModifiersState) -> bool {
        (!self.ctrl || mods.ctrl)
            && (!self.alt || mods.alt)
            && (!self.shift || mods.shift)
            && (!self.logo || mods.logo)
    }
}

/// Apply one modifier name (case-insensitive) to `mask`, shared between
/// [`parse_chord`] and [`parse_mod_mask`]. Returns `false` for anything
/// unrecognized, leaving `mask` untouched by that name.
fn apply_modifier_name(mask: &mut ModMask, name: &str) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => mask.ctrl = true,
        "alt" => mask.alt = true,
        "shift" => mask.shift = true,
        "meta" | "super" | "logo" | "win" => mask.logo = true,
        _ => return false,
    }
    true
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

    let mut mods = ModMask::default();
    for m in mod_names {
        if !apply_modifier_name(&mut mods, m) {
            return None;
        }
    }

    let keysym = xkb::keysym_from_name(key_name, xkb::KEYSYM_CASE_INSENSITIVE);
    if keysym.raw() == xkb::keysyms::KEY_NoSymbol {
        return None;
    }
    Some(Chord {
        ctrl: mods.ctrl,
        alt: mods.alt,
        shift: mods.shift,
        logo: mods.logo,
        keysym: keysym.raw(),
    })
}

/// Parse a modifier-only spec like `"Alt"` or `"Ctrl+Alt"` (no base key)
/// into a [`ModMask`] — used for `stack-hold`. `None` for an empty spec or
/// an unrecognized modifier name.
fn parse_mod_mask(spec: &str) -> Option<ModMask> {
    let mut mask = ModMask::default();
    let mut saw_any = false;
    for part in spec.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        if !apply_modifier_name(&mut mask, part) {
            return None;
        }
        saw_any = true;
    }
    saw_any.then_some(mask)
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
    /// Gates `StackNext`/`StackPrev` matching (see [`Chord::matches_gated`])
    /// and, once one of them fires, is what the preview popup watches for
    /// release to commit — see the Phase 3.5 plan notes. Empty (the
    /// default, since there's no default chord for it) means no gating and
    /// no popup: `stack-next`/`stack-prev` just match normally and commit
    /// immediately, same as before this existed.
    stack_hold: ModMask,
}

impl Keymap {
    /// Load the default keymap, then apply overrides from
    /// `~/.config/mudhuts/config.toml` if present. Any problem reading or
    /// parsing the config (missing file, bad TOML, unknown action name,
    /// unparseable chord) is logged and skipped rather than treated as
    /// fatal — the compositor always ends up with at least the defaults.
    pub fn load() -> Self {
        let mut bindings = default_bindings();
        let mut stack_hold = ModMask::default();

        let Some(path) = config_path() else {
            return Self {
                bindings,
                stack_hold,
            };
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    bindings,
                    stack_hold,
                };
            }
            Err(err) => {
                tracing::warn!("failed to read config at {}: {err}", path.display());
                return Self {
                    bindings,
                    stack_hold,
                };
            }
        };

        Self::apply_toml_overrides(
            &mut bindings,
            &mut stack_hold,
            &contents,
            &path.display().to_string(),
        );
        Self {
            bindings,
            stack_hold,
        }
    }

    /// `stack-hold` is handled here rather than through [`apply_override`]
    /// — it's a pure modifier mask with no base key and no `Action`, so it
    /// doesn't fit the `action-name = "chord"` shape everything else uses.
    fn apply_toml_overrides(
        bindings: &mut HashMap<Chord, Action>,
        stack_hold: &mut ModMask,
        contents: &str,
        source: &str,
    ) {
        let config: ConfigFile = match toml::from_str(contents) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("failed to parse config at {source}: {err}");
                return;
            }
        };
        for (name, value) in config.keybindings {
            if name == "stack-hold" {
                match parse_mod_mask(&value) {
                    Some(mask) => *stack_hold = mask,
                    None => tracing::warn!(
                        "invalid modifier mask {value:?} for stack-hold in config at {source}"
                    ),
                }
                continue;
            }
            if let Err(err) = apply_override(bindings, &name, &value) {
                tracing::warn!("{err} in config at {source}");
            }
        }
    }

    pub fn lookup(&self, mods: &ModifiersState, base_keysym: xkb::Keysym) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(chord, action)| match action {
                Action::StackNext | Action::StackPrev => {
                    chord.matches_gated(&self.stack_hold, mods, base_keysym)
                }
                _ => chord.matches(mods, base_keysym),
            })
            .map(|(_, a)| *a)
    }

    /// See the `stack_hold` field doc.
    pub fn stack_hold(&self) -> ModMask {
        self.stack_hold
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
        let mut stack_hold = ModMask::default();
        Keymap::apply_toml_overrides(&mut bindings, &mut stack_hold, toml, "<test>");

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
        let mut stack_hold = ModMask::default();
        Keymap::apply_toml_overrides(
            &mut bindings,
            &mut stack_hold,
            "this is not valid toml [[[",
            "<test>",
        );
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
            stack_hold: ModMask::default(),
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

    #[test]
    fn parses_mod_mask() {
        let mask = parse_mod_mask("Ctrl+Alt").expect("should parse");
        assert!(mask.ctrl && mask.alt);
        assert!(!mask.shift && !mask.logo);
    }

    #[test]
    fn mod_mask_rejects_empty_and_unknown_names() {
        assert_eq!(parse_mod_mask(""), None);
        assert_eq!(parse_mod_mask("Hyper"), None);
    }

    #[test]
    fn empty_mod_mask_is_satisfied_regardless_of_held_modifiers() {
        let empty = ModMask::default();
        assert!(empty.satisfied_by(&mods(false, false, false, false)));
        assert!(empty.satisfied_by(&mods(true, true, true, true)));
    }

    #[test]
    fn mod_mask_requires_its_own_bits_held_and_ignores_others() {
        let hold = parse_mod_mask("Alt").unwrap();
        assert!(!hold.satisfied_by(&mods(false, false, false, false)));
        assert!(hold.satisfied_by(&mods(false, true, false, false)));
        // Shift being held too doesn't matter — only Alt is required.
        assert!(hold.satisfied_by(&mods(false, true, true, false)));
    }

    #[test]
    fn matches_gated_ignores_held_bits_covered_by_hold_but_still_requires_hold() {
        // Chord itself doesn't require Alt (alt: false) — mirrors the
        // user's real setup: stack-next = "Shift+bracketright",
        // stack-hold = "Alt".
        let chord = parse_chord("Shift+bracketright").unwrap();
        let hold = parse_mod_mask("Alt").unwrap();
        let bracketright = xkb::keysym_from_name("bracketright", xkb::KEYSYM_CASE_INSENSITIVE);

        // Alt+Shift+] : matches, even though the chord's own `alt` field
        // is false — Alt is covered by `hold`, so it's exempted from the
        // equality check and instead required to be held, which it is.
        assert!(chord.matches_gated(&hold, &mods(false, true, true, false), bracketright));
        // Shift+] alone (no Alt held): hold not satisfied, no match.
        assert!(!chord.matches_gated(&hold, &mods(false, false, true, false), bracketright));
        // Alt+] (no Shift): the chord's own shift requirement still
        // applies normally since shift isn't part of `hold`.
        assert!(!chord.matches_gated(&hold, &mods(false, true, false, false), bracketright));
    }

    #[test]
    fn matches_gated_with_empty_hold_behaves_like_a_plain_match() {
        let chord = parse_chord("Alt+Tab").unwrap();
        let empty_hold = ModMask::default();
        let tab = xkb::keysym_from_name("Tab", xkb::KEYSYM_CASE_INSENSITIVE);

        assert_eq!(
            chord.matches_gated(&empty_hold, &mods(false, true, false, false), tab),
            chord.matches(&mods(false, true, false, false), tab)
        );
        assert_eq!(
            chord.matches_gated(&empty_hold, &mods(false, false, false, false), tab),
            chord.matches(&mods(false, false, false, false), tab)
        );
    }

    #[test]
    fn stack_hold_config_key_sets_the_mask_not_a_binding() {
        let mut bindings = default_bindings();
        let mut stack_hold = ModMask::default();
        let toml = r#"
            [keybindings]
            stack-hold = "Alt"
            stack-next = "Shift+bracketright"
        "#;
        Keymap::apply_toml_overrides(&mut bindings, &mut stack_hold, toml, "<test>");

        assert_eq!(stack_hold, parse_mod_mask("Alt").unwrap());
        let chord = parse_chord("Shift+bracketright").unwrap();
        assert_eq!(bindings.get(&chord), Some(&Action::StackNext));
    }

    #[test]
    fn invalid_stack_hold_is_rejected_without_mutating_it() {
        let mut bindings = default_bindings();
        let mut stack_hold = ModMask::default();
        let toml = r#"
            [keybindings]
            stack-hold = "NotAModifier"
        "#;
        Keymap::apply_toml_overrides(&mut bindings, &mut stack_hold, toml, "<test>");
        assert_eq!(stack_hold, ModMask::default());
    }
}
