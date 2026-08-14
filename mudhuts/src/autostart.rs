//! XDG Desktop Autostart (`.desktop` entries in `$XDG_CONFIG_HOME/autostart`,
//! defaulting to `~/.config/autostart`, and `/etc/xdg/autostart`) — run once
//! at startup, after the Wayland socket and event loop already exist (real
//! clients, no different from anything else launched into this session).
//!
//! Every autostarted app gets its own freshly-spawned background
//! ConsoleHut, running the entry's own `Exec=` line directly as that
//! Hut's PTY child (`GraphStack::spawn_and_insert_with_command`) in place
//! of the usual interactive shell — never focused, so nothing steals
//! focus from whatever the user's actually looking at when the session
//! starts. `MUDHUTS_HUT_ID` (set automatically by
//! `ConsoleHut::spawn_with_command` for every Hut, autostart or not) is
//! what lets `ownership.rs`'s `find_owning_hut` resolve any window the
//! app opens back to its own Hut, with no separate tagging needed here: a
//! new toplevel's `should_show_now` check in `handlers/xdg_shell.rs` only
//! ever auto-switches when its owning Hut is already the *focused* one,
//! which an autostart Hut deliberately never is, so every autostarted
//! window just joins its own Hut's tab strip in the background.
//!
//! **One Hut per entry, not one shared Hut for all of them** — an earlier
//! version of this module spawned a single shared background Hut and
//! launched every app as an untracked `std::process::Command` tagged onto
//! it via env var alone, with its own terminal never actually running
//! anything. Two real problems with that, not just style: (1) every
//! autostarted app's window shared one single point of failure — that one
//! Hut's own `touched` field (see `ConsoleHut::touched`'s doc comment)
//! stayed `false` forever, since nobody's meant to type into it, which
//! made it look exactly like any other disposable, safe-to-discard
//! console to `GraphStack::next()`'s "replace an untouched entry rather
//! than grow the stack" rule — the moment the user's own `next()`
//! navigation happened to land on it and they pressed `next()` again
//! (indistinguishable, from the user's side, from "just open another
//! blank console"), it was silently destroyed, taking down *every*
//! autostarted window at once; (2) a failed/misbehaving autostart app had
//! no real terminal output to inspect at all. Splitting into one
//! dedicated, immediately-`touched` Hut per entry (see
//! `GraphStack::spawn_and_insert_with_command`'s own doc comment) fixes
//! both: no Hut looks disposable, and each Hut's terminal shows that
//! app's real stdout/stderr if it ever needs debugging. **Known
//! interaction**: `State::handle_term_event`'s "last ConsoleHut closed,
//! exiting" logout check already counts every ConsoleHut anywhere in the
//! tree, not just visible ones — with N autostart entries running, that
//! count is never `1` while they're still alive, so Ctrl+D-ing out of
//! your own last visible shell falls back to whatever top-level entry is
//! now current (possibly one of these) rather than logging out — a
//! pre-existing characteristic of that check, not new here, just more
//! pronounced with more background Huts around.
//!
//! Scope: `Type=Application` entries only (`Link`/`Directory` skipped —
//! nothing to launch). `Hidden=true`, `Type` not `Application`, or an
//! unresolvable `TryExec` skip an entry. `OnlyShowIn`/`NotShowIn` are
//! honored against the empty set of desktop names mudhuts declares itself
//! as (it doesn't advertise any `XDG_CURRENT_DESKTOP` name) — `OnlyShowIn`
//! present means skip (nothing in it can match), `NotShowIn` present alone
//! doesn't (nothing in it is excluded either) — the standard XDG behavior
//! for a desktop environment with no recognized name. `Exec=`'s field
//! codes (`%f`/`%F`/`%u`/`%U`/`%d`/`%D`/`%n`/`%N`/`%i`/`%c`/`%k`/`%v`/`%m`)
//! are stripped rather than filled in — autostart never has a file/URL
//! argument to fill them with, and `%c`/`%k`/`%i` specifically (translated
//! name/this file's own path/icon flag) aren't worth the extra complexity
//! for how rarely real autostart entries use them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::graph_stack::GraphStack;

/// Scans both autostart directories and spawns every entry that resolves,
/// each into its own new background Hut (see this module's doc comment) —
/// a no-op if nothing resolves. Logs and skips whatever fails along the
/// way; never treated as fatal to the rest of startup.
pub fn run(stack: &mut GraphStack) {
    let mut seen = HashSet::new();
    let mut argvs = Vec::new();

    // User overrides win over system defaults — same precedence XDG's own
    // spec gives `$XDG_CONFIG_HOME` over `/etc/xdg`; `seen` (keyed by
    // file *name*, not full path) is what makes a user override actually
    // suppress the system-wide entry of the same name rather than both
    // running.
    for dir in [user_autostart_dir(), Some(PathBuf::from("/etc/xdg/autostart"))]
        .into_iter()
        .flatten()
    {
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(name) = path.file_name().map(std::ffi::OsStr::to_owned) else {
                continue;
            };
            if !seen.insert(name) {
                continue;
            }
            match resolve_entry(&path) {
                Ok(Some(argv)) => argvs.push(argv),
                Ok(None) => {}
                Err(err) => tracing::warn!("failed to read autostart entry {}: {err}", path.display()),
            }
        }
    }

    for argv in argvs {
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        match stack.spawn_and_insert_with_command(program.clone(), args.to_vec()) {
            Ok(_id) => tracing::info!("autostarted {program:?}"),
            Err(err) => tracing::warn!("failed to autostart {program:?}: {err}"),
        }
    }
}

