//! Phase 3 review nit #3 — the `recording_id` the supervisor mints and
//! logs must be the same id the camera adapter writes to disk, so an
//! operator grepping a `capture.recording_started` event can locate the
//! corresponding staging file by exact filename match (instead of
//! guessing across a second boundary).

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
async fn camera_receives_supervisor_minted_recording_id() {
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
    let camera_handle = camera.handle();

    let trigger_at = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(trigger_at));

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
        state,
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
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    // The supervisor mints `recording_id` from `triggered_at` and logs it
    // on `capture.recording_started`. The camera adapter must use that
    // same id when naming its staging file, so an operator looking up
    // the on-disk artefact from the log line gets an exact match.
    let events = buf.events();
    let started = events
        .iter()
        .find(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_RECORDING_STARTED))
        .expect("capture.recording_started must fire");
    let logged_recording_id = started
        .get("recording_id")
        .and_then(Value::as_str)
        .expect("recording_id field")
        .to_string();

    let camera_recording_id = camera_handle.last_recording_id().expect("camera was invoked");
    assert_eq!(
        camera_recording_id, logged_recording_id,
        "FakeCamera::record_clip must receive the supervisor's recording_id",
    );

    let last_path = camera_handle.last_clip_path().expect("camera produced a clip path");
    let expected_filename = format!("{logged_recording_id}.mp4");
    assert_eq!(
        last_path.file_name().and_then(|s| s.to_str()),
        Some(expected_filename.as_str()),
        "staging filename must be `<recording_id>.mp4` so log↔disk correlation is exact",
    );
}
