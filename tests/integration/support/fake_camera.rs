//! In-memory [`Camera`] for integration tests.
//!
//! Contract: `specs/002-capture-subsystem/contracts/hw-traits.md`
//! §Implementations.
//!
//! The fake writes a fixed-size byte payload (default: 1024 bytes of
//! `0x42`) to a deterministic staging path and exposes four selectable
//! modes mapping to the documented failure modes:
//!
//! - [`Mode::Ok`] — sleeps for `max_duration` (the inner clip bound),
//!   then returns `Ok` with the staging path. Used by the happy-path
//!   tests (`capture_happy`, `capture_bounded_clip`) and the concurrent-
//!   trigger test (T029b).
//! - [`Mode::FailMidway`] — writes a partial payload, then returns
//!   `Err(CameraError::Aborted)` after a brief sleep. Used by the
//!   recording-failure test (`capture_recording_failure`).
//! - [`Mode::Hang`] — writes a partial payload, then never resolves.
//!   The supervisor's outer `tokio::time::timeout(max_duration +
//!   hang_margin)` drops the future; the staging file is removed by
//!   the drop guard. Used by `capture_camera_hang`.
//! - [`Mode::EmptyOutput`] — returns `Err(CameraError::EmptyOutput)`
//!   immediately and creates no file. Used by tests that want to
//!   exercise the supervisor's defensive `byte_size > 0` check.
//!
//! In every error path (or if the future is cancelled by the
//! supervisor's outer timeout), the staging file MUST NOT survive —
//! the on-disk invariant the spec calls out in FR-008.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use perchstation_core::hw_traits::{Camera, CameraError, RecordedClip};

const DEFAULT_PAYLOAD_BYTE: u8 = 0x42;
const DEFAULT_PAYLOAD_LEN: usize = 1024;
const FAIL_MIDWAY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ok,
    FailMidway,
    Hang,
    EmptyOutput,
}

#[derive(Default)]
struct LastInvocation {
    recording_id: Option<String>,
    clip_path: Option<PathBuf>,
}

pub struct FakeCamera {
    staging_dir: PathBuf,
    mode: Arc<Mutex<Mode>>,
    payload: Vec<u8>,
    last: Arc<Mutex<LastInvocation>>,
}

/// Cloneable handle so test code can mutate the fake's mode while the
/// supervisor task owns the [`FakeCamera`] by `&mut self`, and read
/// back what `record_clip` was last invoked with.
#[derive(Clone)]
pub struct FakeCameraHandle {
    mode: Arc<Mutex<Mode>>,
    last: Arc<Mutex<LastInvocation>>,
}

impl FakeCamera {
    #[must_use]
    pub fn new(staging_dir: impl Into<PathBuf>) -> Self {
        Self {
            staging_dir: staging_dir.into(),
            mode: Arc::new(Mutex::new(Mode::Ok)),
            payload: vec![DEFAULT_PAYLOAD_BYTE; DEFAULT_PAYLOAD_LEN],
            last: Arc::new(Mutex::new(LastInvocation::default())),
        }
    }

    #[must_use]
    pub fn with_mode(self, mode: Mode) -> Self {
        *self.mode.lock().expect("fake camera mutex poisoned") = mode;
        self
    }

    #[must_use]
    pub fn handle(&self) -> FakeCameraHandle {
        FakeCameraHandle { mode: self.mode.clone(), last: self.last.clone() }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.handle().set_mode(mode);
    }
}

impl FakeCameraHandle {
    pub fn set_mode(&self, mode: Mode) {
        *self.mode.lock().expect("fake camera mutex poisoned") = mode;
    }

    /// The `recording_id` `Camera::record_clip` was most recently
    /// invoked with, or `None` if the camera has never been called.
    #[must_use]
    pub fn last_recording_id(&self) -> Option<String> {
        self.last.lock().expect("fake camera mutex poisoned").recording_id.clone()
    }

    /// The staging file path the fake constructed on its most recent
    /// invocation, or `None` if the camera has never been called.
    #[must_use]
    pub fn last_clip_path(&self) -> Option<PathBuf> {
        self.last.lock().expect("fake camera mutex poisoned").clip_path.clone()
    }
}

/// RAII guard that removes a staging file on drop unless explicitly
/// disarmed. This is the mechanism by which `Mode::Hang` honours the
/// "remove the partial staging file on drop-cancellation" half of the
/// `Camera` trait's cancellation contract.
struct StagingGuard {
    path: PathBuf,
    keep: bool,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn disarm(mut self) {
        self.keep = true;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn ensure_staging_dir(staging_dir: &Path) -> Result<(), CameraError> {
    std::fs::create_dir_all(staging_dir).map_err(|source| CameraError::Io { source })
}

#[async_trait]
impl Camera for FakeCamera {
    async fn record_clip(
        &mut self,
        recording_id: &str,
        max_duration: Duration,
    ) -> Result<RecordedClip, CameraError> {
        let mode = *self.mode.lock().expect("fake camera mutex poisoned");
        let started_at = Utc::now();
        let staging_path = self.staging_dir.join(format!("{recording_id}.mp4"));

        {
            let mut last = self.last.lock().expect("fake camera mutex poisoned");
            last.recording_id = Some(recording_id.to_string());
            last.clip_path = Some(staging_path.clone());
        }

        if matches!(mode, Mode::EmptyOutput) {
            // No file created — already compliant with "remove or never create" on error.
            return Err(CameraError::EmptyOutput);
        }

        ensure_staging_dir(&self.staging_dir)?;
        let guard = StagingGuard::new(staging_path.clone());

        match mode {
            Mode::Ok => {
                tokio::fs::write(&staging_path, &self.payload)
                    .await
                    .map_err(|source| CameraError::Io { source })?;
                tokio::time::sleep(max_duration).await;
                let ended_at = Utc::now();
                let byte_size = self.payload.len() as u64;
                guard.disarm();
                Ok(RecordedClip { clip_path: staging_path, started_at, ended_at, byte_size })
            }
            Mode::FailMidway => {
                let half = self.payload.len() / 2;
                tokio::fs::write(&staging_path, &self.payload[..half])
                    .await
                    .map_err(|source| CameraError::Io { source })?;
                tokio::time::sleep(FAIL_MIDWAY_DELAY).await;
                drop(guard);
                Err(CameraError::Aborted("simulated mid-recording failure".into()))
            }
            Mode::Hang => {
                tokio::fs::write(&staging_path, &self.payload)
                    .await
                    .map_err(|source| CameraError::Io { source })?;
                // Never resolves; the supervisor's outer timeout drops
                // this future, which runs `guard`'s `Drop` and removes
                // the staging file.
                std::future::pending::<()>().await;
                unreachable!("std::future::pending never resolves")
            }
            Mode::EmptyOutput => unreachable!("EmptyOutput handled above"),
        }
    }
}