fn user_autostart_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("autostart"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/autostart"))
}

/// `Ok(None)` for a `.desktop` file that parses fine but shouldn't be
/// launched (`Hidden`, wrong `Type`, `OnlyShowIn`, unresolvable
/// `TryExec`); `Err` only for a genuine read/parse problem.
fn resolve_entry(path: &Path) -> Result<Option<Vec<String>>, String> {
    let contents = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let fields = parse_desktop_entry(&contents);

    if fields.get("Type").map(String::as_str) != Some("Application") {
        return Ok(None);
    }
    if fields.get("Hidden").map(String::as_str) == Some("true") {
        return Ok(None);
    }
    // See this module's doc comment: mudhuts doesn't declare a desktop
    // name, so it can never appear in `OnlyShowIn`, and never appears in
    // `NotShowIn` either.
    if fields.contains_key("OnlyShowIn") {
        return Ok(None);
    }
    if let Some(try_exec) = fields.get("TryExec")
        && which(try_exec).is_none()
    {
        return Ok(None);
    }
    let Some(exec) = fields.get("Exec") else {
        return Ok(None);
    };
    Ok(parse_exec(exec))
}

/// Minimal `.desktop`/INI parser: just enough for autostart's own needs
/// — only the `[Desktop Entry]` section's `Key=Value` lines, `#` comments
/// and blank lines skipped, no localized `Key[locale]=` variants (falls
/// back to the base key's own value, matching every reader that doesn't
/// care about localization).
fn parse_desktop_entry(contents: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let mut in_desktop_entry = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_desktop_entry = section == "Desktop Entry";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    fields
}

/// Search `$PATH` for an executable, like the `which` command — `program`
/// containing a `/` is checked directly instead, matching how a shell
/// would treat it. Just an existence check, not a permissions/execute-bit
/// check — good enough to catch "this isn't installed," the only case
/// `TryExec` exists to guard against.
fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(program);
        candidate.is_file().then_some(candidate)
    })
}

/// Split an `Exec=` value into argv, honoring the format's basic
/// double-quote/backslash-escape rules, and dropping field codes
/// (`%f`/`%F`/`%u`/`%U`/`%d`/`%D`/`%n`/`%N`/`%i`/`%c`/`%k`/`%v`/`%m`,
/// `%%` → literal `%`) rather than filling them in — see this module's
/// doc comment. `None` for an empty/all-field-codes result.
fn parse_exec(exec: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' if in_quotes => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '%' => match chars.next() {
                Some('%') => current.push('%'),
                Some('f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'i' | 'c' | 'k' | 'v' | 'm') => {}
                Some(other) => {
                    current.push('%');
                    current.push(other);
                }
                None => current.push('%'),
            },
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    (!words.is_empty()).then_some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_desktop_entry_section_ignoring_others() {
        let contents = "[Desktop Entry]\nType=Application\nExec=foo --bar\n\n[Desktop Action New]\nExec=foo --new\n";
        let fields = parse_desktop_entry(contents);
        assert_eq!(fields.get("Type"), Some(&"Application".to_string()));
        assert_eq!(fields.get("Exec"), Some(&"foo --bar".to_string()));
    }

    #[test]
    fn parse_exec_strips_field_codes_and_splits_words() {
        assert_eq!(
            parse_exec("firefox --new-window %u"),
            Some(vec!["firefox".to_string(), "--new-window".to_string()])
        );
    }

    #[test]
    fn parse_exec_honors_double_quoted_words_with_spaces() {
        assert_eq!(
            parse_exec(r#"sh -c "echo hello world""#),
            Some(vec!["sh".to_string(), "-c".to_string(), "echo hello world".to_string()])
        );
    }

    #[test]
    fn parse_exec_unescapes_percent_percent() {
        assert_eq!(parse_exec("prog --format=%%d"), Some(vec!["prog".to_string(), "--format=%d".to_string()]));
    }

    #[test]
    fn parse_exec_of_only_field_codes_is_none() {
        assert_eq!(parse_exec("%u %f"), None);
    }

    #[test]
    fn which_finds_a_program_on_a_synthetic_path() {
        let dir = std::env::temp_dir().join(format!("mudhuts-autostart-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("mudhuts-test-bin");
        std::fs::write(&bin, "").unwrap();

        // SAFETY: this test doesn't spawn threads that also read `PATH`.
        let original = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &dir);
        }
        let found = which("mudhuts-test-bin");
        unsafe {
            match &original {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(found, Some(bin));
        assert_eq!(which("definitely-not-a-real-binary-xyz"), None);
    }
}
