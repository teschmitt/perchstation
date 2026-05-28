//! Capture supervisor: the single tokio task that owns the motion sensor,
//! the camera, and the cooldown / liveness gates.
//!
//! US1 implements the happy-path trigger → record → submit loop. US2's
//! liveness, disk-pressure, queue-refusal, and hang-recovery branches
//! extend [`Capture`] in T032 (Phase 4).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use crate::capture::cooldown::{CooldownOutcome, CooldownState};
use crate::capture::recording::{CaptureRecordError, record_into_staging};
use crate::capture::staging::StagingDir;
use crate::capture::state::CaptureState;
use crate::config::CaptureConfig;
use crate::hw_traits::{Camera, CameraError, Clock, MotionSensor, MotionSensorError};
use crate::observability::tracing as obs_tracing;
use crate::queue::InboxError;
use crate::queue::inbox::Inbox;
use crate::queue::store::ClipMeta;

/// Per-record capture-side staging id format from data-model.md
/// §`MotionTriggerEvent`: `<capture_utc_basic>-cap`. The `-cap` suffix is
/// local to the capture subsystem and never appears in the queue.
fn recording_id(at: chrono::DateTime<Utc>) -> String {
    format!("{}-cap", at.format("%Y%m%dT%H%M%SZ"))
}

/// The capture supervisor task. Constructed by [`Capture::new`] and
/// driven by `Capture::run` (re-exported from `crate::capture`).
pub struct Capture {
    sensor: Box<dyn MotionSensor>,
    camera: Box<dyn Camera>,
    inbox: Arc<dyn Inbox>,
    state: Arc<CaptureState>,
    clock: Arc<dyn Clock>,
    config: CaptureConfig,
    staging: StagingDir,
    cooldown: CooldownState,
}

impl Capture {
    /// Construct the supervisor. The wiring layer (`perchstation serve`)
    /// owns the lifetime of every input.
    #[must_use]
    pub fn new(
        sensor: Box<dyn MotionSensor>,
        camera: Box<dyn Camera>,
        inbox: Arc<dyn Inbox>,
        state: Arc<CaptureState>,
        clock: Arc<dyn Clock>,
        config: CaptureConfig,
        staging: StagingDir,
    ) -> Self {
        Self {
            sensor,
            camera,
            inbox,
            state,
            clock,
            config,
            staging,
            cooldown: CooldownState::new(),
        }
    }

    /// The `Arc<CaptureState>` handed in at construction. Useful for
    /// wiring `perchstation status` to the same projection the runner
    /// updates.
    #[must_use]
    pub fn state(&self) -> Arc<CaptureState> {
        self.state.clone()
    }

    /// Path under which staging files live. Used by [`Capture::run`] in
    /// `mod.rs` to invoke the startup purge.
    pub(super) fn staging_path(&self) -> &std::path::Path {
        self.staging.as_path()
    }

    /// Run the supervisor until `shutdown` fires.
    ///
    /// The `tokio::select!` loop arms three branches:
    /// - `MotionSensor::next_trigger` — yields the next quiescent-to-
    ///   asserted edge (cancellation-safe by the trait's contract).
    /// - A liveness-tick `tokio::time::interval` — fires every
    ///   `liveness_poll_secs`. US1 reads the tick but does nothing with
    ///   it; US2's T032 extension hooks the `SensorLivenessTracker`
    ///   probe in here.
    /// - `shutdown.cancelled()` — exits cleanly.
    pub(super) async fn run_loop(mut self, shutdown: CancellationToken) {
        let poll_secs = self.config.liveness_poll_secs.max(1);
        let mut liveness = tokio::time::interval(Duration::from_secs(poll_secs));
        // The first tick of a `tokio::time::interval` fires immediately
        // by default; advance past it so the loop only acts on real
        // intervals.
        liveness.tick().await;

        loop {
            tokio::select! {
                trigger = self.sensor.next_trigger() => {
                    self.handle_trigger(trigger).await;
                }
                _ = liveness.tick() => {
                    // US1 has no liveness work to do; US2's T032
                    // extension injects the level probe here.
                }
                () = shutdown.cancelled() => {
                    tracing::info!(
                        event = obs_tracing::events::CAPTURE_SHUTDOWN,
                        reason = "cancelled",
                        "capture supervisor shutting down",
                    );
                    return;
                }
            }
        }
    }

    /// Handle one resolved `next_trigger` future.
    async fn handle_trigger(&mut self, trigger: Result<chrono::DateTime<Utc>, MotionSensorError>) {
        let triggered_at = match trigger {
            Ok(at) => at,
            Err(err) => {
                // US1 logs the adapter error and continues; US2 (T032)
                // routes this through the `SensorLivenessTracker`.
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_SENSOR_DEGRADED,
                    kind = "unavailable",
                    reason = %err,
                    "motion sensor returned error",
                );
                return;
            }
        };

        tracing::debug!(
            event = obs_tracing::events::CAPTURE_TRIGGER_OBSERVED,
            triggered_at = %triggered_at.to_rfc3339(),
            "motion trigger observed",
        );

