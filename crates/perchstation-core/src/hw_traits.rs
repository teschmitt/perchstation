//! Traits that draw the hardware-boundary line.
//!
//! `perchstation-core` depends on these and on nothing else hardware-shaped;
//! `perchstation-hw` provides the production implementations
//! ([`crate::perchstation_hw::clock::SystemClock`] and the
//! libcamera-driven QR source); integration tests provide fake
//! implementations under `tests/integration/support/`.

use std::path::PathBuf;
use std::time::Duration;

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

/// Yields fresh quiescent-to-asserted edges from the motion sensor.
///
/// See `specs/002-capture-subsystem/contracts/hw-traits.md` for the full
/// contract. The production adapter
/// (`perchstation_hw::motion_sensor::GpioMotionSensor`) subscribes to
/// `gpio-cdev` rising-edge events; the in-memory test fake
/// (`tests::integration::support::fake_motion_sensor::FakeMotionSensor`)
/// pushes synthetic edges through an mpsc channel.
///
/// `next_trigger` is the only method the supervisor awaits; the future
/// MUST be safe to drop and any edge that arrived while the future was
/// being awaited MUST surface on the next call.
#[async_trait]
pub trait MotionSensor: Send + Sync {
    /// Asynchronously yield the next observed quiescent-to-asserted edge
    /// as the wall-clock time of the transition.
    async fn next_trigger(&mut self) -> Result<DateTime<Utc>, MotionSensorError>;

    /// Non-blocking read of the current high/low level.
    ///
    /// Called by the supervisor's periodic liveness tick. Errors are
    /// surfaced to the sensor-liveness tracker as "unavailable" without
    /// terminating the capture loop.
    fn level(&self) -> Result<SensorLevel, MotionSensorError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorLevel {
    Quiescent,
    Asserted,
}

#[derive(Debug, thiserror::Error)]
pub enum MotionSensorError {
    #[error("sensor unavailable: {0}")]
    Unavailable(String),
    #[error("sensor I/O error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
}

/// Records a single bounded video clip to the staging directory.
///
/// See `specs/002-capture-subsystem/contracts/hw-traits.md` for the full
/// contract. The production adapter
/// (`perchstation_hw::camera_recorder::LibcameraVidCamera`) shells out
/// to `libcamera-vid`; the in-memory test fake
/// (`tests::integration::support::fake_camera::FakeCamera`) writes a
/// fixed-size payload with selectable failure modes.
///
/// Cancellation: if the returned future is dropped before resolving the
/// implementation MUST stop the camera and remove the partial staging
/// file. The supervisor wraps the call in
/// `tokio::time::timeout(max_duration + hang_margin)` and relies on this
/// drop-cleanup path as its hang-recovery mechanism.
#[async_trait]
pub trait Camera: Send + Sync {
    /// Record a single clip of at most `max_duration`, writing a
    /// complete container-formatted file (MP4 / H.264 in production)
    /// into the staging directory the adapter was constructed with.
    ///
    /// `recording_id` is the supervisor-minted staging id from
    /// `data-model.md` §`MotionTriggerEvent` (`<capture_utc_basic>-cap`).
    /// Implementations MUST use it as the staging filename stem so the
    /// id logged in `capture.recording_started`/`completed`/`failed`
    /// matches the on-disk artefact exactly.
    async fn record_clip(
        &mut self,
        recording_id: &str,
        max_duration: Duration,
    ) -> Result<RecordedClip, CameraError>;
}

/// A successfully recorded clip staged on the local filesystem.
///
/// The capture supervisor takes ownership of the file: after
/// `Inbox::submit`, the staging path no longer exists (the queue
/// renamed or copied the bytes into `pending/`).
#[derive(Debug, Clone)]
pub struct RecordedClip {
    pub clip_path: PathBuf,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub byte_size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("camera open failed: {0}")]
    OpenFailed(String),
    #[error("camera I/O error during recording: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
    #[error("recording aborted: {0}")]
    Aborted(String),
    #[error("no media bytes were produced (camera busy or off)")]
    EmptyOutput,
}
