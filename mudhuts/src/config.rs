//! Shared across every `~/.config/mudhuts/config.toml` reader
//! (`keybindings.rs`'s `[keybindings]` section, `theme.rs`'s `[theme]`
//! section, `chrome_config.rs`'s `[chrome]` section, `perf_config.rs`'s
//! `[performance]` section) — the path-resolution rule, plus the shared
//! read-the-file-or-fall-back-to-default step, so they can't drift apart
//! on either.

use std::path::PathBuf;

pub(crate) fn config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("mudhuts/config.toml"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/mudhuts/config.toml"))
}

/// [`read_config_file`]'s result — a named struct rather than a same-
/// typed `(String, String)` tuple specifically so a call site can't
/// silently compile with `contents`/`source` swapped (caught in review
/// on an earlier tuple-returning version of this function).
pub(crate) struct ConfigFileContents {
    pub(crate) contents: String,
    pub(crate) source: String,
}

/// The config file's raw contents plus its path (as a display string,
/// for error messages) — empty `contents` (never `None`/an error out of
/// this function) if it doesn't exist, can't be located at all, or fails
/// to read for any other reason (logged in that last case). Every
/// `*Config::apply_toml_overrides` below already treats an empty/missing
/// `[section]` as a graceful no-op on its own (`toml::from_str("")` is
/// valid, empty TOML), so returning a real, always-usable
/// `ConfigFileContents` here — rather than an `Option` every one of the
/// four call sites had to unwrap just to special-case something that
/// already degraded correctly by itself — collapses that unwrap away
/// entirely (caught in review on an `Option`-returning earlier version).
/// Deliberately doesn't parse the TOML itself: each config type's own
/// `[section]` shape is different enough, and each has exactly one
/// caller, that sharing the parse step too would mean a generic/trait
/// abstraction for no real reuse — this only shares the byte-identical
/// read step (independently duplicated four times over before review
/// first caught that).
///
/// Deliberately reads fresh every call, no process-wide caching — an
/// earlier version cached the result behind a `static OnceLock` (all
/// four `*Config::load()`s below run back to back at startup, so without
/// it they'd each independently open+read+UTF-8-validate the same small
/// file), but review caught that baking a "read once, ever, for the
/// whole process" assumption into this low-level shared helper — rather
/// than at the one call site (`State::new`) that actually owns that
/// invariant — would silently return stale data forever the moment
/// anything violates it (a future config-reload feature, more than one
/// `State` in a process, or a test that sets `HOME`/`XDG_CONFIG_HOME` and
/// calls a `load()` more than once with different env). `State::new`
/// reads this once itself and passes the result to all four `load()`s
/// instead, which gets the same "don't read 4 times" benefit without the
/// hazard.
pub(crate) fn read_config_file() -> ConfigFileContents {
    let Some(path) = config_path() else {
        return ConfigFileContents { contents: String::new(), source: String::new() };
    };
    let source = path.display().to_string();
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            tracing::warn!("failed to read config at {source}: {err}");
            String::new()
        }
    };
    ConfigFileContents { contents, source }
}
