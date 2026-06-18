//! Retry / backoff scheduler (T045).
//!
//! Pure-function scheduler that, given a [`Clock`] and the entry's
//! attempt history, decides whether the next retry should fire (and
//! when) or whether the retry budget is exhausted. Consumed by the
//! delivery runner (T046) and the classify-task poller (T052).
//!
//! Schedule (per `research.md` R-7):
//!
//! - exponential growth: `delay = initial * multiplier^(attempt - 1)`,
//!   capped at `max_attempt_delay`;
//! - ±20 % jitter applied to the capped delay;
//! - `per_clip_max_attempts` caps the attempt count;
//! - `per_clip_max_wallclock` caps the total time since first attempt;
//! - any caller-supplied `retry_after_floor` (e.g., the `Retry-After`
//!   header on 429) acts as a floor on the chosen delay.
//!
//! Jitter source: a `nanoseconds`-based crude PRNG. Tests pin
//! `jitter_fraction = 0.0` for deterministic delays; production gets
//! enough entropy from the wall-clock variation between retries.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::config::RetryConfig;
use crate::hw_traits::Clock;
use crate::perchpub::client::ClientError;

/// Default exponential growth factor (per R-7 — not operator-configurable).
pub const DEFAULT_MULTIPLIER: f64 = 2.0;
/// Default jitter envelope as a fraction of the base delay (per R-7 —
/// not operator-configurable).
pub const DEFAULT_JITTER_FRACTION: f64 = 0.2;

/// Absolute ceiling (seconds) applied to a single computed backoff delay
/// before [`Duration::from_secs_f64`]. Guards against a pathological
/// (unvalidated) `max_attempt_delay` whose `as_secs_f64` approaches `2^64`
/// and would otherwise panic the conversion. Far above any sane operator
/// backoff — `Config::validate` caps the configured ceiling at 7 days.
const MAX_BACKOFF_SECS: f64 = 1e15;

/// Concrete schedule used by the delivery runner. Build with
/// [`BackoffSchedule::from_config`] and reuse across attempts; the
/// scheduler is stateless past construction.
#[derive(Debug, Clone, Copy)]
pub struct BackoffSchedule {
    pub initial_delay: Duration,
    pub max_attempt_delay: Duration,
    pub multiplier: f64,
    pub jitter_fraction: f64,
    pub per_clip_max_attempts: u32,
    pub per_clip_max_wallclock: Duration,
}

impl BackoffSchedule {
    /// Build a schedule from the operator-facing config plus the
    /// non-configurable R-7 defaults for multiplier and jitter.
    #[must_use]
    pub fn from_config(cfg: &RetryConfig) -> Self {
        Self {
            initial_delay: Duration::from_secs(cfg.initial_delay_secs),
            max_attempt_delay: Duration::from_secs(cfg.max_attempt_delay_secs),
            multiplier: DEFAULT_MULTIPLIER,
            jitter_fraction: DEFAULT_JITTER_FRACTION,
            per_clip_max_attempts: cfg.per_clip_max_attempts,
            // Saturate rather than panic if an unvalidated config slips a
            // huge value through (defence in depth; `Config::validate`
            // caps the configured value at 1 year of hours).
            per_clip_max_wallclock: Duration::from_secs(
                cfg.per_clip_max_wallclock_hours.saturating_mul(3600),
            ),
        }
    }
}

/// What the runner should do after a transient failure.
#[derive(Debug, Clone, Copy)]
pub enum NextAction {
    /// Reschedule the entry for another attempt at this wall-clock time.
    Retry(DateTime<Utc>),
    /// Per-clip attempt count or wall-clock budget exhausted. The
    /// runner emits `delivery.attempts_exhausted` and transitions the
    /// entry to `delivered/` with `outcome: Undeliverable`.
    Exhausted,
}

/// Classification of an upload / classify-task failure per
/// `contracts/perchpub-api.md` §2 / §3. The runner consumes this to
/// decide whether to schedule another attempt or mark the clip
/// undeliverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Transient,
    Terminal,
}

