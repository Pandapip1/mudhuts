//! `org.freedesktop.impl.portal.ScreenCast` — **the D-Bus session/method
//! surface is real**: `CreateSession` and `SelectSources` genuinely track
//! per-session state, and every session exports a working
//! `org.freedesktop.impl.portal.Session` object at its `session_handle`
//! path, so `Session.Close()` and the `Closed` signal behave correctly
//! and sessions don't leak.
//!
//! **`Start` is where this is honestly incomplete.** Real screen-sharing
//! clients (browsers, video call apps) expect the actual video frames
//! delivered over a PipeWire stream — `Start`'s success reply is supposed
//! to hand back a PipeWire node id the caller then connects to. Producing
//! one for real means running a PipeWire stream/loop in this process (the
//! `pipewire` crate), which needs libpipewire's development headers and
//! pkg-config files at build time. Those aren't present in this repo's
//! Nix flake devShell (`flake.nix`'s `buildInputs`/`nativeBuildInputs`
//! have no `pipewire`/`pipewire.dev`) — adding them is a one-line flake
//! change, but the brief for this pass was explicit that the flake is a
//! shared file to flag rather than edit unilaterally, so it's flagged
//! here and in the top-level report instead.
//!
//! Rather than fabricate a node id that would produce a stream with no
//! actual frames behind it (indistinguishable, from the caller's side,
//! from "it's about to start" — exactly the kind of silent-failure this
//! project's conventions call out), `Start` fails outright (portal
//! response code `2`, "other error") after logging why. Every caller gets
//! a real, immediate, honest error instead of a screen share that looks
//! like it started and never shows anything. Wiring up the PipeWire
//! producer once the dependency is available is a self-contained follow-up
//! — `CreateSession`/`SelectSources`/the `Session` object don't need to
//! change at all for it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

mod response {
    pub const SUCCESS: u32 = 0;
    pub const OTHER_ERROR: u32 = 2;
}

/// `AvailableSourceTypes` bit values from the interface spec.
mod source_type {
    pub const MONITOR: u32 = 1;
}

/// `AvailableCursorModes` bit values from the interface spec.
mod cursor_mode {
    pub const HIDDEN: u32 = 1;
}

/// Per-session bookkeeping, keyed by the session's own object path (the
/// `session_handle` the frontend chose in `CreateSession`). Recorded for
/// real even though `Start` can't act on it yet, so that landing PipeWire
/// support later is just teaching `Start` to read this map — no session
/// lifecycle rework needed.
#[derive(Default)]
struct SessionState {
    #[allow(dead_code)]
    source_types: u32,
    #[allow(dead_code)]
    cursor_mode: u32,
}

type Sessions = Arc<Mutex<HashMap<OwnedObjectPath, SessionState>>>;

/// Recovers the guard on a poisoned mutex instead of panicking — per this
/// project's no-panics convention, one dropped-while-panicking guard
/// (which shouldn't happen, since nothing in this module panics) must
/// never take the whole portal backend down.
fn lock(sessions: &Sessions) -> MutexGuard<'_, HashMap<OwnedObjectPath, SessionState>> {
    sessions.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct ScreenCastBackend {
    sessions: Sessions,
}

impl ScreenCastBackend {
    pub fn new() -> Self {
        Self { sessions: Arc::new(Mutex::new(HashMap::new())) }
    }
}

impl Default for ScreenCastBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastBackend {
    #[zbus(property)]
    fn version(&self) -> u32 {
        4
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        // MONITOR only — mudhuts is a single-output compositor with no
        // per-window capture source in this pass (see this crate's
        // top-level docs and `mudhuts/src/handlers/capture.rs`).
        source_type::MONITOR
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        // HIDDEN only — no cursor compositing/metadata support exists
        // yet, and advertising EMBEDDED/METADATA without honoring them
        // would be exactly the kind of silent lie this module's doc is
        // about avoiding.
        cursor_mode::HIDDEN
    }

    async fn create_session(
        &self,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        lock(&self.sessions).insert(session_handle.clone(), SessionState::default());

        let session_object = SessionImpl {
            path: session_handle.clone(),
            sessions: self.sessions.clone(),
        };
        if let Err(err) = object_server.at(session_handle.clone(), session_object).await {
            tracing::warn!("mudhuts-portal: failed to export the Session object at {session_handle}: {err}");
            lock(&self.sessions).remove(&session_handle);
            return Ok((response::OTHER_ERROR, HashMap::new()));
        }

        Ok((response::SUCCESS, HashMap::new()))
    }

    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let mut sessions = lock(&self.sessions);
        let Some(session) = sessions.get_mut(&session_handle) else {
            tracing::warn!("mudhuts-portal: SelectSources for an unknown session {session_handle}");
            return Ok((response::OTHER_ERROR, HashMap::new()));
        };

        if let Some(types) = options.get("types").and_then(|v| u32::try_from(v.clone()).ok()) {
            session.source_types = types;
        }
        if let Some(mode) = options.get("cursor_mode").and_then(|v| u32::try_from(v.clone()).ok()) {
            session.cursor_mode = mode;
        }

        Ok((response::SUCCESS, HashMap::new()))
    }

    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        if !lock(&self.sessions).contains_key(&session_handle) {
            tracing::warn!("mudhuts-portal: Start for an unknown session {session_handle}");
            return Ok((response::OTHER_ERROR, HashMap::new()));
        }

        tracing::warn!(
            "mudhuts-portal: ScreenCast.Start refused for session {session_handle} — no PipeWire frame \
             delivery is wired up in this build (see screencast.rs's module doc for exactly why)"
        );
        Ok((response::OTHER_ERROR, HashMap::new()))
    }
}

/// `org.freedesktop.impl.portal.Session` — exported dynamically at
/// whatever path the frontend picked, one instance per live session (see
/// `ScreenCastBackend::create_session`).
struct SessionImpl {
    path: OwnedObjectPath,
    sessions: Sessions,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionImpl {
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }

    async fn close(
        &self,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        lock(&self.sessions).remove(&self.path);
        if let Err(err) = Self::closed(&emitter).await {
            tracing::warn!("mudhuts-portal: failed to emit Session.Closed for {}: {err}", self.path);
        }
        if let Err(err) = object_server.remove::<SessionImpl, _>(&self.path).await {
            tracing::warn!("mudhuts-portal: failed to remove the Session object at {}: {err}", self.path);
        }
        Ok(())
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
