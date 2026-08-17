//! Automates the "launch mudhuts nested inside a live host Wayland
//! compositor and confirm it starts cleanly" smoke test — this session's
//! own earlier manual check (build fresh, run under the real
//! `$WAYLAND_DISPLAY` this same Claude Code session was itself running
//! inside, confirm clean init + several seconds of stable runtime, no
//! panics), now repeatable instead of hand-run once.
//!
//! Lives in `tests/` (a real Cargo integration test, not a `#[cfg(test)]`
//! module inside `src/`) specifically to get `CARGO_BIN_EXE_mudhuts` —
//! Cargo only sets that for integration tests, and this one genuinely
//! needs to spawn the real compiled binary as a subprocess (unlike every
//! other test in this crate, which exercises in-process functions/types
//! directly). Works even though this crate has no `[lib]` target: an
//! integration test can still target a binary's own executable without
//! needing a library API to import.
//!
//! `#[ignore]`d — needs a real host Wayland compositor to nest inside (a
//! live `$WAYLAND_DISPLAY`), the same reasoning `udev_backend.rs`'s
//! `vkms_*` tests use for needing a real vkms card: not portable to a
//! bare sandboxed CI runner with nothing to nest inside at all. Run
//! explicitly: `cargo test -- --ignored winit_nested`.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// How long to let mudhuts run nested before killing it — long enough to
/// get past real EGL/GLES context creation and the first few frames.
/// This session's own earlier manual check used exactly this window
/// (`timeout 8 ...`) and confirmed clean, stable output the whole way
/// through — matched here rather than shortened, since a startup panic
/// specifically *within* that window (the slowest, most failure-prone
/// part of nested init) is exactly the class of bug this test exists to
/// catch; an earlier version of this constant was `5s`, silently
/// narrower than the empirically-validated window the comment right next
/// to it already described (caught in review).
const RUN_DURATION: Duration = Duration::from_secs(8);

