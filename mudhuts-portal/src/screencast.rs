//! `org.freedesktop.impl.portal.ScreenCast` — the D-Bus session/method
//! surface: `CreateSession` and `SelectSources` track per-session state,
//! every session exports a working `org.freedesktop.impl.portal.Session`
//! object at its `session_handle` path (so `Session.Close()` and the
//! `Closed` signal behave correctly and sessions don't leak), and
//! `Start` spawns a real `pipewire_stream` producer, waits for it to
//! reach PipeWire's `Paused` state, and returns its node id in the
//! `streams` result — the actual video frames arrive over that PipeWire
//! node from then on, fed by `pipewire_stream::run`'s `process` callback
//! pulling from `wayland.rs`'s continuous capture session.
//!
//! Scope carried over from `screenshot.rs`: single monitor source only
//! (no per-window capture, no picker), fixed resolution for the whole
//! session (mudhuts doesn't support resizing a stream mid-flight), no
//! cursor compositing (`available_cursor_modes` only ever advertises
//! `HIDDEN`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::pipewire_stream::{self, PwCast};
use crate::wayland;

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
/// `session_handle` the frontend chose in `CreateSession`). `cast` is
/// `Some` only between a successful `Start` and the session closing —
/// `SessionImpl::close` (and a repeat `Start`, though that's rejected
/// rather than restarted) is what tears it down.
#[derive(Default)]
struct SessionState {
    #[allow(dead_code)]
    source_types: u32,
    #[allow(dead_code)]
    cursor_mode: u32,
    cast: Option<PwCast>,
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
    jobs: mpsc::UnboundedSender<wayland::Job>,
}

impl ScreenCastBackend {
    pub fn new(jobs: mpsc::UnboundedSender<wayland::Job>) -> Self {
        Self { sessions: Arc::new(Mutex::new(HashMap::new())), jobs }
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
            jobs: self.jobs.clone(),
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
        match lock(&self.sessions).get(&session_handle) {
            None => {
                tracing::warn!("mudhuts-portal: Start for an unknown session {session_handle}");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
            Some(session) if session.cast.is_some() => {
                tracing::warn!("mudhuts-portal: Start called twice for session {session_handle}");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
            Some(_) => {}
        }

        let (size_tx, size_rx) = oneshot::channel();
        if self.jobs.send(wayland::Job::StartCapture(size_tx)).is_err() {
            tracing::error!("mudhuts-portal: the Wayland capture thread is gone, can't start a screencast");
            return Ok((response::OTHER_ERROR, HashMap::new()));
        }
        let (width, height) = match size_rx.await {
            Ok(Ok(size)) => size,
            Ok(Err(err)) => {
                tracing::warn!("mudhuts-portal: failed to start screencast capture: {err}");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
            Err(_) => {
                tracing::error!("mudhuts-portal: the Wayland capture thread dropped the reply channel");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
        };

        let (cast, node_id_rx) = pipewire_stream::start(width, height, self.jobs.clone());

        let node_id = match tokio::time::timeout(Duration::from_secs(5), node_id_rx).await {
            Ok(Ok(Ok(id))) => id,
            Ok(Ok(Err(err))) => {
                tracing::warn!("mudhuts-portal: screencast PipeWire stream failed to start: {err}");
                cast.stop();
                let _ = self.jobs.send(wayland::Job::StopCapture);
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
            Ok(Err(_)) => {
                tracing::warn!("mudhuts-portal: screencast PipeWire thread dropped the node id channel");
                let _ = self.jobs.send(wayland::Job::StopCapture);
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
            Err(_) => {
                tracing::warn!("mudhuts-portal: timed out waiting for the screencast PipeWire stream to start");
                cast.stop();
                let _ = self.jobs.send(wayland::Job::StopCapture);
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
        };

        {
            let mut sessions = lock(&self.sessions);
            match sessions.get_mut(&session_handle) {
                Some(session) => session.cast = Some(cast),
                None => {
                    // The session closed while Start was in flight.
                    cast.stop();
                    let _ = self.jobs.send(wayland::Job::StopCapture);
                    return Ok((response::OTHER_ERROR, HashMap::new()));
                }
            }
        }

        let size_value: OwnedValue = match Value::from((width as i32, height as i32)).try_into() {
            Ok(value) => value,
            Err(err) => {
                tracing::error!("mudhuts-portal: failed to encode the screencast stream size: {err}");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
        };
        let mut stream_props: HashMap<String, OwnedValue> = HashMap::new();
        stream_props.insert("source_type".to_string(), OwnedValue::from(source_type::MONITOR));
        stream_props.insert("size".to_string(), size_value);
        let streams: Vec<(u32, HashMap<String, OwnedValue>)> = vec![(node_id, stream_props)];

        let streams_value: OwnedValue = match Value::from(streams).try_into() {
            Ok(value) => value,
            Err(err) => {
                tracing::error!("mudhuts-portal: failed to encode the screencast streams result: {err}");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
        };
        let mut results = HashMap::new();
        results.insert("streams".to_string(), streams_value);
        Ok((response::SUCCESS, results))
    }
}

/// `org.freedesktop.impl.portal.Session` — exported dynamically at
/// whatever path the frontend picked, one instance per live session (see
/// `ScreenCastBackend::create_session`).
struct SessionImpl {
    path: OwnedObjectPath,
    sessions: Sessions,
    jobs: mpsc::UnboundedSender<wayland::Job>,
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
        if let Some(session) = lock(&self.sessions).remove(&self.path)
            && let Some(cast) = session.cast
        {
            cast.stop();
            let _ = self.jobs.send(wayland::Job::StopCapture);
        }
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