        let now = self.clock.now();
        if self.cooldown.is_active(now) {
            let until = self.cooldown.until().expect("is_active implies Some(until)");
            tracing::debug!(
                event = obs_tracing::events::CAPTURE_COOLDOWN_SKIP,
                cooldown_until = %until.to_rfc3339(),
                "trigger arrived during cooldown",
            );
            return;
        }

        let recording_id = recording_id(triggered_at);
        let max_duration = Duration::from_secs(self.config.clip_duration_secs);
        let hang_margin = Duration::from_secs(self.config.hang_margin_secs);

        tracing::info!(
            event = obs_tracing::events::CAPTURE_RECORDING_STARTED,
            recording_id = %recording_id,
            triggered_at = %triggered_at.to_rfc3339(),
            "starting capture recording",
        );

        let record_start = self.clock.now();
        let record_result = record_into_staging(
            self.camera.as_mut(),
            self.staging.as_path(),
            max_duration,
            hang_margin,
        )
        .await;

        match record_result {
            Ok(clip) => self.submit_clip(&recording_id, triggered_at, record_start, clip).await,
            Err(CaptureRecordError::Failed(err)) => {
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_RECORDING_FAILED,
                    recording_id = %recording_id,
                    kind = camera_error_kind(&err),
                    message = %err,
                    "capture recording failed",
                );
                self.state.record_failure(self.clock.now(), "recording_failed", err.to_string());
                self.start_cooldown(CooldownOutcome::Failed);
            }
            Err(CaptureRecordError::EmptyClip { path }) => {
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_RECORDING_FAILED,
                    recording_id = %recording_id,
                    kind = "empty_output",
                    path = %path.display(),
                    "capture recording produced no bytes",
                );
                self.state.record_failure(
                    self.clock.now(),
                    "recording_failed",
                    "empty clip".to_string(),
                );
                self.start_cooldown(CooldownOutcome::Failed);
            }
            Err(CaptureRecordError::Timeout) => {
                let max_duration_ms = i64::try_from(max_duration.as_millis()).unwrap_or(i64::MAX);
                tracing::error!(
                    event = obs_tracing::events::CAPTURE_RECORDING_HUNG,
                    recording_id = %recording_id,
                    max_duration_ms,
                    "camera adapter hung past clip duration + hang margin",
                );
                self.state.record_failure(
                    self.clock.now(),
                    "camera_hang",
                    format!("{max_duration_ms} ms"),
                );
                self.start_cooldown(CooldownOutcome::Failed);
            }
        }
    }

    async fn submit_clip(
        &mut self,
        recording_id: &str,
        triggered_at: chrono::DateTime<Utc>,
        record_start: chrono::DateTime<Utc>,
        clip: crate::hw_traits::RecordedClip,
    ) {
        let clip_path = clip.clip_path.clone();
        let byte_size = clip.byte_size;
        let meta = ClipMeta { captured_at: triggered_at };
        match self.inbox.submit(&clip_path, meta).await {
            Ok(entry) => {
                let elapsed_ms = (self.clock.now() - record_start).num_milliseconds().max(0);
                tracing::info!(
                    event = obs_tracing::events::CAPTURE_RECORDING_COMPLETED,
                    recording_id = %recording_id,
                    clip_id = %entry.clip_id,
                    byte_size,
                    duration_ms = elapsed_ms,
                    "capture recording completed",
                );
                self.state.record_success(entry.clip_id.clone(), triggered_at);
                self.start_cooldown(CooldownOutcome::Submitted);
            }
            Err(InboxError::QueueFull { current_clips, max_clips, current_bytes, max_bytes }) => {
                let _ = std::fs::remove_file(&clip_path);
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_QUEUE_REFUSED,
                    recording_id = %recording_id,
                    kind = "queue_full",
                    current_clips,
                    max_clips,
                    current_bytes,
                    max_bytes,
                    "queue refused new clip",
                );
                self.state.record_failure(
                    self.clock.now(),
                    "queue_full",
                    format!("{current_clips}/{max_clips} clips, {current_bytes}/{max_bytes} bytes"),
                );
                self.start_cooldown(CooldownOutcome::QueueRefused);
            }
            Err(InboxError::Queue(err)) => {
                let _ = std::fs::remove_file(&clip_path);
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_QUEUE_REFUSED,
                    recording_id = %recording_id,
                    kind = "queue_io",
                    error = %err,
                    "queue I/O error during submit",
                );
                self.state.record_failure(self.clock.now(), "queue_io", err.to_string());
                self.start_cooldown(CooldownOutcome::QueueRefused);
            }
        }
    }

    fn start_cooldown(&mut self, outcome: CooldownOutcome) {
        self.cooldown.start_after(self.clock.now(), self.config.cooldown_secs, outcome);
    }
}

/// Map a `CameraError` variant to the `kind` string the
/// `capture.recording_failed` event uses (per `contracts/log-events.md`).
fn camera_error_kind(err: &CameraError) -> &'static str {
    match err {
        CameraError::OpenFailed(_) => "open_failed",
        CameraError::Io { .. } => "io",
        CameraError::Aborted(_) => "aborted",
        CameraError::EmptyOutput => "empty_output",
    }
}
