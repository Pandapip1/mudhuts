//! The PipeWire half of `screencast.rs`: a `Video/Source` producer
//! stream running on its own dedicated OS thread (this crate's Wayland
//! capture also gets its own thread — see `wayland.rs`'s module doc for
//! why the same pattern applies here: a `pipewire::main_loop::MainLoop`
//! has to own its thread via `MainLoop::run()`, which never returns
//! while the loop is alive, so it can't share a thread with the async
//! D-Bus side or the blocking Wayland dispatch loop).
//!
//! Modeled closely on how [niri's `pw_utils.rs`](https://github.com/YaLTeR/niri/blob/main/src/pw_utils.rs)
//! sets up its own portal-facing PipeWire producer — same overall shape
//! (fixed-format `param_changed`, capture the node id at the first
//! `Paused` transition, feed frames from `process`) — but simplified for
//! CPU/SHM-buffer frames instead of niri's DMA-BUF path: no explicit
//! `SPA_PARAM_Buffers` negotiation or `add_buffer` handling is needed
//! here, matching `pipewire-rs`'s own plain `examples/tone.rs`/
//! `examples/streams.rs`, since `StreamFlags::MAP_BUFFERS` is enough for
//! PipeWire to allocate and map ordinary memory buffers on its own.
//!
//! Frames are pulled on demand: `process` fires once per PipeWire graph
//! cycle once a real consumer is linked to this stream, and each firing
//! does one synchronous round trip to the Wayland capture thread (via
//! `wayland::Job::CaptureFrame`) to get the latest frame, blocking this
//! thread — never the D-Bus/tokio side — until it arrives. No consumer
//! linked means `process` never fires at all, so an accepted-but-unwatched
//! screencast costs nothing beyond the idle stream/session bookkeeping.

use tokio::sync::{mpsc, oneshot};

use pipewire as pw;
use pw::spa;
use spa::pod::Pod;

use crate::wayland;

/// Sent through `pipewire::channel` to make the producer thread's
/// `MainLoop` quit. A dedicated channel type is required (rather than
/// e.g. `std::sync::mpsc`) because the receiving side has to be attached
/// directly to the PipeWire loop as an event source — `MainLoop::run()`
/// never returns to ordinary Rust code to poll anything on its own.
struct Quit;

/// Handle to a running producer thread. Has no `Drop` impl on purpose —
/// dropping this does *not* stop the thread; call [`PwCast::stop`]
/// explicitly once the portal session backing it closes
/// (`screencast.rs`'s `Session.Close`/re-`Start` handling), so a stream
/// staying alive is always a deliberate, traceable decision rather than
/// an implicit side effect of a value going out of scope.
pub struct PwCast {
    quit_tx: pw::channel::Sender<Quit>,
}

impl PwCast {
    pub fn stop(self) {
        if self.quit_tx.send(Quit).is_err() {
            tracing::warn!("mudhuts-portal: screencast PipeWire thread was already gone when asked to stop");
        }
    }
}

/// Shared across this stream's callbacks via `add_local_listener_with_user_data` —
/// PipeWire delivers `&mut StreamData` to each one, all on this thread.
struct StreamData {
    /// Consumed the first time the stream reaches `Paused` (success) or
    /// `Error` (failure) — see `run`'s `state_changed` handler. `None`
    /// afterward, since the id is only ever reported once.
    node_id_tx: Option<oneshot::Sender<Result<u32, String>>>,
    jobs: mpsc::UnboundedSender<wayland::Job>,
}

/// Spawns the dedicated PipeWire thread for one ScreenCast session and
/// returns immediately. `width`/`height` are fixed for the session's
/// lifetime (queried once via `wayland::Job::StartCapture` before this
/// is even called — mudhuts is single-output and this stream doesn't
/// support renegotiating size mid-session).
///
/// The returned receiver resolves once the stream reaches PipeWire's
/// `Paused` state (the first point `Stream::node_id()` is meaningful)
/// with either the node id to hand back from `ScreenCast.Start`, or an
/// error if stream setup failed.
pub fn start(width: u32, height: u32, jobs: mpsc::UnboundedSender<wayland::Job>) -> (PwCast, oneshot::Receiver<Result<u32, String>>) {
    let (node_id_tx, node_id_rx) = oneshot::channel();
    let (quit_tx, quit_rx) = pw::channel::channel::<Quit>();

    std::thread::spawn(move || run(width, height, jobs, node_id_tx, quit_rx));

    (PwCast { quit_tx }, node_id_rx)
}

