//! T011 — US1 acceptance #3 / FR-005.
//!
//! Recording terminates at `clip_duration_secs` even when the sensor
//! stays asserted; the resulting clip lands in `pending/`. Uses
//! `FakeCamera::Mode::Ok` (which sleeps for `max_duration` before
//! returning) together with `FakeMotionSensor::set_level(Asserted)`.
//!
//! Spec mapping: US1 #3 / FR-005.

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
use perchstation_core::observability::tracing::events as ev;
use perchstation_core::queue::inbox::StoreInbox;
use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
use perchstation_core::queue::store::QueueStore;

use support::fake_camera::{FakeCamera, Mode};
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

#[tokio::test(flavor = "current_thread")]
async fn recording_terminates_at_clip_duration_even_when_sensor_stays_asserted() {
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

    // Sensor reports Asserted continuously and stays that way.
    let sensor = FakeMotionSensor::new(SensorLevel::Asserted);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path).with_mode(Mode::Ok);

    let trigger_at = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(trigger_at));

    // Short clip duration (1 s) so the test stays fast; large enough
    // that the sensor's "sustained Asserted" state would otherwise
    // continue producing recordings if FR-005 were not honoured.
    let cfg = CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        cooldown_secs: 0,
        liveness_stuck_secs: 300,
        liveness_poll_secs: 60,
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
    sensor_handle.trigger(trigger_at);

    // Wait for completion. The recording duration is 1s plus a small
    // amount of overhead; allow 5s.
    let started = std::time::Instant::now();
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;
    let elapsed = started.elapsed();

    // The camera was bounded by clip_duration_secs = 1; even with the
    // sensor stuck Asserted, the recording terminated.
    assert!(
        elapsed >= Duration::from_millis(900),
        "recording finished before clip_duration elapsed (took {elapsed:?})",
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "recording took longer than clip_duration + slack (took {elapsed:?})",
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    // Exactly one clip in pending/.
    let pending = store.pending_dir();
    let mp4_count = pending
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(mp4_count, 1, "expected exactly one mp4 in pending/");

    // No capture.recording_hung event — the inner bound did the right
    // thing; we should not have hit the outer timeout.
    let events = buf.events();
    let hung = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_RECORDING_HUNG))
        .count();
    assert_eq!(hung, 0, "no capture.recording_hung expected on Mode::Ok");
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
