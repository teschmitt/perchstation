//! Mutable in-process holder for the capture-side projection rendered by
//! `perchstation status` (FR-015, data-model.md §`CaptureSnapshot`).
//!
//! Writers see [`CaptureState`] (an `Arc<RwLock<CaptureStateInner>>`);
//! readers see [`crate::observability::status::CaptureSnapshot`], cloned
//! out under a read-lock by [`CaptureState::snapshot`].

use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};

use crate::observability::status::{
    CaptureFailureSnapshot, CaptureLivenessSnapshot, CaptureSnapshot,
};

#[derive(Debug, Default)]
struct CaptureStateInner {
    last_recording_at: Option<DateTime<Utc>>,
    last_clip_id: Option<String>,
    last_failure: Option<CaptureFailureSnapshot>,
    sensor_liveness: CaptureLivenessSnapshot,
    sensor_degraded_since: Option<DateTime<Utc>>,
}

impl CaptureStateInner {
    fn snapshot(&self) -> CaptureSnapshot {
        CaptureSnapshot {
            last_recording_at: self.last_recording_at,
            last_clip_id: self.last_clip_id.clone(),
            last_failure: self.last_failure.clone(),
            sensor_liveness: self.sensor_liveness,
            sensor_degraded_since: self.sensor_degraded_since,
        }
    }
}

/// Process-local capture-side state. Cheap to `Clone` (an `Arc`).
///
/// Writers (the [`crate::capture::runner::Capture`] supervisor) use
/// `record_success`, `record_failure`, and `set_liveness`. Readers
/// (`perchstation status`) call [`Self::snapshot`] to obtain an
/// immutable [`CaptureSnapshot`].
#[derive(Debug, Clone, Default)]
pub struct CaptureState {
    inner: Arc<RwLock<CaptureStateInner>>,
}

impl CaptureState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current state for `perchstation status`.
    #[must_use]
    pub fn snapshot(&self) -> CaptureSnapshot {
        self.inner.read().expect("capture state lock poisoned").snapshot()
    }

    /// Record a successful submission. Clears `last_failure` so the
    /// status surface reflects "last failure was cleared by a fresh
    /// recording".
    pub fn record_success(&self, clip_id: String, at: DateTime<Utc>) {
        let mut guard = self.inner.write().expect("capture state lock poisoned");
        guard.last_recording_at = Some(at);
        guard.last_clip_id = Some(clip_id);
        guard.last_failure = None;
    }

    /// Record a capture-side failure (the higher-level kind enumerated in
    /// `contracts/cli.md` §JSON output).
    pub fn record_failure(&self, at: DateTime<Utc>, kind: &str, message: String) {
        let mut guard = self.inner.write().expect("capture state lock poisoned");
        guard.last_failure = Some(CaptureFailureSnapshot { at, kind: kind.to_string(), message });
    }

    /// Update the sensor liveness projection. `since` is `Some` for the
    /// two degraded variants and `None` for `Healthy` / `NeverObserved`.
    pub fn set_liveness(&self, snapshot: CaptureLivenessSnapshot, since: Option<DateTime<Utc>>) {
        let mut guard = self.inner.write().expect("capture state lock poisoned");
        guard.sensor_liveness = snapshot;
        guard.sensor_degraded_since = since;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn new_state_projects_never_observed_with_no_data() {
        let state = CaptureState::new();
        let snap = state.snapshot();
        assert!(snap.last_recording_at.is_none());
        assert!(snap.last_clip_id.is_none());
        assert!(snap.last_failure.is_none());
        assert_eq!(snap.sensor_liveness, CaptureLivenessSnapshot::NeverObserved);
        assert!(snap.sensor_degraded_since.is_none());
    }

    #[test]
    fn record_success_clears_last_failure() {
        let state = CaptureState::new();
        state.record_failure(t("2026-05-27T12:00:00Z"), "recording_failed", "io".into());
        assert!(state.snapshot().last_failure.is_some());
        state.record_success("20260527T120100Z-001".into(), t("2026-05-27T12:01:00Z"));
        let snap = state.snapshot();
        assert_eq!(snap.last_clip_id.as_deref(), Some("20260527T120100Z-001"));
        assert!(snap.last_failure.is_none());
    }

    #[test]
    fn record_failure_preserves_last_recording() {
        let state = CaptureState::new();
        state.record_success("X".into(), t("2026-05-27T12:00:00Z"));
        state.record_failure(t("2026-05-27T12:05:00Z"), "camera_hang", "10000ms".into());
        let snap = state.snapshot();
        assert_eq!(snap.last_clip_id.as_deref(), Some("X"));
        let f = snap.last_failure.as_ref().expect("failure");
        assert_eq!(f.kind, "camera_hang");
    }

    #[test]
    fn set_liveness_updates_projection_and_since() {
        let state = CaptureState::new();
        let when = Utc.with_ymd_and_hms(2026, 5, 27, 12, 0, 0).unwrap();
        state.set_liveness(CaptureLivenessSnapshot::StuckAsserted, Some(when));
        let snap = state.snapshot();
        assert_eq!(snap.sensor_liveness, CaptureLivenessSnapshot::StuckAsserted);
        assert_eq!(snap.sensor_degraded_since, Some(when));

        state.set_liveness(CaptureLivenessSnapshot::Healthy, None);
        let snap = state.snapshot();
        assert_eq!(snap.sensor_liveness, CaptureLivenessSnapshot::Healthy);
        assert!(snap.sensor_degraded_since.is_none());
    }
}
