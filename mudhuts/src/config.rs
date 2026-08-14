//! Shared across every `~/.config/mudhuts/config.toml` reader
//! (`keybindings.rs`'s `[keybindings]` section, `theme.rs`'s `[theme]`
//! section) — just the one path-resolution rule, so they can't drift
//! apart on which file they're each reading.

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
