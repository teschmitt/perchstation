//! Production [`QrFrameSource`] backed by the Pi camera still CLI —
//! `rpicam-still` by default, configurable via `[capture].camera_still_command`.
//!
//! The Raspberry Pi camera stack does not expose a stable in-process Rust
//! binding, so we shell out to the stock `libcamera-still` CLI for the
//! single still capture enrollment needs. This keeps `perchstation-hw`
//! free of vendor SDKs and keeps the surface inside the constitution's
//! "hardware at the boundary" rule.
//!
//! Cfg-gated to `target_os = "linux"` because `libcamera-still` only
//! exists on Pi-class Linux distros. On other targets, the operator falls
//! back to `--qr-source=file`.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use perchstation_core::hw_traits::{QrFrameError, QrFrameSource};
use thiserror::Error;
use tokio::process::Command;

/// QR source that drives `libcamera-still` for one still per enrollment
/// attempt. The capture file is written to a freshly-created tempdir that
/// is removed after the frame is read.
pub struct CameraQrSource {
    binary: PathBuf,
    width: u32,
    height: u32,
    timeout_ms: u32,
}

impl Default for CameraQrSource {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("rpicam-still"),
            width: 800,
            height: 600,
            // `--immediate` exits as soon as the sensor produces a frame.
            // The timeout is a belt-and-braces ceiling for a wedged camera.
            timeout_ms: 2_000,
        }
    }
}

impl CameraQrSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the binary path (wired from `[capture].camera_still_command`;
    /// also used in tests and on hosts with a non-standard prefix).
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }
}

#[derive(Debug, Error)]
enum CaptureError {
    #[error("failed to create capture tempdir: {0}")]
    Tempdir(#[from] std::io::Error),
    #[error("`{binary}` exited {status:?}: {stderr}")]
    NonZero { binary: String, status: Option<i32>, stderr: String },
    #[error("`{binary}` produced no output file at {path}")]
    NoOutput { binary: String, path: PathBuf },
    #[error("could not decode captured JPEG: {0}")]
    Decode(#[from] image::ImageError),
}

#[async_trait]
impl QrFrameSource for CameraQrSource {
    async fn next_frame(&mut self) -> Result<image::GrayImage, QrFrameError> {
        capture(self).await.map_err(|err| match err {
            CaptureError::Tempdir(source) => QrFrameError::Io { source },
            CaptureError::Decode(err) => QrFrameError::Decode(err.to_string()),
            other @ (CaptureError::NonZero { .. } | CaptureError::NoOutput { .. }) => {
                QrFrameError::Unavailable(other.to_string())
            }
        })
    }
}

async fn capture(source: &CameraQrSource) -> Result<image::GrayImage, CaptureError> {
    // The tempdir is sync-only; that's fine — it's a one-time mkdir.
    let dir = tempfile::tempdir()?;
    let out_path = dir.path().join("qr.jpg");

    let output = Command::new(&source.binary)
        .arg("--immediate")
        .arg("--nopreview")
        .arg("--width")
        .arg(source.width.to_string())
        .arg("--height")
        .arg(source.height.to_string())
        .arg("--timeout")
        .arg(source.timeout_ms.to_string())
        .arg("--output")
        .arg(&out_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|err| CaptureError::NonZero {
            binary: source.binary.display().to_string(),
            status: None,
            stderr: format!("spawn error: {err}"),
        })?;

    if !output.status.success() {
        return Err(CaptureError::NonZero {
            binary: source.binary.display().to_string(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    if !out_path.exists() {
        return Err(CaptureError::NoOutput {
            binary: source.binary.display().to_string(),
            path: out_path,
        });
    }

    let bytes = std::fs::read(&out_path).map_err(CaptureError::from)?;
    let img = image::load_from_memory(&bytes)?;
    Ok(img.into_luma8())
}
