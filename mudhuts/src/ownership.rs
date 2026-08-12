//! Default Hut assignment for new client toplevels: walk the connecting
//! client's process ancestry back to a known Hut's shell PID — see the
//! plan's Phase 4 notes. No protocol needed for this default case (that's
//! reserved for Sub-Window/Alert role assignment, Phase 5).

use std::fs;

use crate::stack::HutStack;

/// This only ever climbs a normal process tree (client -> ... -> some
/// Hut's shell), so a real hit is always close — bounded so a
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

/// Walk `client_pid`'s ancestry looking for a Hut whose shell is it or
/// one of its ancestors. `None` if no Hut matches within the bound —
/// callers should fall back to the currently focused Hut.
pub fn find_owning_hut(client_pid: u32, stack: &HutStack) -> Option<u64> {
    let mut pid = client_pid;
    for _ in 0..MAX_ANCESTRY_HOPS {
        if let Some(hut) = stack.huts().find(|h| h.shell_pid() == pid) {
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
    use crate::hut::Hut;

    fn loop_handle() -> LoopHandle<'static, State> {
        let event_loop: EventLoop<'static, State> = EventLoop::try_new().unwrap();
        Box::leak(Box::new(event_loop)).handle()
    }

    fn new_stack() -> HutStack {
        let (hut, events) = Hut::spawn(std::iter::empty()).unwrap();
        HutStack::new(hut, events, loop_handle(), Vec::new()).unwrap()
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
        // This test process is the *parent* of the spawned shell, not a
        // descendant of it — walking upward from its own pid climbs
        // toward init/the test harness, never toward the shell.
        assert_eq!(find_owning_hut(std::process::id(), &stack), None);
    }
}
