//! `~/.config/mudhuts/config.toml` reading for `settings.rs`'s
//! `[appearance]` section — deliberately a self-contained duplicate of
//! `mudhuts`'s own `config.rs` (`config_path`/`read_config_file`), not a
//! shared dependency: this binary is a separate, independently-launched
//! process from the compositor (same "structured the same way as
//! `mudhuts-authority-helper`" independence this crate's own top-level
//! doc already calls out), and `mudhuts` itself is a binary crate with no
//! library target to depend on. The logic is small and stable enough
//! (path resolution, read-or-empty) that duplicating it costs far less
//! than introducing a new shared library crate for one function.

use std::path::PathBuf;

/// `None` only if neither `XDG_CONFIG_HOME` nor `HOME` is set — logged at
/// `warn`, unlike `mudhuts`'s own equivalent (silent there): this binary
/// is D-Bus-session-activated, not launched directly by a user's own
/// shell/session the way the compositor is, so a stripped-down activation
/// environment actually missing `HOME` is a real, higher-likelihood case
/// here (caught in review) — without a log line, `[appearance]`
/// falling back to the default would otherwise look identical to the
/// user simply never having set one.
fn config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("mudhuts/config.toml"));
    }
    let Ok(home) = std::env::var("HOME") else {
        tracing::warn!(
            "mudhuts-portal: neither XDG_CONFIG_HOME nor HOME is set, can't locate \
             config.toml — using default settings"
        );
        return None;
    };
    Some(PathBuf::from(home).join(".config/mudhuts/config.toml"))
}

/// [`read_config_file`]'s result — a named struct rather than a same-
/// typed `(String, String)` tuple, matching `mudhuts::config`'s own
/// `ConfigFileContents` (introduced there specifically to rule out a
/// call site silently compiling with `contents`/`source` swapped).
pub(crate) struct ConfigFileContents {
    pub(crate) contents: String,
    pub(crate) source: String,
}

/// The config file's raw contents plus its path (as a display string, for
/// error messages) — empty `contents` if it doesn't exist, can't be
/// located at all, or fails to read for any other reason (logged in that
/// last case). Mirrors `mudhuts::config::read_config_file`'s exact
/// contract, including "empty, not an error" for the missing-file case —
/// see its own doc comment for why.
pub(crate) fn read_config_file() -> ConfigFileContents {
    let Some(path) = config_path() else {
        return ConfigFileContents { contents: String::new(), source: String::new() };
    };
    let source = path.display().to_string();
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            tracing::warn!("mudhuts-portal: failed to read config at {source}: {err}");
            String::new()
        }
    };
    ConfigFileContents { contents, source }
}