/// Short stable identifier for the failure cause, persisted on
/// `ClipQueueEntry.last_error.kind` and emitted as the `kind` field
/// on `delivery.upload_transient` / `delivery.upload_terminal` events.
#[must_use]
pub fn error_kind(err: &ClientError) -> &'static str {
    match err {
        ClientError::Http { .. } => "http_status",
        ClientError::Network { .. } => "network",
        ClientError::Decode { .. } => "decode",
        ClientError::OutboundDisallowed { .. } => "outbound_disallowed",
        ClientError::ClipOpen { .. } => "clip_open",
        ClientError::CredentialIo { .. }
        | ClientError::TlsConfig(_)
        | ClientError::InvalidUrl { .. } => "config",
    }
}

/// Classify an [`ClientError`] returned by `upload_clip`. Per
/// `contracts/perchpub-api.md` §2, the transient HTTP codes are
/// 408, 425, 429, 500, 502, 503, 504; everything else 4xx is terminal.
/// Network and decode errors are transient (server may recover).
#[must_use]
pub fn classify_upload_error(err: &ClientError) -> FailureKind {
    match err {
        ClientError::Http { status, .. } => classify_status(*status),
        ClientError::Network { .. } | ClientError::Decode { .. } => FailureKind::Transient,
        ClientError::OutboundDisallowed { .. }
        | ClientError::ClipOpen { .. }
        | ClientError::CredentialIo { .. }
        | ClientError::TlsConfig(_)
        | ClientError::InvalidUrl { .. } => FailureKind::Terminal,
    }
}

/// Classify an [`ClientError`] returned by `get_classify_task`. Same
/// rules as upload — 5xx + network are transient, 4xx (including the
/// 404 / 422 documented as "task lost") are terminal — except that the
/// poller treats terminal as `classify.lost` rather than `Undeliverable`.
#[must_use]
pub fn classify_poll_error(err: &ClientError) -> FailureKind {
    classify_upload_error(err)
}

const fn classify_status(status: u16) -> FailureKind {
    match status {
        408 | 425 | 429 | 500 | 502 | 503 | 504 => FailureKind::Transient,
        _ => FailureKind::Terminal,
    }
}

impl BackoffSchedule {
    /// Compute the next action.
    ///
    /// `attempt` is the count of attempts already completed (1-indexed).
    /// `first_attempt_at` is when this entry first transitioned into
    /// `inflight/`, used for the wall-clock ceiling.
    /// `retry_after_floor` is an optional minimum delay supplied by
    /// `Retry-After` on a 429.
    #[must_use]
    pub fn schedule(
        &self,
        clock: &dyn Clock,
        attempt: u32,
        first_attempt_at: Option<DateTime<Utc>>,
        retry_after_floor: Option<Duration>,
    ) -> NextAction {
        if attempt >= self.per_clip_max_attempts {
            return NextAction::Exhausted;
        }

        let now = clock.now();
        if let Some(first) = first_attempt_at {
            let elapsed = (now - first).to_std().unwrap_or(Duration::ZERO);
            if elapsed >= self.per_clip_max_wallclock {
                return NextAction::Exhausted;
            }
        }

        let base = self.base_delay(attempt);
        let jittered = apply_jitter(base, self.jitter_fraction);
        let delay = match retry_after_floor {
            Some(floor) if floor > jittered => floor,
            _ => jittered,
        };

        let chrono_delay = chrono::Duration::from_std(delay).unwrap_or_else(|_| {
            // Saturating: if the delay overflows `i64::MAX` ms (over
            // 292 million years), the wall-clock budget will have
            // already vetoed the retry — but guard anyway.
            chrono::Duration::seconds(self.max_attempt_delay.as_secs().cast_signed())
        });
        NextAction::Retry(now + chrono_delay)
    }

    fn base_delay(&self, attempt: u32) -> Duration {
        let exp = i32::try_from(attempt.saturating_sub(1)).unwrap_or(i32::MAX);
        let base_secs = self.initial_delay.as_secs_f64() * self.multiplier.powi(exp);
        let capped = base_secs.min(self.max_attempt_delay.as_secs_f64());
        // Clamp to a finite, non-negative, representable value so a
        // pathological `max_attempt_delay` (near `u64::MAX` seconds) can
        // never push `from_secs_f64` past its panic threshold.
        let bounded =
            if capped.is_finite() { capped.clamp(0.0, MAX_BACKOFF_SECS) } else { MAX_BACKOFF_SECS };
        Duration::from_secs_f64(bounded)
    }
}

