//! Default ConsoleHut assignment for new client toplevels — see the plan's
//! Phase 4 notes. No protocol needed for this default case (that's
//! reserved for Floating Window/Alert role assignment, Phase 5).
//!
//! Two resolution paths, tried in order:
//!
//! 1. `MUDHUTS_HUT_ID`, an env var every ConsoleHut's shell has set in its own
//!    environment (see `ConsoleHut::spawn`) — inherited by every descendant
//!    process regardless of `fork()`/`exec()`, and so, unlike walking
//!    `PPid` chains, immune to a descendant being reparented away from
//!    the shell entirely. That reparenting isn't a hypothetical: apps
//!    that daemonize their real process on launch (VS Code/Codium's `code`
//!    CLI is a well-known example — it backgrounds the actual Electron
//!    process and exits immediately so the invoking shell doesn't block)
//!    end up parented to init well before their window actually appears,
//!    which breaks a pure ancestry walk outright — not "too slow", but
//!    genuinely disconnected from the shell by the time it matters. This
//!    also explains reports of ownership being wrong specifically for
//!    slow-to-launch apps: the substantial delay is exactly the window
//!    during which the real process gets reparented before its toplevel
//!    ever shows up.
//! 2. Walking the connecting client's process ancestry back to a known
//!    ConsoleHut's shell PID — kept as a fallback for whatever `MUDHUTS_HUT_ID`
//!    doesn't cover (e.g. a launcher that explicitly clears its child's
//!    environment).

use std::fs;

use crate::stack::Stack;

/// Read `/proc/<pid>/environ`'s null-separated `KEY=VALUE` entries
/// looking for `MUDHUTS_HUT_ID`. `None` if the process is gone,
/// unreadable, or doesn't have it set — never panics on a malformed file.
fn env_hut_id(pid: u32) -> Option<u64> {
    let contents = fs::read(format!("/proc/{pid}/environ")).ok()?;
    contents.split(|&b| b == 0).find_map(|entry| {
        std::str::from_utf8(entry)
            .ok()?
            .strip_prefix("MUDHUTS_HUT_ID=")?
            .parse()
            .ok()
    })
}

/// This only ever climbs a normal process tree (client -> ... -> some
/// ConsoleHut's shell), so a real hit is always close — bounded so a
/// pathological or adversarial `/proc` entry can't hang the walk.
const MAX_ANCESTRY_HOPS: usize = 32;

/// Read `/proc/<pid>/status`'s `PPid:` line. `None` if the process is
/// gone, unreadable, or the file's shape is unexpected — never panics on
/// a malformed status file.
fn parent_pid(pid: u32) -> Option<u32> {
    let contents = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|rest| rest.trim().parse().ok())
}

/// Resolve which ConsoleHut owns `client_pid` — first via its own `MUDHUTS_HUT_ID`
/// environment variable (see the module doc for why this is tried
/// first), then by walking its process ancestry looking for a ConsoleHut whose
/// shell is it or one of its ancestors. `None` if neither finds a match —
/// callers should fall back to the currently focused ConsoleHut.
pub fn find_owning_hut(client_pid: u32, stack: &Stack) -> Option<u64> {
    if let Some(id) = env_hut_id(client_pid)
        && stack.all_huts().any(|hut| hut.id == id)
    {
        return Some(id);
    }

    let mut pid = client_pid;
    for _ in 0..MAX_ANCESTRY_HOPS {
        if let Some(hut) = stack.all_huts().find(|h| h.shell_pid() == pid) {
            return Some(hut.id);
        }
        pid = parent_pid(pid)?;
        if pid == 0 {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use smithay::reexports::calloop::EventLoop;
    use smithay::reexports::calloop::LoopHandle;

    use super::*;
    use crate::State;
    use crate::console_hut::ConsoleHut;

    fn loop_handle() -> LoopHandle<'static, State> {
        let event_loop: EventLoop<'static, State> = EventLoop::try_new().unwrap();
        Box::leak(Box::new(event_loop)).handle()
    }

    fn new_stack() -> Stack {
        let (hut, events) = ConsoleHut::spawn(std::iter::empty(), 1.0).unwrap();
        let (ping, _source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        Stack::new(
            hut,
            events,
            loop_handle(),
            Vec::new(),
            crate::redraw::RedrawHandle::new(ping),
        )
        .unwrap()
    }

    #[test]
    fn parent_pid_of_the_current_process_is_readable() {
        // Every real process but pid 1 has a parent; the test harness
        // running this is no exception.
        assert!(parent_pid(std::process::id()).is_some());
    }

    #[test]
    fn parent_pid_of_a_bogus_pid_is_none() {
        assert_eq!(parent_pid(u32::MAX), None);
    }

    #[test]
    fn finds_the_hut_whose_own_shell_pid_matches_directly() {
        let stack = new_stack();
        let shell_pid = stack.focused().shell_pid();
        assert_eq!(find_owning_hut(shell_pid, &stack), Some(stack.focused().id));
    }

    #[test]
    fn unrelated_pid_finds_no_owning_hut() {
        let stack = new_stack();
        // Deliberately not `std::process::id()`: this test binary's own
        // environment isn't guaranteed clean of `MUDHUTS_HUT_ID` — if the
        // whole test suite happens to be run from inside a live mudhuts
        // session (its own daily-driver use), the test process inherits
        // that ConsoleHut's real id, which `env_hut_id` would then find first,
        // making this assertion depend on ambient state outside the
        // test's control. A short-lived child with that variable
        // explicitly cleared sidesteps it entirely.
        let mut child = std::process::Command::new("true")
            .env_remove("MUDHUTS_HUT_ID")
            .spawn()
            .expect("failed to spawn `true`");
        let pid = child.id();
        assert_eq!(find_owning_hut(pid, &stack), None);
        let _ = child.wait();
    }
}
