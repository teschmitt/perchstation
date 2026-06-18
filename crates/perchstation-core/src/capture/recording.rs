//! Bound-duration record-and-stage helper (FR-005, FR-008).
//!
//! Wraps [`Camera::record_clip`] in a `tokio::time::timeout` that is
//! `clip_duration + hang_margin` long, so a hung adapter cannot pin the
//! supervisor. On any error path — including the outer timeout, which
//! drops the future and triggers the adapter's drop-cancellation cleanup
//! — the staging file is removed if it survived the failure (defence in
//! depth against an adapter that forgets to honour the trait's "remove
//! on error" rule).
//!
//! Returns a [`CaptureRecordError`] that the supervisor matches on to
//! decide between `capture.recording_failed` (adapter-level failure) and
//! `capture.recording_hung` (outer timeout fired).

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use thiserror::Error;

use crate::hw_traits::{Camera, CameraError, RecordedClip};

/// Failure modes the supervisor needs to distinguish.
#[derive(Debug, Error)]
pub enum CaptureRecordError {
    /// `Camera::record_clip` returned `Err`. Carries the underlying
    /// adapter error for logging.
    #[error("camera recording failed: {0}")]
    Failed(#[source] CameraError),
    /// The outer `tokio::time::timeout(clip_duration + hang_margin)`
    /// fired. Drop-cancellation runs the adapter's cleanup path.
    #[error("camera adapter exceeded clip_duration + hang_margin")]
    Timeout,
    /// The adapter returned `Ok` but the staging file was empty. Treated
    /// identically to a recording failure (defence in depth — the trait
    /// promises to emit `CameraError::EmptyOutput` on this path, but the
    /// supervisor double-checks the on-disk byte count).
    #[error("camera produced a zero-length clip at `{path}`")]
    EmptyClip { path: std::path::PathBuf },
    /// `clip_duration + hang_margin` overflowed the maximum representable
    /// [`Duration`]. Defence in depth — `Config::validate` already rejects
    /// such a config, so this yields a clean error rather than a panic.
    #[error("clip_duration + hang_margin overflows the maximum representable duration")]
    DurationOverflow,
}

/// Record one bounded clip and return a [`RecordedClip`] on success.
///
/// `recording_id` is the supervisor-minted staging id and is threaded
/// through to the adapter so the on-disk filename matches the id logged
/// in `capture.recording_started`. `staging_dir` is used only for the
/// post-error cleanup defence; the adapter's return value is the
/// authoritative clip path on success.
pub async fn record_into_staging(
    camera: &mut dyn Camera,
    recording_id: &str,
    staging_dir: &Path,
    max_duration: Duration,
    hang_margin: Duration,
) -> Result<RecordedClip, CaptureRecordError> {
    let Some(outer) = max_duration.checked_add(hang_margin) else {
        return Err(CaptureRecordError::DurationOverflow);
    };
    let result = tokio::time::timeout(outer, camera.record_clip(recording_id, max_duration)).await;
    match result {
        Ok(Ok(clip)) => {
            if clip.byte_size == 0 {
                let _ = remove_if_exists(&clip.clip_path);
                return Err(CaptureRecordError::EmptyClip { path: clip.clip_path });
            }
            Ok(clip)
        }
        Ok(Err(err)) => {
            sweep_staging(staging_dir);
            Err(CaptureRecordError::Failed(err))
        }
        Err(_elapsed) => {
            // The future was dropped, which runs the adapter's drop-clean
            // path. Sweep staging defensively in case the adapter did not
            // clean up before the timeout fired.
            sweep_staging(staging_dir);
            Err(CaptureRecordError::Timeout)
        }
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Defensively remove any file left under `staging_dir`. The adapter
/// contract says it cleans up its own staging file on error; this is
/// belt-and-braces against a faulty adapter (or against a future timeout
/// firing before the adapter's drop-guard has finished cleaning up).
fn sweep_staging(staging_dir: &Path) {
    let Ok(read) = fs::read_dir(staging_dir) else { return };
    for entry in read.flatten() {
        let _ = fs::remove_file(entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    struct OkCamera {
        staging: PathBuf,
        payload: Vec<u8>,
        sleep_for: Duration,
    }

    #[async_trait]
    impl Camera for OkCamera {
        async fn record_clip(
            &mut self,
            recording_id: &str,
            _max_duration: Duration,
        ) -> Result<RecordedClip, CameraError> {
            let path = self.staging.join(format!("{recording_id}.mp4"));
            tokio::fs::write(&path, &self.payload)
                .await
                .map_err(|source| CameraError::Io { source })?;
            tokio::time::sleep(self.sleep_for).await;
            Ok(RecordedClip {
                clip_path: path,
                started_at: Utc::now(),
                ended_at: Utc::now(),
                byte_size: self.payload.len() as u64,
            })
        }
    }

    struct FailingCamera {
        staging: PathBuf,
    }

    #[async_trait]
    impl Camera for FailingCamera {
        async fn record_clip(
            &mut self,
            recording_id: &str,
            _max_duration: Duration,
        ) -> Result<RecordedClip, CameraError> {
            // Write a partial file to simulate a failed adapter that
            // forgot to clean up.
            let leftover = self.staging.join(format!("{recording_id}.mp4"));
            tokio::fs::write(&leftover, b"partial")
                .await
                .map_err(|source| CameraError::Io { source })?;
            Err(CameraError::Aborted("simulated".into()))
        }
    }

    struct HangCamera {
        staging: PathBuf,
        observed_drop: std::sync::Arc<Mutex<bool>>,
    }

    struct DropTrip(std::sync::Arc<Mutex<bool>>);
    impl Drop for DropTrip {
        fn drop(&mut self) {
            if let Ok(mut g) = self.0.try_lock() {
                *g = true;
            }
        }
    }

    #[async_trait]
    impl Camera for HangCamera {
        async fn record_clip(
            &mut self,
            recording_id: &str,
            _max_duration: Duration,
        ) -> Result<RecordedClip, CameraError> {
            let partial = self.staging.join(format!("{recording_id}.mp4"));
            tokio::fs::write(&partial, b"partial")
                .await
                .map_err(|source| CameraError::Io { source })?;
            // Drop-aware sentinel: the test asserts the future was
            // dropped (which corresponds to drop-cancellation).
            let _trip = DropTrip(self.observed_drop.clone());
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    struct EmptyCamera {
        staging: PathBuf,
    }

    #[async_trait]
    impl Camera for EmptyCamera {
        async fn record_clip(
            &mut self,
            recording_id: &str,
            _max_duration: Duration,
        ) -> Result<RecordedClip, CameraError> {
            let path = self.staging.join(format!("{recording_id}.mp4"));
            tokio::fs::write(&path, b"").await.map_err(|source| CameraError::Io { source })?;
            Ok(RecordedClip {
                clip_path: path,
                started_at: Utc::now(),
                ended_at: Utc::now(),
                byte_size: 0,
            })
        }
    }

    #[tokio::test]
    async fn ok_returns_clip() {
        let dir = TempDir::new().unwrap();
        let mut cam = OkCamera {
            staging: dir.path().to_path_buf(),
            payload: vec![0x42; 1024],
            sleep_for: Duration::from_millis(10),
        };
        let clip = record_into_staging(
            &mut cam,
            "20260528T142312Z-cap",
            dir.path(),
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .await
        .expect("ok");
        assert_eq!(clip.byte_size, 1024);
        assert!(clip.clip_path.is_file());
        assert_eq!(
            clip.clip_path.file_name().and_then(|s| s.to_str()),
            Some("20260528T142312Z-cap.mp4")
        );
    }

    #[tokio::test]
    async fn failed_sweeps_staging_and_returns_failed() {
        let dir = TempDir::new().unwrap();
        let mut cam = FailingCamera { staging: dir.path().to_path_buf() };
        let err = record_into_staging(
            &mut cam,
            "20260528T142312Z-cap",
            dir.path(),
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .await
        .expect_err("failed");
        assert!(matches!(err, CaptureRecordError::Failed(_)));
        // Staging was swept defensively.
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().filter_map(Result::ok).collect();
        assert!(leftovers.is_empty(), "staging should be empty; got {leftovers:?}");
    }

    #[tokio::test]
    async fn timeout_drops_camera_future_and_sweeps_staging() {
        let dir = TempDir::new().unwrap();
        let observed = std::sync::Arc::new(Mutex::new(false));
        let mut cam =
            HangCamera { staging: dir.path().to_path_buf(), observed_drop: observed.clone() };
        let err = record_into_staging(
            &mut cam,
            "20260528T142312Z-cap",
            dir.path(),
            Duration::from_millis(10),
            Duration::from_millis(10),
        )
        .await
        .expect_err("timeout");
        assert!(matches!(err, CaptureRecordError::Timeout));
        // give the drop trip a moment to register
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(*observed.lock().await, "camera future must be dropped on timeout");
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().filter_map(Result::ok).collect();
        assert!(leftovers.is_empty(), "staging should be empty; got {leftovers:?}");
    }

    #[tokio::test]
    async fn overflowing_outer_duration_is_clean_error_not_panic() {
        let dir = TempDir::new().unwrap();
        let mut cam = OkCamera {
            staging: dir.path().to_path_buf(),
            payload: vec![0x42; 16],
            sleep_for: Duration::from_millis(0),
        };
        // `clip_duration + hang_margin` overflows `Duration`; the helper
        // must return a clean error instead of panicking on the add.
        let err = record_into_staging(
            &mut cam,
            "20260528T142312Z-cap",
            dir.path(),
            Duration::MAX,
            Duration::from_secs(1),
        )
        .await
        .expect_err("overflow");
        assert!(matches!(err, CaptureRecordError::DurationOverflow));
    }

    #[tokio::test]
    async fn empty_clip_is_rejected_after_ok() {
        let dir = TempDir::new().unwrap();
        let mut cam = EmptyCamera { staging: dir.path().to_path_buf() };
        let err = record_into_staging(
            &mut cam,
            "20260528T142312Z-cap",
            dir.path(),
            Duration::from_millis(10),
            Duration::from_millis(10),
        )
        .await
        .expect_err("empty");
        assert!(matches!(err, CaptureRecordError::EmptyClip { .. }));
    }
}
