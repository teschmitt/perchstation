# Contract: Capture hardware traits

**Direction**: `perchstation-core` capture loop ↔ `perchstation-hw`
production adapters (and `tests/integration/support/` fakes).

**Definition site**: `crates/perchstation-core/src/hw_traits.rs`
(extends the existing `QrFrameSource` + `Clock` defined in the same
file by feature 001).

These two new traits draw the hardware-boundary line for the capture
subsystem. The platform-agnostic supervisor depends only on these
traits; the production GPIO + libcamera adapters live exclusively in
`perchstation-hw`, and integration tests provide in-memory fakes (see
`research.md` R-2, R-3, R-5; spec FR-016).

---

## Trait: `MotionSensor`

```rust
use async_trait::async_trait;
use chrono::{DateTime, Utc};

#[async_trait]
pub trait MotionSensor: Send + Sync {
    /// Asynchronously yield the next observed quiescent-to-asserted
    /// edge as the wall-clock time of the transition.
    ///
    /// Cancellation: the returned future MUST be safe to drop. An edge
    /// that arrived while the future was being awaited (or that arrived
    /// between two consecutive calls) MUST surface on the next call.
    /// Implementations satisfy this by reading from a kernel- or
    /// channel-buffered FIFO whose state survives across `next_trigger`
    /// invocations.
    ///
    /// On adapter failure resolves with `Err`. The supervisor reacts by
    /// marking the sensor `Unavailable` and continues to poll
    /// `next_trigger` so the adapter can recover.
    async fn next_trigger(&mut self) -> Result<DateTime<Utc>, MotionSensorError>;

    /// Non-blocking read of the current high/low level.
    ///
    /// Called by the supervisor's periodic liveness tick. Errors are
    /// surfaced to the [`SensorLivenessTracker`] as "unavailable"
    /// without terminating the capture loop.
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
```

### Implementations

| Implementation                                                       | Crate                  | Purpose                                              |
| -------------------------------------------------------------------- | ---------------------- | ---------------------------------------------------- |
| `perchstation_hw::motion_sensor::GpioMotionSensor`                   | `perchstation-hw`      | Production. `gpio-cdev` rising-edge subscription + level read on the configured BCM line. |
| `tests::integration::support::fake_motion_sensor::FakeMotionSensor`  | `tests/integration/`   | mpsc-driven; test code pushes synthetic `DateTime<Utc>` edges and sets the level. |

### Notes

- `next_trigger` is the only method the supervisor awaits; cancellation
  safety of this method is the foundation on which the supervisor's
  `tokio::select!` loop is built.
- `level` is `&self` (not `&mut self`) so the liveness tick can take a
  read-only borrow during the same `select!` arm.
- The production adapter MUST debounce kernel-side via the gpiochip
  configuration; software-side hysteresis lives in the supervisor's
  liveness tracker, not in the adapter.
- The adapter is constructed once at `perchstation serve` start; it
  holds a long-lived file descriptor on `/dev/gpiochip0` for the
  process lifetime.

---

## Trait: `Camera`

```rust
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::time::Duration;

#[async_trait]
pub trait Camera: Send + Sync {
    /// Record a single clip of *at most* `max_duration`, writing a
    /// complete container-formatted file (MP4 / H.264 in production)
    /// into the staging directory the adapter was constructed with.
    ///
    /// The implementation MUST stop the recording cleanly at the bound
    /// and MUST produce a *valid* container file before returning
    /// `Ok`. On any error the implementation MUST remove its staging
    /// file (or never have created it) before returning `Err`.
    ///
    /// Cancellation: if the returned future is dropped before
    /// resolving, the implementation MUST stop the camera (terminating
    /// any child process) and remove the partial staging file. The
    /// supervisor wraps this call in
    /// `tokio::time::timeout(max_duration + hang_margin)` to catch a
    /// hung adapter; the drop-cleanup path is the supervisor's
    /// hang-recovery mechanism.
    async fn record_clip(
        &mut self,
        max_duration: Duration,
    ) -> Result<RecordedClip, CameraError>;
}

/// A successfully recorded clip staged on the local filesystem.
///
/// The capture supervisor takes ownership of the file: after
/// `Inbox::submit`, the staging path no longer exists (the queue
/// renamed or copied the bytes into `pending/`).
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
```