fn run(
    width: u32,
    height: u32,
    jobs: mpsc::UnboundedSender<wayland::Job>,
    node_id_tx: oneshot::Sender<Result<u32, String>>,
    quit_rx: pw::channel::Receiver<Quit>,
) {
    pw::init();

    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(err) => {
            let _ = node_id_tx.send(Err(format!("failed to create a PipeWire main loop: {err}")));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(err) => {
            let _ = node_id_tx.send(Err(format!("failed to create a PipeWire context: {err}")));
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(err) => {
            let _ = node_id_tx.send(Err(format!("failed to connect to the PipeWire daemon: {err}")));
            return;
        }
    };

    // Kept alive for the rest of this function (until `mainloop.run()`
    // returns) so a `Quit` sent from `PwCast::stop` keeps being able to
    // reach the loop.
    let _quit_receiver = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |Quit| mainloop.quit()
    });

    let stream = match pw::stream::StreamBox::new(
        &core,
        "mudhuts-screencast",
        pw::properties::properties! {
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(err) => {
            let _ = node_id_tx.send(Err(format!("failed to create a PipeWire stream: {err}")));
            return;
        }
    };

    let data = StreamData { node_id_tx: Some(node_id_tx), jobs };

    let listener_builder = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|stream, data, _old, new| {
            tracing::debug!("mudhuts-portal: screencast PipeWire stream state -> {new:?}");
            match new {
                pw::stream::StreamState::Paused => {
                    if let Some(tx) = data.node_id_tx.take() {
                        let _ = tx.send(Ok(stream.node_id()));
                    }
                }
                pw::stream::StreamState::Error(err) => {
                    if let Some(tx) = data.node_id_tx.take() {
                        let _ = tx.send(Err(format!("PipeWire stream entered an error state: {err}")));
                    }
                }
                _ => {}
            }
        })
        .process(|stream, data| {
            let (reply_tx, reply_rx) = oneshot::channel();
            if data.jobs.send(wayland::Job::CaptureFrame(reply_tx)).is_err() {
                tracing::warn!("mudhuts-portal: Wayland capture thread is gone, dropping a screencast frame");
                return;
            }
            // Safe to block this thread (not the tokio/D-Bus side) on
            // the Wayland round trip — see this module's doc.
            let image = match reply_rx.blocking_recv() {
                Ok(Ok(image)) => image,
                Ok(Err(err)) => {
                    tracing::warn!("mudhuts-portal: screencast frame capture failed: {err}");
                    return;
                }
                Err(_) => {
                    tracing::warn!("mudhuts-portal: Wayland capture thread dropped the reply channel");
                    return;
                }
            };

            let Some(mut buffer) = stream.dequeue_buffer() else {
                tracing::warn!("mudhuts-portal: PipeWire gave no buffer for a screencast frame");
                return;
            };
            let Some(data0) = buffer.datas_mut().first_mut() else {
                return;
            };
            let Some(slice) = data0.data() else {
                return;
            };
            let len = slice.len().min(image.rgba.len());
            slice[..len].copy_from_slice(&image.rgba[..len]);
            let chunk = data0.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = (image.width * 4) as i32;
            *chunk.size_mut() = len as u32;
        });

    let listener = match listener_builder.register() {
        Ok(l) => l,
        Err(err) => {
            // `data` (and the `node_id_tx` inside it) was consumed by
            // the builder above and is gone with this `Err` — nothing
            // left to report the failure through except the log;
            // `node_id_rx` will simply see the sender dropped, which
            // `screencast.rs` already treats as a real failure.
            tracing::warn!("mudhuts-portal: failed to register the screencast PipeWire stream listener: {err}");
            return;
        }
    };

    let obj = pw::spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
        pw::spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
        pw::spa::pod::property!(spa::param::format::FormatProperties::VideoFormat, Id, spa::param::video::VideoFormat::RGBA),
        pw::spa::pod::property!(spa::param::format::FormatProperties::VideoSize, Rectangle, spa::utils::Rectangle { width, height }),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 60, denom: 1 }
        ),
    );
    let values = match spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj)) {
        Ok(v) => v.0.into_inner(),
        Err(err) => {
            tracing::warn!("mudhuts-portal: failed to serialize the screencast format: {err:?}");
            return;
        }
    };
    let Some(format_pod) = Pod::from_bytes(&values) else {
        tracing::warn!("mudhuts-portal: failed to build a Pod from the serialized screencast format");
        return;
    };
    let mut params = [format_pod];

    if let Err(err) = stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    ) {
        tracing::warn!("mudhuts-portal: failed to connect the screencast PipeWire stream: {err}");
        return;
    }

    tracing::info!("mudhuts-portal: screencast PipeWire stream connected at {width}x{height}, waiting for a consumer");
    mainloop.run();
    drop(listener);
}
