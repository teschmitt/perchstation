//! T025 — US2 #4 / FR-011 / SC-005.
//!
//! When `MotionSensor::level()` and `MotionSensor::next_trigger()` return
//! `Err`, the supervisor marks the sensor as `Unavailable`:
//!
//! - emits `capture.sensor_degraded { kind: "unavailable", reason: <error> }`,
//! - keeps the loop running (the supervisor must not crash on adapter
//!   failure),
//! - emits `capture.sensor_recovered { kind: "unavailable" }` when the
//!   adapter recovers.
//!
//! Spec mapping: US2 #4 / FR-011 / SC-005.

#[path = "support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use perchstation_core::capture::{Capture, CaptureState, StagingDir};
use perchstation_core::config::{CaptureConfig, EvictionPolicy};
use perchstation_core::hw_traits::SensorLevel;
use perchstation_core::observability::status::CaptureLivenessSnapshot;
use perchstation_core::observability::tracing::events as ev;
use perchstation_core::queue::inbox::StoreInbox;
use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
use perchstation_core::queue::store::QueueStore;

use support::fake_camera::FakeCamera;
use support::fake_clock::FakeClock;
use support::fake_motion_sensor::FakeMotionSensor;
use support::logs::CaptureBuffer;

fn install_json_subscriber(buf: &CaptureBuffer) -> tracing::subscriber::DefaultGuard {
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_default(subscriber)
}

async fn wait_for_event_with_kind(buf: &CaptureBuffer, code: &str, kind: &str, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let found = buf.events().iter().any(|e| {
            e.get("event").and_then(Value::as_str) == Some(code)
                && e.get("kind").and_then(Value::as_str) == Some(kind)
        });
        if found {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let codes: Vec<String> = buf
        .events()
        .iter()
        .filter_map(|e| e.get("event").and_then(Value::as_str).map(str::to_string))
        .collect();
    panic!("timed out waiting for `{code}` with kind=`{kind}`; saw events: {codes:?}");
}

async fn wait_for_event(buf: &CaptureBuffer, code: &str, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if buf.events().iter().any(|e| e.get("event").and_then(Value::as_str) == Some(code)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let codes: Vec<String> = buf
        .events()
        .iter()
        .filter_map(|e| e.get("event").and_then(Value::as_str).map(str::to_string))
        .collect();
    panic!("timed out waiting for `{code}`; saw events: {codes:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn unavailable_sensor_emits_degraded_then_recovered_and_loop_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    let policy = QueuePolicy {
        max_clips: 500,
        max_bytes: 2 * 1024 * 1024 * 1024,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox: Arc<_> =
        Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy));

    let sensor = FakeMotionSensor::new(SensorLevel::Quiescent);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path);

    let now = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(now));

    let cfg = CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        cooldown_secs: 0,
        liveness_stuck_secs: 300,
        liveness_poll_secs: 1,
        ..CaptureConfig::default()
    };

    let state = Arc::new(CaptureState::new());
    let capture = Capture::new(
        Box::new(sensor),
        Box::new(camera),
        inbox,
        state.clone(),
        clock,
        cfg,
        StagingDir::new(&staging_dir_path),
    );

    let shutdown = CancellationToken::new();
    let buf = CaptureBuffer::new();
    let _guard = install_json_subscriber(&buf);

    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move { capture.run(task_shutdown).await });

    wait_for_event(&buf, ev::CAPTURE_READY, Duration::from_secs(2)).await;

    // Drive the adapter into Unavailable. set_error also pushes an Err
    // through the next_trigger channel, so the supervisor sees both
    // routes onto the Unavailable branch.
    sensor_handle.set_error("simulated disconnect");

    wait_for_event_with_kind(
        &buf,
        ev::CAPTURE_SENSOR_DEGRADED,
        "unavailable",
        Duration::from_secs(10),
    )
    .await;

    // Snapshot reflects Unavailable.
    let snap = state.snapshot();
    assert_eq!(snap.sensor_liveness, CaptureLivenessSnapshot::Unavailable);
    assert!(snap.sensor_degraded_since.is_some());

    // Loop still running — capture.shutdown has NOT fired and we can
    // observe more events after the degrade.
    // (Implicit: if the task panicked, subsequent waits would time out.)

    // Clear the error → recovery.
    sensor_handle.clear_error();
    wait_for_event_with_kind(
        &buf,
        ev::CAPTURE_SENSOR_RECOVERED,
        "unavailable",
        Duration::from_secs(10),
    )
    .await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    let snap = state.snapshot();
    assert_eq!(snap.sensor_liveness, CaptureLivenessSnapshot::Healthy);
    assert!(snap.sensor_degraded_since.is_none());
}
