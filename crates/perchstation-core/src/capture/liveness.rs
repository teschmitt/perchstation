//! Sensor liveness state machine (data-model.md §`SensorLivenessTracker`,
//! research.md R-8).
//!
//! The tracker observes two input streams:
//!
//! - `MotionSensor::level()` results from the supervisor's periodic
//!   liveness tick (via [`SensorLivenessTracker::observe_level`]).
//! - `MotionSensor::next_trigger()` adapter errors (via
//!   [`SensorLivenessTracker::observe_trigger_error`]).
//!
//! Each `observe_*` call returns a [`SensorLivenessTransition`] describing
//! the change (if any). The supervisor uses the returned enum to emit the
//! `capture.sensor_degraded` / `capture.sensor_recovered` events and to
//! update the [`crate::capture::state::CaptureState`] projection.

use chrono::{DateTime, Duration, Utc};

use crate::hw_traits::{MotionSensorError, SensorLevel};

/// Liveness state. The supervisor refuses to start a recording when the
/// tracker is in either degraded variant (`is_degraded()` returns true).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SensorLiveness {
    Healthy,
    StuckAsserted { since: DateTime<Utc> },
    Unavailable { since: DateTime<Utc>, reason: String },
}

/// Kind discriminator carried on the `capture.sensor_degraded` /
/// `capture.sensor_recovered` events (`contracts/log-events.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedKind {
    StuckAsserted,
    Unavailable,
}

impl DegradedKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StuckAsserted => "stuck_asserted",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The transition observed on a single `observe_*` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SensorLivenessTransition {
    Degraded { kind: DegradedKind, since: DateTime<Utc>, reason: Option<String> },
    Recovered { kind: DegradedKind },
    NoChange,
}

/// `SensorLivenessTracker` (data-model.md §`SensorLivenessTracker`).
///
/// Internally remembers the moment the most recent run of consecutive
/// `Asserted` level observations began so the `Healthy` → `StuckAsserted`
/// transition can be detected on the first probe that crosses
/// `stuck_secs` worth of consecutive Asserted readings.
#[derive(Debug, Clone)]
pub struct SensorLivenessTracker {
    state: SensorLiveness,
    stuck_threshold: Duration,
    /// `Some(t)` while we are in Healthy and the most recent level probe
    /// was Asserted; `None` otherwise. Set on the Quiescent → Asserted
    /// transition and on tracker construction if the first probe is
    /// Asserted; cleared on any Quiescent observation or on transition
    /// out of Healthy.
    asserted_since: Option<DateTime<Utc>>,
}

impl SensorLivenessTracker {
    /// Construct a new tracker. Starts in `Healthy`. The supervisor
    /// converts this to `CaptureLivenessSnapshot::Healthy` on the first
    /// publish.
    #[must_use]
    pub fn new(stuck_secs: u64) -> Self {
        let secs = i64::try_from(stuck_secs).unwrap_or(i64::MAX);
        Self {
            state: SensorLiveness::Healthy,
            stuck_threshold: Duration::seconds(secs),
            asserted_since: None,
        }
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> &SensorLiveness {
        &self.state
    }

    /// `true` when the supervisor must refuse to record on a fresh
    /// trigger (`StuckAsserted` or `Unavailable`).
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        !matches!(self.state, SensorLiveness::Healthy)
    }

    /// Feed a `MotionSensor::level()` result into the tracker.
    pub fn observe_level(
        &mut self,
        now: DateTime<Utc>,
        result: Result<SensorLevel, MotionSensorError>,
    ) -> SensorLivenessTransition {
        match result {
            Err(err) => self.handle_error(now, &err),
            Ok(SensorLevel::Asserted) => self.handle_asserted(now),
            Ok(SensorLevel::Quiescent) => self.handle_quiescent(),
        }
    }

    /// Feed a `MotionSensor::next_trigger()` error into the tracker. A
    /// successful trigger does not need to be observed — only the error
    /// case affects liveness.
    pub fn observe_trigger_error(
        &mut self,
        now: DateTime<Utc>,
        err: &MotionSensorError,
    ) -> SensorLivenessTransition {
        self.handle_error(now, err)
    }

    fn handle_error(
        &mut self,
        now: DateTime<Utc>,
        err: &MotionSensorError,
    ) -> SensorLivenessTransition {
        if matches!(&self.state, SensorLiveness::Unavailable { .. }) {
            return SensorLivenessTransition::NoChange;
        }
        let reason = err.to_string();
        self.state = SensorLiveness::Unavailable { since: now, reason: reason.clone() };
        self.asserted_since = None;
        SensorLivenessTransition::Degraded {
            kind: DegradedKind::Unavailable,
            since: now,
            reason: Some(reason),
        }
    }

    fn handle_asserted(&mut self, now: DateTime<Utc>) -> SensorLivenessTransition {
        match &self.state {
            SensorLiveness::Unavailable { .. } => {
                self.state = SensorLiveness::Healthy;
                self.asserted_since = Some(now);
                SensorLivenessTransition::Recovered { kind: DegradedKind::Unavailable }
            }
            SensorLiveness::StuckAsserted { .. } => SensorLivenessTransition::NoChange,
            SensorLiveness::Healthy => {
                let started_at = *self.asserted_since.get_or_insert(now);
                if now - started_at >= self.stuck_threshold {
                    self.state = SensorLiveness::StuckAsserted { since: started_at };
                    SensorLivenessTransition::Degraded {
                        kind: DegradedKind::StuckAsserted,
                        since: started_at,
                        reason: None,
                    }
                } else {
                    SensorLivenessTransition::NoChange
                }
            }
        }
    }

