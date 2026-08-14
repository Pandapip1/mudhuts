//! `mudhuts-portal` — a standalone `org.freedesktop.impl.portal.*` D-Bus
//! backend for mudhuts, structured the same way as
//! `mudhuts-authority-helper`: a normal Wayland client of whatever
//! compositor `WAYLAND_DISPLAY` points to (see `wayland.rs`), no special
//! privilege needed, which *also* registers itself as a D-Bus service on
//! the session bus so `xdg-desktop-portal` can activate it on behalf of
//! sandboxed (Flatpak) apps.
//!
//! Implements three of the `org.freedesktop.impl.portal.*` interfaces:
//!
//! - **Settings** (`settings.rs`) — complete. A small fixed table
//!   (`color-scheme`/`contrast`/`reduced-motion`), no live sync.
//! - **Screenshot** (`screenshot.rs`) — complete for the scope of this
//!   pass: whole-output only, no interactive picker, via
//!   `ext-image-copy-capture-v1`.
//! - **ScreenCast** (`screencast.rs`) — complete: `Start` spawns a real
//!   `pipewire_stream` producer (single monitor source, fixed
//!   resolution, no cursor compositing) and returns its PipeWire node
//!   id, matching the interface's contract.
//!
//! **FileChooser and every other portal interface are deliberately not
//! implemented here.** See `mudhuts-portals.conf` and `mudhuts.portal` in
//! this crate's directory for the D-Bus/portal-config setup that routes
//! Settings/Screenshot/ScreenCast to this backend while leaving
//! FileChooser (and anything else) to an existing GTK/KDE portal backend.

mod pipewire_stream;
mod screencast;
mod screenshot;
mod settings;
mod wayland;

use std::process::ExitCode;

use tokio::sync::mpsc;

/// The well-known bus name this backend activates on, and the object path
/// every `org.freedesktop.impl.portal.*` interface is served at — both
/// fixed by the portal spec's own conventions (see `mudhuts.portal`'s
/// `DBusName` and the interface docs' object path).
const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.mudhuts";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();

    let (job_tx, job_rx) = mpsc::unbounded_channel();
    // The Wayland client runs its own blocking dispatch loop on a
    // dedicated OS thread (see `wayland.rs`'s module doc for why) rather
    // than sharing the async runtime the D-Bus side uses.
    std::thread::spawn(move || wayland::run(job_rx));

    let conn = match start_service(job_tx).await {
        Ok(conn) => conn,
        Err(err) => {
            tracing::error!("mudhuts-portal: failed to start the D-Bus service: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Kept alive for as long as this process runs — dropping it would
    // tear down the bus connection (and with it, every served
    // interface).
    let _conn = conn;

    tracing::info!(
        "mudhuts-portal: registered {BUS_NAME} at {OBJECT_PATH}, serving Settings/Screenshot/ScreenCast"
    );
    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    ExitCode::SUCCESS
}

async fn start_service(job_tx: mpsc::UnboundedSender<wayland::Job>) -> zbus::Result<zbus::Connection> {
    let settings = settings::SettingsBackend::new();
    let screencast = screencast::ScreenCastBackend::new(job_tx.clone());
    let screenshot = screenshot::ScreenshotBackend::new(job_tx);

    zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, settings)?
        .serve_at(OBJECT_PATH, screenshot)?
        .serve_at(OBJECT_PATH, screencast)?
        .build()
        .await
}

fn init_logging() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}