### Implementations

| Implementation                                                       | Crate                  | Purpose                                              |
| -------------------------------------------------------------------- | ---------------------- | ---------------------------------------------------- |
| `perchstation_hw::camera_recorder::LibcameraVidCamera`               | `perchstation-hw`      | Production. Shells out to `libcamera-vid` with `--codec h264 -o staging/...mp4 --timeout <ms>`. |
| `tests::integration::support::fake_camera::FakeCamera`               | `tests/integration/`   | In-memory driver. Writes a configurable byte payload (default: 1024 bytes of `0x42`) to the staging path, with adjustable failure modes (`Mode::Ok`, `Mode::FailMidway`, `Mode::Hang`, `Mode::EmptyOutput`). |

### Notes

- The trait does **not** expose a separate `start` / `stop` /
  `abort` triple. The single async method plus drop-cancellation is the
  contract, matching the simplicity of feature 001's `QrFrameSource`.
- The supervisor MUST validate `byte_size > 0` before calling
  `Inbox::submit` (defence in depth against an adapter that
  forgets to fail on empty output). A zero-length clip is treated as
  a recording failure (`capture.recording_failed { kind: "empty_output" }`).
- Adapters MUST NOT touch any directory other than the staging directory
  they were constructed with. In particular they MUST NOT write into
  `<data_dir>/queue/` (FR-007).

---

## Constructor surfaces

These are the documented constructor surfaces of the production
adapters. The supervisor takes the trait objects, not the concrete
types, so the constructors are wiring-layer concerns described here
for the implementer-facing tasks.

```rust
// perchstation-hw::motion_sensor
impl GpioMotionSensor {
    pub fn new(
        chip_path: impl AsRef<Path>,        // e.g. "/dev/gpiochip0"
        line: u32,                          // BCM line number
        active_high: bool,                  // false for active-low wiring
    ) -> Result<Self, MotionSensorError>;
}

// perchstation-hw::camera_recorder
impl LibcameraVidCamera {
    pub fn new(
        staging_dir: impl AsRef<Path>,      // <data_dir>/capture-staging/
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_bps: u64,
    ) -> Self;
}
```

The wiring layer in `perchstation::commands::serve` reads
`config.capture.sensor_*` and `config.capture.camera_*` to construct
these, then boxes them into `Box<dyn MotionSensor>` /
`Box<dyn Camera>` and hands them to `Capture::new`.

---

## Test obligations

- `tests/integration/capture_happy.rs` exercises the trait surface
  end-to-end with `FakeMotionSensor` + `FakeCamera`, asserting the
  staged clip lands in `<data_dir>/queue/pending/` via the existing
  `StoreInbox::submit` path.
- `tests/integration/capture_recording_failure.rs` exercises
  `FakeCamera::Mode::FailMidway`, asserting the staging file is removed
  and that no clip enters the queue.
- `tests/integration/capture_bounded_clip.rs` exercises
  `FakeCamera::Mode::Hang` to verify the supervisor's outer
  `tokio::time::timeout` catches a hung adapter (the drop on the
  `record_clip` future triggers the `FakeCamera`'s cleanup branch).
- `tests/integration/capture_unavailable_sensor.rs` exercises
  `FakeMotionSensor::set_error(...)` to drive `MotionSensorError::Unavailable`
  through both `next_trigger` and `level`, asserting the supervisor
  transitions through the liveness state machine and recovers.

---

## Versioning

The trait surfaces above are the capture subsystem's hardware-boundary
contract. Adding new methods to either trait is a breaking change
inside the workspace and requires the corresponding fake updated and
all integration tests reviewed. The error variants are an
`#[non_exhaustive]`-like contract in practice: callers must handle the
documented variants exhaustively, but the supervisor's logging path
maps unknown variants to a generic `kind: "unknown"` so adding a
variant remains a minor compatibility change.
