//! Capture supervisor: the single tokio task that owns the motion sensor,
//! the camera, and the cooldown / liveness gates.
//!
//! US1 implemented the happy-path trigger → record → submit loop. US2's
//! T032 extension adds: the [`SensorLivenessTracker`]-driven liveness tick,
//! the liveness gate (refuse-to-record when degraded), the pre-record
//! disk-pressure gate, and the routing of `next_trigger` adapter errors
//! through the tracker.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use crate::capture::cooldown::{CooldownOutcome, CooldownState};
use crate::capture::liveness::{
    DegradedKind, SensorLiveness, SensorLivenessTracker, SensorLivenessTransition,
};
use crate::capture::recording::{CaptureRecordError, record_into_staging};
use crate::capture::staging::{PurgeReport, StagingDir, staging_bytes};
use crate::capture::state::CaptureState;
use crate::config::CaptureConfig;
use crate::hw_traits::{Camera, CameraError, Clock, MotionSensor, MotionSensorError};
use crate::observability::status::CaptureLivenessSnapshot;
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
    liveness: SensorLivenessTracker,
    purge_report: PurgeReport,
}

impl Capture {
    /// Construct the supervisor. The wiring layer (`perchstation serve`)
    /// owns the lifetime of every input.
    ///
    /// The startup staging-purge is the wiring layer's responsibility;
    /// pass its outcome via [`Capture::with_purge_report`] so the count
    /// surfaces on `capture.ready`. When absent (most tests), the
    /// `staging_purged_files` field defaults to `0`.
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
        let liveness = SensorLivenessTracker::new(config.liveness_stuck_secs);
        Self {
            sensor,
            camera,
            inbox,
            state,
            clock,
            config,
            staging,
            cooldown: CooldownState::new(),
            liveness,
            purge_report: PurgeReport::default(),
        }
    }

    /// Attach the report produced by the wiring layer's pre-`service.ready`
    /// staging purge. The count is echoed on `capture.ready` so an
    /// operator can correlate the visible info-level event with the
    /// detailed `capture.staging_purged` debug-level event.
    #[must_use]
    pub fn with_purge_report(mut self, report: PurgeReport) -> Self {
        self.purge_report = report;
        self
    }

    /// The `Arc<CaptureState>` handed in at construction. Useful for
    /// wiring `perchstation status` to the same projection the runner
    /// updates.
    #[must_use]
    pub fn state(&self) -> Arc<CaptureState> {
        self.state.clone()
    }

    pub(super) fn purge_report(&self) -> PurgeReport {
        self.purge_report
    }

    pub(super) fn staging_path(&self) -> &std::path::Path {
        self.staging.as_path()
    }

    /// Run the supervisor until `shutdown` fires.
    ///
    /// The `tokio::select!` loop arms three branches:
    /// - `MotionSensor::next_trigger` — yields the next quiescent-to-
    ///   asserted edge (cancellation-safe by the trait's contract).
    /// - A liveness-tick `tokio::time::interval` — fires every
    ///   `liveness_poll_secs`, drives [`Self::update_liveness`].
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
                    self.update_liveness();
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

    /// Probe the sensor level and feed the result into the
    /// [`SensorLivenessTracker`]. Emits any resulting transition events
    /// and updates the [`CaptureState`] projection.
    fn update_liveness(&mut self) {
        let now = self.clock.now();
        let result = self.sensor.level();
        let transition = self.liveness.observe_level(now, result);
        self.apply_transition(transition);
    }

    fn apply_transition(&mut self, transition: SensorLivenessTransition) {
        match transition {
            SensorLivenessTransition::NoChange => {}
            SensorLivenessTransition::Degraded { kind, since, reason } => {
                Self::emit_degraded(kind, since, reason.as_deref());
                let snap = match kind {
                    DegradedKind::StuckAsserted => CaptureLivenessSnapshot::StuckAsserted,
                    DegradedKind::Unavailable => CaptureLivenessSnapshot::Unavailable,
                };
                self.state.set_liveness(snap, Some(since));
            }
            SensorLivenessTransition::Recovered { kind } => {
                Self::emit_recovered(kind);
                self.state.set_liveness(CaptureLivenessSnapshot::Healthy, None);
            }
        }
    }

    fn emit_degraded(kind: DegradedKind, since: chrono::DateTime<Utc>, reason: Option<&str>) {
        let since_str = since.to_rfc3339();
        match reason {
            Some(r) => {
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_SENSOR_DEGRADED,
                    kind = kind.as_str(),
                    since = %since_str,
                    reason = r,
                    "sensor liveness degraded",
                );
            }
            None => {
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_SENSOR_DEGRADED,
                    kind = kind.as_str(),
                    since = %since_str,
                    "sensor liveness degraded",
                );
            }
        }
    }

    fn emit_recovered(kind: DegradedKind) {
        tracing::info!(
            event = obs_tracing::events::CAPTURE_SENSOR_RECOVERED,
            kind = kind.as_str(),
            "sensor liveness recovered",
        );
    }

    /// Handle one resolved `next_trigger` future.
    async fn handle_trigger(&mut self, trigger: Result<chrono::DateTime<Utc>, MotionSensorError>) {
        let triggered_at = match trigger {
            Ok(at) => at,
            Err(err) => {
                let now = self.clock.now();
                let transition = self.liveness.observe_trigger_error(now, &err);
                self.apply_transition(transition);
                return;
            }
        };

        tracing::debug!(
            event = obs_tracing::events::CAPTURE_TRIGGER_OBSERVED,
            triggered_at = %triggered_at.to_rfc3339(),
            "motion trigger observed",
        );

        let now = self.clock.now();

        // Cooldown gate. A sustained-asserted sensor or a recent failure
        // can produce a fresh edge while we are still inside the
        // cooldown window; the gate short-circuits the loop.
        if self.cooldown.is_active(now) {
            let until = self.cooldown.until().expect("is_active implies Some(until)");
            tracing::debug!(
                event = obs_tracing::events::CAPTURE_COOLDOWN_SKIP,
                cooldown_until = %until.to_rfc3339(),
                "trigger arrived during cooldown",
            );
            return;
        }

        // Liveness gate. While the sensor is degraded, the supervisor
        // refuses to record (US2 #3, US2 #4).
        if self.liveness.is_degraded() {
            let snapshot = sensor_liveness_label(self.liveness.state());
            tracing::warn!(
                event = obs_tracing::events::CAPTURE_DEGRADED_SKIP,
                sensor_liveness = snapshot,
                "trigger arrived while sensor is degraded",
            );
            self.start_cooldown(CooldownOutcome::DegradedSkip);
            return;
        }

        // Disk-pressure gate. The pre-record check (FR-013) refuses to
        // start a recording when the staging directory's current
        // footprint already exceeds the configured ceiling.
        match staging_bytes(self.staging.as_path()) {
            Ok(bytes) if bytes >= self.config.max_staging_bytes => {
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_DISK_PRESSURE_SKIP,
                    staging_bytes = bytes,
                    max_staging_bytes = self.config.max_staging_bytes,
                    "trigger refused: staging-side disk pressure",
                );
                self.state.record_failure(
                    self.clock.now(),
                    "disk_pressure",
                    format!("{}/{} bytes", bytes, self.config.max_staging_bytes),
                );
                self.start_cooldown(CooldownOutcome::DiskPressureSkip);
                return;
            }
            Ok(_) => {}
            Err(err) => {
                // Fall through to recording; let the camera adapter
                // surface the I/O error. A failed staging_bytes call
                // does not by itself justify refusing to record.
                tracing::warn!(
                    event = obs_tracing::events::CAPTURE_STAGING_PROBE_FAILED,
                    error = %err,
                    "staging_bytes probe failed; continuing with recording attempt",
                );
            }
        }

        self.record_and_handle(triggered_at).await;
    }

    async fn record_and_handle(&mut self, triggered_at: chrono::DateTime<Utc>) {
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
            &recording_id,
            self.staging.as_path(),
            max_duration,
            hang_margin,
        )
        .await;

        match record_result {
            Ok(clip) => self.submit_clip(&recording_id, triggered_at, record_start, clip).await,
            Err(CaptureRecordError::Failed(err)) => {
                self.handle_recording_failed(&recording_id, &err);
            }
            Err(CaptureRecordError::EmptyClip { path }) => {
                self.handle_recording_empty(&recording_id, &path);
            }
            Err(CaptureRecordError::Timeout) => {
                self.handle_recording_hung(&recording_id, max_duration);
            }
        }
    }

    fn handle_recording_failed(&mut self, recording_id: &str, err: &CameraError) {
        tracing::warn!(
            event = obs_tracing::events::CAPTURE_RECORDING_FAILED,
            recording_id,
            kind = camera_error_kind(err),
            message = %err,
            "capture recording failed",
        );
        self.state.record_failure(self.clock.now(), "recording_failed", err.to_string());
        self.start_cooldown(CooldownOutcome::Failed);
    }

    fn handle_recording_empty(&mut self, recording_id: &str, path: &std::path::Path) {
        tracing::warn!(
            event = obs_tracing::events::CAPTURE_RECORDING_FAILED,
            recording_id,
            kind = "empty_output",
            path = %path.display(),
            "capture recording produced no bytes",
        );
        self.state.record_failure(self.clock.now(), "recording_failed", "empty clip".to_string());
        self.start_cooldown(CooldownOutcome::Failed);
    }

    fn handle_recording_hung(&mut self, recording_id: &str, max_duration: Duration) {
        let max_duration_ms = i64::try_from(max_duration.as_millis()).unwrap_or(i64::MAX);
        tracing::error!(
            event = obs_tracing::events::CAPTURE_RECORDING_HUNG,
            recording_id,
            max_duration_ms,
            "camera adapter hung past clip duration + hang margin",
        );
        self.state.record_failure(self.clock.now(), "camera_hang", format!("{max_duration_ms} ms"));
        self.start_cooldown(CooldownOutcome::Failed);
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

/// Render the supervisor's current liveness state as the string the
/// `capture.degraded_skip` event emits. Uses the `snake_case` wire form
/// from `contracts/cli.md` §JSON output.
const fn sensor_liveness_label(state: &SensorLiveness) -> &'static str {
    match state {
        SensorLiveness::Healthy => "healthy",
        SensorLiveness::StuckAsserted { .. } => "stuck_asserted",
        SensorLiveness::Unavailable { .. } => "unavailable",
    }
}
