//! Post-recording cooldown gate (data-model.md §`CooldownState`, FR-006).
//!
//! After every handled trigger — success, failure, queue-refused, degraded
//! skip, disk-pressure skip — the supervisor starts a cooldown so a
//! sustained-asserted sensor cannot produce back-to-back recordings (US2
//! #2 in letter, FR-006 in spirit). The only state the gate carries is the
//! outstanding deadline, surfaced as `cooldown_until` on
//! `capture.cooldown_skip`.

use chrono::{DateTime, Duration, Utc};

/// Cooldown gate. Holds at most one outstanding deadline.
#[derive(Debug, Clone, Copy, Default)]
pub struct CooldownState {
    until: Option<DateTime<Utc>>,
}

impl CooldownState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `until = now + cooldown_secs`.
    pub fn start_after(&mut self, now: DateTime<Utc>, cooldown_secs: u64) {
        let secs = i64::try_from(cooldown_secs).unwrap_or(i64::MAX);
        self.until = Some(now + Duration::seconds(secs));
    }

    /// `true` while the cooldown deadline has not yet elapsed.
    #[must_use]
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.until.is_some_and(|t| t > now)
    }

    /// The deadline (if cooldown is active). Surfaced as the
    /// `cooldown_until` field on `capture.cooldown_skip`.
    #[must_use]
    pub fn until(&self) -> Option<DateTime<Utc>> {
        self.until
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
    fn new_is_inactive() {
        let c = CooldownState::new();
        assert!(!c.is_active(Utc::now()));
        assert!(c.until().is_none());
    }

    #[test]
    fn start_after_sets_deadline() {
        let mut c = CooldownState::new();
        let now = t("2026-05-27T12:00:00Z");
        c.start_after(now, 30);
        assert_eq!(c.until(), Some(now + Duration::seconds(30)));
        assert!(c.is_active(now));
        assert!(c.is_active(now + Duration::seconds(29)));
        assert!(!c.is_active(now + Duration::seconds(30)));
        assert!(!c.is_active(now + Duration::seconds(31)));
    }

    #[test]
    fn start_after_overrides_previous_deadline() {
        let mut c = CooldownState::new();
        let now = Utc.with_ymd_and_hms(2026, 5, 27, 12, 0, 0).unwrap();
        c.start_after(now, 30);
        c.start_after(now, 60);
        assert_eq!(c.until(), Some(now + Duration::seconds(60)));
    }
}
