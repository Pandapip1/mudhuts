//! `org.freedesktop.impl.portal.Screenshot` — v1 scope is a single
//! non-interactive whole-output screenshot: no picker UI, no
//! window/region targets (the interface's `target`/`interactive` options
//! are accepted but ignored). Captures via `ext-image-copy-capture-v1` on
//! this crate's own Wayland connection (see `wayland.rs`), encodes to
//! PNG, and returns a `file://` URI per the interface's `Screenshot`
//! method contract.
//!
//! `PickColor` is not implemented in this pass — it's simply not declared
//! on the `#[interface]` impl below, so a caller invoking it gets the
//! standard D-Bus `UnknownMethod` error rather than a half-working color
//! picker.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot};
use zbus::interface;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Str};

use crate::wayland::{CapturedImage, Job};

/// Portal response codes used in the `(response, results)` tuple every
/// `org.freedesktop.impl.portal.*` method returns. Only the two this
/// backend ever produces are named — see the interface's own doc for the
/// third (`1` = user cancelled), which never applies here since v1 has no
/// interactive UI to cancel out of.
mod response {
    pub const SUCCESS: u32 = 0;
    pub const OTHER_ERROR: u32 = 2;
}

pub struct ScreenshotBackend {
    jobs: mpsc::UnboundedSender<Job>,
}

impl ScreenshotBackend {
    pub fn new(jobs: mpsc::UnboundedSender<Job>) -> Self {
        Self { jobs }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl ScreenshotBackend {
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }

    async fn screenshot(
        &self,
        _handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.jobs.send(Job::Screenshot(reply_tx)).is_err() {
            tracing::error!("mudhuts-portal: the Wayland capture thread is gone, can't take a screenshot");
            return Ok((response::OTHER_ERROR, HashMap::new()));
        }

        let image = match reply_rx.await {
            Ok(Ok(image)) => image,
            Ok(Err(err)) => {
                tracing::warn!("mudhuts-portal: screenshot capture failed: {err}");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
            Err(_) => {
                tracing::error!("mudhuts-portal: the Wayland capture thread dropped the reply channel");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
        };

        let path = match save_png(&image) {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!("mudhuts-portal: failed to save the screenshot: {err}");
                return Ok((response::OTHER_ERROR, HashMap::new()));
            }
        };

        let uri = format!("file://{}", path.display());
        let mut results = HashMap::new();
        results.insert("uri".to_string(), OwnedValue::from(Str::from(uri)));
        Ok((response::SUCCESS, results))
    }
}

/// Where screenshots get saved: `$XDG_CACHE_HOME/mudhuts-portal/screenshots`
/// (falling back to `~/.cache/...`, then `/tmp/...` if even `$HOME` is
/// unset). A cache directory rather than `~/Pictures/Screenshots` (what
/// some other backends use) — keeping this v1 free of a dependency on
/// resolving XDG user directories felt like the right tradeoff; the file
/// this returns is a normal file on disk either way, so nothing stops a
/// user or a later revision from moving it.
fn screenshot_dir() -> PathBuf {
    if let Ok(cache) = std::env::var("XDG_CACHE_HOME")
        && !cache.is_empty()
    {
        return PathBuf::from(cache).join("mudhuts-portal/screenshots");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/mudhuts-portal/screenshots")
}

fn save_png(image: &CapturedImage) -> Result<PathBuf, String> {
    let dir = screenshot_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("screenshot-{timestamp}.png"));

    let file = std::fs::File::create(&path).map_err(|err| format!("failed to create {}: {err}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|err| format!("failed to write PNG header for {}: {err}", path.display()))?;
    writer
        .write_image_data(&image.rgba)
        .map_err(|err| format!("failed to write PNG data for {}: {err}", path.display()))?;

    Ok(path)
}