/// Kills (and reaps) the wrapped child on drop, unconditionally —
/// including on an early return via a panicking `assert!`/`.expect()`
/// anywhere between spawning it and the explicit `drop(guard)` below. A
/// bare `std::process::Child` does *not* do this itself (dropping one
/// leaves the process running — this crate has no `panic = "abort"`
/// profile to fall back on either), so without this wrapper, a genuine
/// panic in this test's own polling logic would orphan a real nested
/// mudhuts process — indefinitely, and invisibly, on whoever's desktop
/// this happened to run against (caught in review).
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Same idea as [`KillOnDrop`], for the capture log file instead of the
/// child process — an earlier version relied solely on the explicit
/// `remove_file` call at the end of the test function, which (like the
/// child process before `KillOnDrop` existed) never runs at all if a
/// panic/`.expect()` fires anywhere earlier — spawn failure, a
/// `try_wait` error — leaking `mudhuts-nested-smoke-<pid>.log` in the
/// OS temp directory on every such failure (caught in review).
struct RemoveOnDrop(std::path::PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
#[ignore = "needs a live host Wayland compositor ($WAYLAND_DISPLAY) to nest inside — not portable to a bare sandboxed CI runner without one"]
fn mudhuts_starts_cleanly_nested_under_a_live_host_compositor() {
    assert!(
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        "no WAYLAND_DISPLAY set — this test needs a live host Wayland compositor to nest inside"
    );

    // Captured to a real file, not a piped Stdio — mudhuts' own winit
    // backend logs a genuinely large amount at startup (EGL/GL extension
    // lists especially), easily enough to fill an OS pipe buffer. A pipe
    // nobody's draining concurrently would then make the child block on
    // its own write() call the moment that fills, which would silently
    // turn "mudhuts is running normally" into "mudhuts is stuck waiting
    // on us" for the whole `RUN_DURATION` sleep below — a file write
    // never blocks on a reader, so this can't happen.
    //
    // Both stdout *and* stderr point at the same file (via a cloned fd,
    // so they share one file offset and interleave correctly — same
    // idea as a shell's own `2>&1`), not just stderr: `main.rs`'s
    // `init_logging` builds its `tracing_subscriber::fmt::layer()` with
    // no `.with_writer` override, so every `tracing::info!`/`warn!`/
    // `error!` call — including `run()`'s own non-panic startup-failure
    // path (`Err(err) => tracing::error!("{err}")`) — defaults to
    // *stdout*, not stderr. An earlier version of this test only
    // captured stderr and discarded stdout, which would have silently
    // thrown away the one diagnostic line that actually explains a
    // non-panicking startup failure, defeating the point of capturing
    // anything at all for exactly that case (caught in review). Only
    // the default panic hook's own raw stderr print is stderr-only —
    // captured either way now that both streams land in one file.
    let output_path = std::env::temp_dir().join(format!("mudhuts-nested-smoke-{}.log", std::process::id()));
    // Guards cleanup from here on, regardless of which path (success or
    // an early panic below) the rest of this function actually takes —
    // see its own doc comment.
    let _remove_output_guard = RemoveOnDrop(output_path.clone());
    let stderr_file = std::fs::File::create(&output_path)
        .unwrap_or_else(|err| panic!("failed to create {output_path:?}: {err}"));
    let stdout_file =
        stderr_file.try_clone().unwrap_or_else(|err| panic!("failed to clone the capture file handle: {err}"));

    // No `--tty` (see `main.rs`'s own `parse_args` doc comment: real
    // `--tty` is a DRM/seat-owning backend, explicit opt-in only) — the
    // absence of it is exactly what selects the nested winit backend
    // this test means to exercise.
    let child = Command::new(env!("CARGO_BIN_EXE_mudhuts"))
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn mudhuts: {err}"));
    let mut guard = KillOnDrop(child);

    std::thread::sleep(RUN_DURATION);

    // Checked *before* killing it — `try_wait` returning `Ok(None)`
    // means it's still alive at this point, which is the actual thing
    // "started cleanly and ran stably" means here. Killing first would
    // make every run trivially "not running" regardless of whether it
    // had already crashed on its own well before this point.
    let still_running = guard
        .0
        .try_wait()
        .unwrap_or_else(|err| panic!("failed to poll mudhuts' status: {err}"))
        .is_none();

    // Explicit, ordered cleanup — not just letting `guard` fall out of
    // scope at the end of the function — so the process is fully killed
    // and reaped (all its writes to `output_path` flushed and complete)
    // *before* that file gets read just below.
    drop(guard);

    // Raw bytes, not `read_to_string` — mudhuts' own captured output can
    // include native EGL/Mesa/GPU-driver text that isn't guaranteed
    // valid UTF-8, and `read_to_string` treats that the same as a real
    // I/O error. An earlier version used `read_to_string(..)
    // .unwrap_or_default()`, which silently turned *either* case into an
    // empty string — meaning a genuine panic recorded right next to a
    // stray non-UTF-8 byte would make the assertion below spuriously
    // pass instead of catching it (caught in review). A real I/O error
    // still panics loudly here (nothing to silently default past); only
    // invalid UTF-8 is tolerated, and only by lossily replacing just the
    // bad bytes — everything around them, including a `panicked at`
    // line, survives intact.
    let output_bytes = std::fs::read(&output_path).unwrap_or_else(|err| panic!("failed to read {output_path:?}: {err}"));
    let output = String::from_utf8_lossy(&output_bytes).into_owned();
    // No explicit `remove_file` here — `_remove_output_guard` (above)
    // handles it uniformly on every path out of this function, this one
    // included.

    // Both `tracing::error!` (stdout, per `init_logging`'s own default
    // writer — see the capture setup above) and the default panic hook's
    // own raw stderr print land in this one merged file, so this catches
    // a panic regardless of which path actually fired first.
    assert!(!output.contains("panicked at"), "mudhuts panicked while running nested:\n{output}");
    assert!(
        still_running,
        "mudhuts exited on its own within {RUN_DURATION:?} instead of running stably — output:\n{output}"
    );
}
