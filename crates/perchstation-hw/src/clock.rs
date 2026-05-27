//! Production `Clock` implementation wrapping `chrono::Utc::now`.

use chrono::{DateTime, Utc};
use perchstation_core::hw_traits::Clock;

/// The only production `Clock`. Forwards to `chrono::Utc::now`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_moves_forward() {
        let clock = SystemClock;
        let a = clock.now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = clock.now();
        assert!(b >= a);
    }
}
