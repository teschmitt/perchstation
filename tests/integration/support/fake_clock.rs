//! Settable [`Clock`] for tests that need to control "now" — backoff
//! scheduler verification (T044), clock-skew tolerance (T044a), and
//! cert-expiry pre-flight (T058) all depend on it.
//!
//! The US1 RED tests don't touch this — the happy-path delivery loop
//! uses the real `SystemClock`. Included here so the support module is
//! complete and US2/US3 tests can adopt it without further plumbing.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use perchstation_core::hw_traits::Clock;

pub struct FakeClock {
    now: Mutex<DateTime<Utc>>,
}

impl FakeClock {
    #[must_use]
    pub fn new(initial: DateTime<Utc>) -> Self {
        Self { now: Mutex::new(initial) }
    }

    pub fn set(&self, instant: DateTime<Utc>) {
        *self.now.lock().unwrap() = instant;
    }

    pub fn advance(&self, by: chrono::Duration) {
        let mut now = self.now.lock().unwrap();
        *now += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }
}
