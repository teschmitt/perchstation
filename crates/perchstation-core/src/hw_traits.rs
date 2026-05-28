//! Traits that draw the hardware-boundary line.
//!
//! `perchstation-core` depends on these and on nothing else hardware-shaped;
//! `perchstation-hw` provides the production implementations
//! ([`crate::perchstation_hw::clock::SystemClock`] and the
//! libcamera-driven QR source); integration tests provide fake
//! implementations under `tests/integration/support/`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// Pull a single grayscale frame from a QR source.
///
/// The trait is intentionally async + dyn-compatible: the production
/// implementation in `perchstation-hw` shells out to `libcamera-still` and
/// blocks, while the file-based and in-memory implementations used by
/// `tests/integration/` finish synchronously. The enrollment command holds
/// a `Box<dyn QrFrameSource>` so the source can be selected at runtime
/// (`--qr-source camera|file`).
///
/// Implementations:
/// - `perchstation_hw::camera_qr::CameraQrSource` (production, Linux-only)
/// - `perchstation_core::enrollment::file_source::FileQrSource` (recovery / dev)
/// - `tests::integration::support::fake_qr::FakeQrSource` (tests)
#[async_trait]
pub trait QrFrameSource: Send + Sync {
    /// Acquire one grayscale frame, ready to feed to the QR decoder.
    async fn next_frame(&mut self) -> Result<image::GrayImage, QrFrameError>;
}

#[derive(Debug, thiserror::Error)]
pub enum QrFrameError {
    #[error("no QR frame available: {0}")]
    Unavailable(String),
    #[error("frame source I/O error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
    #[error("frame source decode error: {0}")]
    Decode(String),
}

/// Read-only access to wall-clock UTC.
///
/// All delivery-loop logic that reasons about backoff schedules, cert
/// expiry, or "now" timestamps depends on this trait rather than calling
/// `chrono::Utc::now` directly, so tests can inject a deterministic clock.
///
/// Implementations:
/// - `perchstation_hw::clock::SystemClock` (production)
/// - `tests::integration::support::*::FakeClock` (tests)
pub trait Clock: Send + Sync {
    /// Return the current wall-clock time in UTC.
    fn now(&self) -> DateTime<Utc>;
}
