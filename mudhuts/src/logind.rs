//! Takes systemd-logind's `handle-power-key` inhibitor lock for the
//! lifetime of the process, so logind's own default action (an
//! immediate poweroff on every power-button press, independent of any
//! Wayland client — logind reads the same evdev device directly) never
//! races `mudhuts_power_button_v1` (see
//! `mudhuts-protocols/protocol/mudhuts-power-button.xml` and
//! `handlers/power_button.rs`). Meant to run only under a real TTY
//! session (`main.rs`'s `--tty`) — a nested winit session isn't the
//! seat logind's power-key handling even applies to. Also gated on
//! `[power] inhibit-power-key` in `config.toml` — see `power_config.rs`'s
//! own doc comment for why this is on by default but has a real opt-out.
//!
//! `mode: "block"` (not `"delay"`) is deliberate: `"delay"` only
//! postpones logind's default action by a few seconds, `"block"`
//! suppresses it entirely for as long as the returned fd stays open —
//! this feature exists specifically so a `mudhuts_power_button_v1`
//! client (or, absent one, mudhuts' own logout fallback) is the *only*
//! thing that ever acts on the key, not just the first.
//!
//! Uses `zbus`, not a shelled-out `busctl call` — raised more than once
//! in review as inconsistent with `keybindings.rs`'s own media-key
//! handling, which shells out to `brightnessctl`/`wpctl` specifically to
//! avoid a D-Bus dependency in the main compositor binary. Not the same
//! situation: `Inhibit` returns a live file descriptor as its reply,
//! which is exactly the part `busctl call`'s text-based CLI output has
//! no clean, non-fragile way to hand back to a caller (unlike
//! `brightnessctl`/`wpctl`, both fire-and-forget commands with no
//! meaningful reply to capture at all) — parsing a raw fd out of a
//! spawned child's stdout would be far hackier than the dependency it'd
//! save. `zbus`'s own default-feature footprint is trimmed instead (see
//! `Cargo.toml`'s own comment on this dependency).

/// Spawns a thread that takes the inhibitor lock and intentionally leaks
/// the returned fd (`mem::forget`, same idiom `test_support.rs`'s own
/// `Box::leak` already uses in this codebase for the same kind of
/// "must outlive everything, nothing left to hand back to" need) rather
/// than handing it back to any caller — a plain, non-`OwnedFd` raw fd
/// held open in the kernel for the rest of the process's life, exactly
/// as if some long-lived value owned it, without actually needing one.
/// The thread itself then exits normally; it doesn't need to stay alive
/// itself (a prior version parked it forever purely to keep the fd's
/// owning value in scope, one whole OS thread spent on holding a single
/// integer — caught in review). Dropping the fd, including on process
/// exit, hands the power key back to logind's own default immediately —
/// `mem::forget` on the fd's owning value never runs that `Drop`, so
/// only the process's own exit (which closes every fd regardless of
/// what userspace thinks owns them) actually releases it.
///
/// Returns immediately itself, without waiting for the D-Bus round trip
/// to complete: an earlier version made this call synchronously on the
/// main thread before `udev_backend::init_udev`, which meant a slow-to-
/// start system bus/logind (plausible early at boot) delayed DRM master
/// acquisition and the compositor's first frame for no reason a user
/// could see (caught in review) — nothing on the startup path actually
/// needs this lock to be held *yet*, only eventually, so there's nothing
/// to block on.
///
/// Never fatal if the thread itself fails to spawn (logged instead) or
/// if the D-Bus call inside it fails (see `inhibit_power_key`'s own doc
/// comment) — either way, failure just means logind's poweroff-on-press
/// default stays in effect, exactly as if this feature didn't exist.
pub(crate) fn spawn_power_key_inhibitor() {
    let result = std::thread::Builder::new().name("power-key-inhibit".to_string()).spawn(|| {
        if let Some(fd) = inhibit_power_key() {
            std::mem::forget(fd);
        }
    });
    if let Err(err) = result {
        tracing::warn!(
            "failed to spawn the logind power-key-inhibitor thread, power button presses fall \
             back to systemd-logind's own default (immediate poweroff): {err}"
        );
    }
}

/// Never fatal — same "best-effort startup step, logged and skipped"
/// posture as `rt_sched::apply` (see its own doc comment): failure just
/// means logind's poweroff-on-press default stays in effect, exactly as
/// if this feature didn't exist. Logged at `warn` so a user who expected
/// mudhuts to own this decision can tell why it didn't take (no logind
/// running, a permission problem, ...).
///
/// Neither blocking D-Bus call below has a timeout — a genuinely wedged
/// (not just slow-to-start) system bus/logind would hang this whole
/// function forever with no warning ever logged, unlike every other
/// failure path here (caught in review). Harmless since this always runs
/// on its own dedicated thread (see `spawn_power_key_inhibitor`) rather
/// than anything else waiting on it, but the debug line below at least
/// gives a real hang something to look like in the logs, rather than
/// being silently indistinguishable from this feature never having run
/// at all.
fn inhibit_power_key() -> Option<zbus::zvariant::OwnedFd> {
    tracing::debug!("connecting to the system D-Bus to take logind's handle-power-key inhibitor lock");
    let connection = match zbus::blocking::Connection::system() {
        Ok(connection) => connection,
        Err(err) => {
            tracing::warn!(
                "failed to connect to the system D-Bus, power button presses fall back to \
                 systemd-logind's own default (immediate poweroff): {err}"
            );
            return None;
        }
    };
    let proxy = match zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    ) {
        Ok(proxy) => proxy,
        Err(err) => {
            tracing::warn!(
                "failed to reach systemd-logind, power button presses fall back to its own \
                 default (immediate poweroff): {err}"
            );
            return None;
        }
    };

    let result: zbus::Result<zbus::zvariant::OwnedFd> = proxy.call(
        "Inhibit",
        &(
            "handle-power-key",
            "mudhuts",
            "let mudhuts_power_button_v1 clients (or mudhuts' own logout fallback) decide",
            "block",
        ),
    );
    match result {
        Ok(fd) => {
            tracing::info!("took logind's handle-power-key inhibitor lock");
            Some(fd)
        }
        Err(err) => {
            tracing::warn!(
                "failed to take logind's handle-power-key inhibitor lock, power button presses \
                 fall back to its own default (immediate poweroff): {err}"
            );
            None
        }
    }
}
