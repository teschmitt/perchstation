//! T024 — US2 #3 / FR-010 / SC-004.
//!
//! When the motion sensor stays `Asserted` continuously for
//! `liveness_stuck_secs`, the supervisor's periodic liveness tick
//! observes the condition and the [`SensorLivenessTracker`] transitions
//! `Healthy` → `StuckAsserted`. The supervisor:
//!
//! - emits `capture.sensor_degraded { kind: "stuck_asserted" }`,
//! - refuses to record on a subsequent trigger (emits
//!   `capture.degraded_skip` and updates `last_failure.kind` to a
//!   "degraded" reason),
//! - emits `capture.sensor_recovered { kind: "stuck_asserted" }` once the
//!   sensor returns to Quiescent.
//!
//! Spec mapping: US2 #3 / FR-010 / SC-004.

#[path = "support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
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
use perchstation_hw::clock::SystemClock;

use support::fake_camera::FakeCamera;
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
async fn stuck_asserted_degrades_then_skips_then_recovers() {
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

    // Sensor stays Asserted continuously — this is the stuck-sensor scenario.
    let sensor = FakeMotionSensor::new(SensorLevel::Asserted);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path);

    // SystemClock so the supervisor's `now - asserted_since` comparison
    // actually progresses; FakeClock's static `now()` would keep the
    // tracker pinned at `Healthy` because the threshold never elapses.
    let trigger_at = Utc::now();
    let clock = Arc::new(SystemClock);

    // Aggressively short thresholds so the test runs fast.
    let cfg = CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        cooldown_secs: 0,
        liveness_stuck_secs: 1,
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

    // Within ~liveness_poll_secs + liveness_stuck_secs the supervisor's
    // periodic tick should detect the stuck condition.
    wait_for_event_with_kind(
        &buf,
        ev::CAPTURE_SENSOR_DEGRADED,
        "stuck_asserted",
        Duration::from_secs(10),
    )
    .await;

    // A fresh trigger now should be skipped due to the degraded sensor.
    sensor_handle.trigger(trigger_at);
    wait_for_event(&buf, ev::CAPTURE_DEGRADED_SKIP, Duration::from_secs(5)).await;

    // No clip in pending/, no file in staging.
    let pending_mp4s = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(pending_mp4s, 0);
    let staging_files: Vec<_> = std::fs::read_dir(&staging_dir_path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .collect();
    assert!(staging_files.is_empty());

    // Snapshot shows sensor_liveness = StuckAsserted.
    let snap = state.snapshot();
    assert_eq!(snap.sensor_liveness, CaptureLivenessSnapshot::StuckAsserted);
    assert!(snap.sensor_degraded_since.is_some());

    // Sensor recovers — flip to Quiescent.
    sensor_handle.set_level(SensorLevel::Quiescent);
    wait_for_event_with_kind(
        &buf,
        ev::CAPTURE_SENSOR_RECOVERED,
        "stuck_asserted",
        Duration::from_secs(10),
    )
    .await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    let snap = state.snapshot();
    assert_eq!(snap.sensor_liveness, CaptureLivenessSnapshot::Healthy);
    assert!(snap.sensor_degraded_since.is_none());
}
