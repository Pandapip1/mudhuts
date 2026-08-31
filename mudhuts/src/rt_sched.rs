//! Applies `SCHED_FIFO` real-time scheduling to the calling (main)
//! thread when `perf_config.rs`'s `[performance]` opt-in is on — see
//! `PerfConfig::sched_fifo`'s doc comment for why that defaults off
//! and what it takes for this to actually succeed.

/// Requests `SCHED_FIFO` at `priority` for the calling thread. Meant
/// to run early in `main.rs`'s `run`, before any backend/session setup,
/// so the compositor's whole main-thread event/render loop runs under
/// it from the start.
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
    if result == 0 {
        tracing::info!("SCHED_FIFO enabled at priority {clamped}");
    } else {
        let err = std::io::Error::last_os_error();
        tracing::warn!(
            "failed to enable SCHED_FIFO at priority {clamped}, continuing at the default \
             scheduling policy: {err} (needs CAP_SYS_NICE or a raised RLIMIT_RTPRIO from the \
             launching session/service)"
        );
    }
}
