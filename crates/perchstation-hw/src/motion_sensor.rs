//! Production [`MotionSensor`] backed by the Linux gpiochip character-device
//! ABI (`/dev/gpiochip0`) via the pure-Rust `gpio-cdev` crate.
//!
//! The adapter requests a single `BOTH_EDGES` event subscription on the
//! configured BCM line. `next_trigger` awaits the kernel-buffered event
//! stream (cancellation-safe), filters for the asserted-side edge
//! (rising for active-high wiring, falling for active-low), and returns
//! the wall-clock time of the transition. `level` reads a cached value
//! kept up to date by the event loop — `gpio-cdev` 0.6's
//! `AsyncLineEventHandle` does not expose the inner `LineEventHandle`'s
//! `get_value` directly, so we cache the level on the side. The cache
//! is seeded once at construction by reading the line value through the
//! sync handle before it is wrapped, so the first `level()` call
//! reflects the actual initial line state.
//!
//! Cfg-gated to `target_os = "linux"` because gpiochip only exists on
//! Linux; integration tests use the fake under
//! `tests/integration/support/fake_motion_sensor.rs`.

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use gpio_cdev::{AsyncLineEventHandle, Chip, EventRequestFlags, EventType, LineRequestFlags};
use perchstation_core::hw_traits::{MotionSensor, MotionSensorError, SensorLevel};

const CONSUMER: &str = "perchstation-motion";

const LEVEL_QUIESCENT: u8 = 0;
const LEVEL_ASSERTED: u8 = 1;

pub struct GpioMotionSensor {
    /// `true` when the wired sensor's "asserted" state corresponds to a
    /// logical high level on the GPIO line; `false` for active-low.
    active_high: bool,
    /// Async stream of edge events from the kernel-buffered gpiochip
    /// FIFO. Cancellation-safe by construction.
    events: AsyncLineEventHandle,
    /// Logical level cache (Asserted / Quiescent). Seeded at construction
    /// by reading the line synchronously; updated by every observed edge
    /// on the way through `next_trigger`.
    current_level: AtomicU8,
}

impl GpioMotionSensor {
    /// Open `chip_path`, request `BOTH_EDGES` events on `line`, seed the
    /// level cache from the initial line value, and return a ready-to-use
    /// [`GpioMotionSensor`].
    pub fn new(
        chip_path: impl AsRef<Path>,
        line: u32,
        active_high: bool,
    ) -> Result<Self, MotionSensorError> {
        let mut chip = Chip::new(chip_path.as_ref()).map_err(|err| map_gpio_err(&err))?;
        let chip_line = chip.get_line(line).map_err(|err| map_gpio_err(&err))?;

        let raw_handle = chip_line
            .events(LineRequestFlags::INPUT, EventRequestFlags::BOTH_EDGES, CONSUMER)
            .map_err(|err| map_gpio_err(&err))?;

        // Seed the level cache by reading the line synchronously through
        // the event handle before wrapping it in the async stream.
        let initial_raw = raw_handle.get_value().map_err(|err| map_gpio_err(&err))?;
        let asserted = if active_high { initial_raw != 0 } else { initial_raw == 0 };
        let current_level = AtomicU8::new(if asserted { LEVEL_ASSERTED } else { LEVEL_QUIESCENT });

        let events = AsyncLineEventHandle::new(raw_handle).map_err(|err| map_gpio_err(&err))?;
        Ok(Self { active_high, events, current_level })
    }

    /// Map an observed [`EventType`] to the logical sensor state after
    /// the edge — true if the post-edge state is "asserted".
    fn edge_means_asserted(&self, event_type: EventType) -> bool {
        match event_type {
            EventType::RisingEdge => self.active_high,
            EventType::FallingEdge => !self.active_high,
        }
    }
}

#[async_trait]
impl MotionSensor for GpioMotionSensor {
    async fn next_trigger(&mut self) -> Result<DateTime<Utc>, MotionSensorError> {
        loop {
            match self.events.next().await {
                Some(Ok(event)) => {
                    let asserted = self.edge_means_asserted(event.event_type());
                    let raw = if asserted { LEVEL_ASSERTED } else { LEVEL_QUIESCENT };
                    self.current_level.store(raw, Ordering::Relaxed);
                    if asserted {
                        return Ok(Utc::now());
                    }
                    // Asserted-to-quiescent edge — update the cache and
                    // keep waiting for the next fresh assertion.
                }
                Some(Err(err)) => return Err(map_gpio_err(&err)),
                None => {
                    return Err(MotionSensorError::Unavailable("gpio event stream closed".into()));
                }
            }
        }
    }

    fn level(&self) -> Result<SensorLevel, MotionSensorError> {
        let raw = self.current_level.load(Ordering::Relaxed);
        Ok(if raw == LEVEL_ASSERTED { SensorLevel::Asserted } else { SensorLevel::Quiescent })
    }
}

fn map_gpio_err(err: &gpio_cdev::Error) -> MotionSensorError {
    MotionSensorError::Unavailable(err.to_string())
}