    fn handle_quiescent(&mut self) -> SensorLivenessTransition {
        match &self.state {
            SensorLiveness::StuckAsserted { .. } => {
                self.state = SensorLiveness::Healthy;
                self.asserted_since = None;
                SensorLivenessTransition::Recovered { kind: DegradedKind::StuckAsserted }
            }
            SensorLiveness::Unavailable { .. } => {
                self.state = SensorLiveness::Healthy;
                self.asserted_since = None;
                SensorLivenessTransition::Recovered { kind: DegradedKind::Unavailable }
            }
            SensorLiveness::Healthy => {
                self.asserted_since = None;
                SensorLivenessTransition::NoChange
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn new_starts_healthy() {
        let t = SensorLivenessTracker::new(300);
        assert!(matches!(t.state(), SensorLiveness::Healthy));
        assert!(!t.is_degraded());
    }

    #[test]
    fn level_err_transitions_to_unavailable_once() {
        let mut t = SensorLivenessTracker::new(300);
        let now = at("2026-05-28T14:00:00Z");
        let err = MotionSensorError::Unavailable("boom".into());
        let r = t.observe_level(now, Err(err));
        assert!(matches!(
            r,
            SensorLivenessTransition::Degraded { kind: DegradedKind::Unavailable, .. }
        ));
        assert!(t.is_degraded());
        // Subsequent error is NoChange.
        let r2 = t.observe_level(
            now + Duration::seconds(1),
            Err(MotionSensorError::Unavailable("still bad".into())),
        );
        assert_eq!(r2, SensorLivenessTransition::NoChange);
    }

    #[test]
    fn unavailable_recovers_on_ok() {
        let mut t = SensorLivenessTracker::new(300);
        let now = at("2026-05-28T14:00:00Z");
        let _ = t.observe_level(now, Err(MotionSensorError::Unavailable("bad".into())));
        let r = t.observe_level(now + Duration::seconds(1), Ok(SensorLevel::Asserted));
        assert!(matches!(
            r,
            SensorLivenessTransition::Recovered { kind: DegradedKind::Unavailable }
        ));
        assert!(matches!(t.state(), SensorLiveness::Healthy));
    }

    #[test]
    fn healthy_does_not_degrade_until_threshold_elapses() {
        let mut t = SensorLivenessTracker::new(60);
        let start = Utc.with_ymd_and_hms(2026, 5, 28, 14, 0, 0).unwrap();
        let r1 = t.observe_level(start, Ok(SensorLevel::Asserted));
        assert_eq!(r1, SensorLivenessTransition::NoChange);
        let r2 = t.observe_level(start + Duration::seconds(59), Ok(SensorLevel::Asserted));
        assert_eq!(r2, SensorLivenessTransition::NoChange);
        let r3 = t.observe_level(start + Duration::seconds(60), Ok(SensorLevel::Asserted));
        assert!(matches!(
            r3,
            SensorLivenessTransition::Degraded {
                kind: DegradedKind::StuckAsserted,
                since: _,
                reason: None
            }
        ));
        if let SensorLiveness::StuckAsserted { since } = t.state() {
            assert_eq!(*since, start);
        } else {
            panic!("expected StuckAsserted, got {:?}", t.state());
        }
    }

    #[test]
    fn quiescent_resets_asserted_run() {
        let mut t = SensorLivenessTracker::new(60);
        let start = Utc.with_ymd_and_hms(2026, 5, 28, 14, 0, 0).unwrap();
        let _ = t.observe_level(start, Ok(SensorLevel::Asserted));
        let _ = t.observe_level(start + Duration::seconds(30), Ok(SensorLevel::Quiescent));
        // Even though we crossed 60s of wall-clock time, the run of
        // Asserted observations restarted, so no degrade yet.
        let r = t.observe_level(start + Duration::seconds(70), Ok(SensorLevel::Asserted));
        assert_eq!(r, SensorLivenessTransition::NoChange);
    }

    #[test]
    fn stuck_asserted_recovers_on_quiescent() {
        let mut t = SensorLivenessTracker::new(1);
        let start = at("2026-05-28T14:00:00Z");
        let _ = t.observe_level(start, Ok(SensorLevel::Asserted));
        let _ = t.observe_level(start + Duration::seconds(2), Ok(SensorLevel::Asserted));
        assert!(t.is_degraded());
        let r = t.observe_level(start + Duration::seconds(3), Ok(SensorLevel::Quiescent));
        assert!(matches!(
            r,
            SensorLivenessTransition::Recovered { kind: DegradedKind::StuckAsserted }
        ));
        assert!(!t.is_degraded());
    }

    #[test]
    fn observe_trigger_error_transitions_to_unavailable() {
        let mut t = SensorLivenessTracker::new(300);
        let now = at("2026-05-28T14:00:00Z");
        let err = MotionSensorError::Unavailable("trigger boom".into());
        let r = t.observe_trigger_error(now, &err);
        assert!(matches!(
            r,
            SensorLivenessTransition::Degraded { kind: DegradedKind::Unavailable, .. }
        ));
    }
}
