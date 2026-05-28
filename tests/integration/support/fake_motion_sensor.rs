//! In-memory [`MotionSensor`] for integration tests.
//!
//! Contract: `specs/002-capture-subsystem/contracts/hw-traits.md`
//! §Implementations. `next_trigger` is mpsc-backed (cancellation-safe);
//! `level` reads a shared `Mutex`. Helpers:
//!
//! - [`FakeMotionSensor::trigger`] — push a synthetic
//!   quiescent-to-asserted edge with a chosen wall-clock instant.
//! - [`FakeMotionSensor::set_level`] — set the value returned by the
//!   next [`MotionSensor::level`] call (the liveness tick reads this).
//! - [`FakeMotionSensor::set_error`] — drive both `next_trigger` and
//!   `level` to return [`MotionSensorError::Unavailable`] with the
//!   supplied message. Mirrors a disconnected GPIO line.
//! - [`FakeMotionSensor::clear_error`] — return both surfaces to the
//!   last `Ok` state. Used by the recovery legs of the liveness tests.
//! - [`FakeMotionSensor::panic_on_next_trigger`] — schedule a panic to
//!   run as soon as the supervisor awaits the next trigger. Used by the
//!   capture↔delivery panic-isolation test (T029c).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use perchstation_core::hw_traits::{MotionSensor, MotionSensorError, SensorLevel};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// `next_trigger` produces one of three outcomes when consumed.
enum TriggerItem {
    Ok(DateTime<Utc>),
    Err(MotionSensorError),
    /// When this is yielded, the `next_trigger` impl panics. Used by
    /// the panic-isolation test (T029c) to simulate a capture-side
    /// bug that would otherwise abort the supervisor.
    Panic(&'static str),
}

#[derive(Debug, Clone)]
struct LevelState {
    level: SensorLevel,
    error: Option<String>,
}

pub struct FakeMotionSensor {
    rx: UnboundedReceiver<TriggerItem>,
    handle: FakeMotionSensorHandle,
}

/// Cloneable handle for test code to drive the fake from outside the
/// `&mut self` `next_trigger` future. The handle is `Clone` so the test
/// can hold a copy while the supervisor task owns the
/// [`FakeMotionSensor`] itself.
#[derive(Clone)]
pub struct FakeMotionSensorHandle {
    tx: UnboundedSender<TriggerItem>,
    state: Arc<Mutex<LevelState>>,
}

impl FakeMotionSensor {
    #[must_use]
    pub fn new(initial_level: SensorLevel) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(LevelState { level: initial_level, error: None }));
        Self { rx, handle: FakeMotionSensorHandle { tx, state } }
    }

    #[must_use]
    pub fn handle(&self) -> FakeMotionSensorHandle {
        self.handle.clone()
    }

    /// Push a synthetic edge. Equivalent to `handle().trigger(at)`.
    pub fn trigger(&self, at: DateTime<Utc>) {
        self.handle.trigger(at);
    }

    /// Set the level returned by [`MotionSensor::level`].
    pub fn set_level(&self, level: SensorLevel) {
        self.handle.set_level(level);
    }

    /// Drive both surfaces to `Unavailable`.
    pub fn set_error(&self, message: impl Into<String>) {
        self.handle.set_error(message);
    }

    /// Clear a previously injected error so the recovery legs of the
    /// liveness tests can drive `Unavailable → Healthy`.
    pub fn clear_error(&self) {
        self.handle.clear_error();
    }

    /// Schedule a panic on the next `next_trigger` call.
    pub fn panic_on_next_trigger(&self, message: &'static str) {
        self.handle.panic_on_next_trigger(message);
    }
}

impl FakeMotionSensorHandle {
    pub fn trigger(&self, at: DateTime<Utc>) {
        let _ = self.tx.send(TriggerItem::Ok(at));
    }

    pub fn set_level(&self, level: SensorLevel) {
        let mut guard = self.state.lock().expect("fake motion-sensor mutex poisoned");
        guard.level = level;
    }

    pub fn set_error(&self, message: impl Into<String>) {
        let message = message.into();
        {
            let mut guard = self.state.lock().expect("fake motion-sensor mutex poisoned");
            guard.error = Some(message.clone());
        }
        let _ = self.tx.send(TriggerItem::Err(MotionSensorError::Unavailable(message)));
    }

    pub fn clear_error(&self) {
        let mut guard = self.state.lock().expect("fake motion-sensor mutex poisoned");
        guard.error = None;
    }

    pub fn panic_on_next_trigger(&self, message: &'static str) {
        let _ = self.tx.send(TriggerItem::Panic(message));
    }
}

#[async_trait]
impl MotionSensor for FakeMotionSensor {
    async fn next_trigger(&mut self) -> Result<DateTime<Utc>, MotionSensorError> {
        // mpsc receive is cancellation-safe; a dropped future leaves
        // queued edges in place for the next call. A None return means
        // every handle has been dropped — treat that as Unavailable so
        // the supervisor's liveness tracker can still observe it.
        match self.rx.recv().await {
            Some(TriggerItem::Ok(at)) => Ok(at),
            Some(TriggerItem::Err(err)) => Err(err),
            Some(TriggerItem::Panic(msg)) => panic!("{msg}"),
            None => Err(MotionSensorError::Unavailable("fake motion-sensor channel closed".into())),
        }
    }

    fn level(&self) -> Result<SensorLevel, MotionSensorError> {
        let guard = self.handle.state.lock().expect("fake motion-sensor mutex poisoned");
        match &guard.error {
            Some(msg) => Err(MotionSensorError::Unavailable(msg.clone())),
            None => Ok(guard.level),
        }
    }
}
