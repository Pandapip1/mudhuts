//! Applies `SCHED_FIFO` real-time scheduling to the calling (main)
//! thread when `perf_config.rs`'s `[performance]` opt-in is on — see
//! `PerfConfig::sched_fifo`'s doc comment for why that defaults off
//! and what it takes for this to actually succeed.

/// A `SCHED_FIFO` thread that ever spins instead of blocking holds the
/// CPU indefinitely — nothing of lower priority (which is everything
/// else on the system, including most kernel housekeeping) can preempt
/// it, so a bug that would just be "high CPU" under the default
/// scheduler becomes "the machine is unresponsive until a hard reset"
/// under `SCHED_FIFO`. `RLIMIT_RTTIME` is the kernel's own answer to
/// exactly this: a cap, in microseconds, on how much CPU time a
/// real-time-scheduled thread may burn *without making a blocking
/// syscall* — the kernel resets the counter to zero on every blocking
/// call the thread makes (`sched_setrlimit(2)`), so this never fires
/// during real work as long as the event loop keeps returning to its
/// own blocking `epoll_wait` between units of work (which is the
/// entire point of this module's sibling requirement — see `main.rs`'s
/// `run`: nothing in mudhuts' own code busy-loops, every event source
/// is either a real blocking wait or backed by one). If that invariant
/// is ever broken by a future bug, this is what turns "wedge the whole
/// system, needs a physical hard reset" into "this one process gets
/// `SIGXCPU`'d and dies" — a real, recoverable failure instead of an
/// unrecoverable one. Two full seconds is deliberately generous: not a
/// tight frame-time budget (this measures *total accumulated*
/// unblocked CPU time, not per-frame time — a burst of several fast
/// renders in a row without an intervening block would never come
/// close), just a backstop against genuine runaway spinning.
const RT_TIME_LIMIT: libc::rlim_t = 2_000_000;

/// Requests `SCHED_FIFO` at `priority` for the calling thread, then (only
/// on success) caps its `RLIMIT_RTTIME` at [`RT_TIME_LIMIT`] as a safety
/// net — see that constant's own doc comment. Meant to run early in
/// `main.rs`'s `run`, before any backend/session setup, so the
/// compositor's whole main-thread event/render loop runs under both
/// from the start.
///
/// Never fatal: a normal, unprivileged process has no business calling
/// `sched_setscheduler` successfully (it needs `CAP_SYS_NICE` or a
/// raised `RLIMIT_RTPRIO`, neither of which mudhuts grants itself —
/// see `PerfConfig::sched_fifo`'s doc comment), so failure here is the
/// expected outcome unless the launching session/service granted one
/// of those. Logged at `warn` (so a user who *did* mean to grant it
/// can tell it didn't take) and otherwise treated as a no-op — per
/// this codebase's "no panics" convention, requesting a scheduling
/// policy is exactly the kind of best-effort operation that should
/// degrade gracefully rather than take the compositor down with it.
pub(crate) fn apply(priority: u8) {
    let min = unsafe { libc::sched_get_priority_min(libc::SCHED_FIFO) };
    let max = unsafe { libc::sched_get_priority_max(libc::SCHED_FIFO) };
    let clamped = if min >= 0 && max >= 0 {
        i32::from(priority).clamp(min, max)
    } else {
        i32::from(priority)
    };

    let param = libc::sched_param { sched_priority: clamped };
    // SAFETY: `param` is a valid, live `sched_param` for the duration
    // of this call; `pid = 0` targets the calling thread, per
    // `sched_setscheduler(2)`.
    let result = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        tracing::warn!(
            "failed to enable SCHED_FIFO at priority {clamped}, continuing at the default \
             scheduling policy: {err} (needs CAP_SYS_NICE or a raised RLIMIT_RTPRIO from the \
             launching session/service)"
        );
        return;
    }
    tracing::info!("SCHED_FIFO enabled at priority {clamped}");

    // Lowering RLIMIT_RTTIME never needs any special privilege (only
    // *raising* it past the current hard limit does), so this is safe
    // to attempt unconditionally right after a successful
    // sched_setscheduler above.
    let limit = libc::rlimit { rlim_cur: RT_TIME_LIMIT, rlim_max: RT_TIME_LIMIT };
    // SAFETY: `limit` is a valid, live `rlimit` for the duration of
    // this call.
    let result = unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &limit) };
    if result == 0 {
        tracing::info!("RLIMIT_RTTIME capped at {}s as a runaway-spin safety net", RT_TIME_LIMIT / 1_000_000);
    } else {
        let err = std::io::Error::last_os_error();
        tracing::warn!(
            "failed to set RLIMIT_RTTIME after enabling SCHED_FIFO, running without the \
             runaway-spin safety net: {err}"
        );
    }
}