fn apply_jitter(base: Duration, jitter_fraction: f64) -> Duration {
    if jitter_fraction <= 0.0 {
        return base;
    }
    // Crude PRNG from nanosecond resolution of `Utc::now()` — coarse
    // enough that successive calls within the same nanosecond would
    // share jitter, fine enough that retries spaced by even a millisecond
    // see different values. Good enough for ±20 % jitter; if a future
    // change demands real randomness, swap to `getrandom`.
    let nanos = chrono::Utc::now().timestamp_subsec_nanos();
    let normalized = (f64::from(nanos) / 500_000_000.0) - 1.0; // [-1.0, 1.0)
    let factor = 1.0 + normalized.clamp(-1.0, 1.0) * jitter_fraction;
    Duration::from_secs_f64(base.as_secs_f64() * factor.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use chrono::TimeZone;

    struct FakeClock {
        now: Mutex<DateTime<Utc>>,
    }
    impl FakeClock {
        fn new(t: DateTime<Utc>) -> Self {
            Self { now: Mutex::new(t) }
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> DateTime<Utc> {
            *self.now.lock().unwrap()
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 27, 12, 0, 0).unwrap()
    }

    fn no_jitter_schedule() -> BackoffSchedule {
        BackoffSchedule {
            initial_delay: Duration::from_secs(10),
            max_attempt_delay: Duration::from_hours(1),
            multiplier: 2.0,
            jitter_fraction: 0.0,
            per_clip_max_attempts: 12,
            per_clip_max_wallclock: Duration::from_hours(24),
        }
    }

    #[test]
    fn from_config_picks_up_documented_defaults() {
        let cfg = RetryConfig {
            initial_delay_secs: 10,
            max_attempt_delay_secs: 3600,
            per_clip_max_attempts: 12,
            per_clip_max_wallclock_hours: 24,
        };
        let sched = BackoffSchedule::from_config(&cfg);
        assert_eq!(sched.initial_delay, Duration::from_secs(10));
        assert_eq!(sched.max_attempt_delay, Duration::from_hours(1));
        assert!((sched.multiplier - 2.0).abs() < f64::EPSILON);
        assert!((sched.jitter_fraction - 0.2).abs() < f64::EPSILON);
        assert_eq!(sched.per_clip_max_attempts, 12);
        assert_eq!(sched.per_clip_max_wallclock, Duration::from_hours(24));
    }

    #[test]
    fn exponential_growth_without_jitter() {
        let clock = FakeClock::new(fixed_now());
        let sched = no_jitter_schedule();
        for (attempt, expected_secs) in [(1, 10), (2, 20), (3, 40), (4, 80), (5, 160)] {
            match sched.schedule(&clock, attempt, Some(fixed_now()), None) {
                NextAction::Retry(next) => {
                    assert_eq!(
                        (next - fixed_now()).num_seconds(),
                        expected_secs,
                        "attempt {attempt}",
                    );
                }
                NextAction::Exhausted => panic!("attempt {attempt} unexpectedly exhausted"),
            }
        }
    }

    #[test]
    fn delay_capped_at_max_attempt_delay() {
        let clock = FakeClock::new(fixed_now());
        let mut sched = no_jitter_schedule();
        sched.max_attempt_delay = Duration::from_mins(1);
        // Attempt 4 would be 80 s uncapped; with cap 60 s, should be 60 s.
        match sched.schedule(&clock, 4, Some(fixed_now()), None) {
            NextAction::Retry(next) => {
                assert_eq!((next - fixed_now()).num_seconds(), 60);
            }
            NextAction::Exhausted => panic!("attempt 4 within cap should retry"),
        }
    }

    #[test]
    fn jitter_stays_inside_pm_20_percent() {
        let clock = FakeClock::new(fixed_now());
        let mut sched = no_jitter_schedule();
        sched.jitter_fraction = 0.2;
        // Sample many times; the nanosecond clock varies, so we get a
        // distribution. Base for attempt=1 is 10 s; envelope is [8, 12].
        for _ in 0..50 {
            match sched.schedule(&clock, 1, Some(fixed_now()), None) {
                NextAction::Retry(next) => {
                    let ms = (next - fixed_now()).num_milliseconds();
                    assert!(
                        (8_000..=12_000).contains(&ms),
                        "delay {ms} ms outside ±20 % envelope of 10 s",
                    );
                }
                NextAction::Exhausted => panic!("attempt 1 inside budget should retry"),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn attempt_ceiling_returns_exhausted() {
        let clock = FakeClock::new(fixed_now());
        let mut sched = no_jitter_schedule();
        sched.per_clip_max_attempts = 3;
        // attempt = 2 → next attempt would be the 3rd; still allowed.
        assert!(
            matches!(sched.schedule(&clock, 2, Some(fixed_now()), None), NextAction::Retry(_),)
        );
        // attempt = 3 → 3 attempts already done; we're at the ceiling.
        assert!(matches!(
            sched.schedule(&clock, 3, Some(fixed_now()), None),
            NextAction::Exhausted,
        ));
    }

    #[test]
    fn wallclock_ceiling_returns_exhausted() {
        let clock = FakeClock::new(fixed_now());
        let mut sched = no_jitter_schedule();
        sched.per_clip_max_wallclock = Duration::from_hours(1);
        let first = fixed_now() - chrono::Duration::seconds(3_700);
        assert!(matches!(sched.schedule(&clock, 1, Some(first), None), NextAction::Exhausted,));
    }

    #[test]
    fn retry_after_floor_supersedes_smaller_base_delay() {
        let clock = FakeClock::new(fixed_now());
        let sched = no_jitter_schedule();
        // initial 10 s; retry-after 30 s → 30 s wins.
        match sched.schedule(&clock, 1, Some(fixed_now()), Some(Duration::from_secs(30))) {
            NextAction::Retry(next) => {
                assert_eq!((next - fixed_now()).num_seconds(), 30);
            }
            NextAction::Exhausted => panic!("attempt 1 inside budget should retry"),
        }
        // retry-after 5 s smaller than base 10 s → base wins.
        match sched.schedule(&clock, 1, Some(fixed_now()), Some(Duration::from_secs(5))) {
            NextAction::Retry(next) => {
                assert_eq!((next - fixed_now()).num_seconds(), 10);
            }
            NextAction::Exhausted => panic!("attempt 1 inside budget should retry"),
        }
    }

    #[test]
    fn from_config_saturates_huge_wallclock() {
        let cfg = RetryConfig {
            initial_delay_secs: 10,
            max_attempt_delay_secs: 3600,
            per_clip_max_attempts: 12,
            per_clip_max_wallclock_hours: u64::MAX,
        };
        // `hours * 3600` must saturate rather than panic on overflow.
        let sched = BackoffSchedule::from_config(&cfg);
        assert_eq!(sched.per_clip_max_wallclock, Duration::from_secs(u64::MAX));
    }

    #[test]
    fn base_delay_clamps_huge_max_attempt_delay() {
        let mut sched = no_jitter_schedule();
        sched.max_attempt_delay = Duration::from_secs(u64::MAX);
        // With a near-`u64::MAX` ceiling a large attempt drives the capped
        // delay toward `2^64` seconds, which would panic `from_secs_f64`.
        // The clamp must keep it representable.
        let delay = sched.base_delay(100);
        assert!(delay > Duration::ZERO);
    }

    #[test]
    fn first_attempt_none_skips_wallclock_check() {
        let clock = FakeClock::new(fixed_now());
        let sched = no_jitter_schedule();
        // Even with wallclock = 1 s, missing first_attempt_at means the
        // ceiling isn't applied (first attempt hasn't happened yet, so
        // by definition we're inside the budget).
        let mut tweaked = sched;
        tweaked.per_clip_max_wallclock = Duration::from_secs(1);
        assert!(matches!(tweaked.schedule(&clock, 1, None, None), NextAction::Retry(_)));
    }
}
