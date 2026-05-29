//! T010 — US1 acceptance #1 + #2 / SC-001.
//!
//! End-to-end happy path: a single motion-sensor edge produces exactly
//! one bounded clip file that arrives in `<data_dir>/queue/pending/` via
//! `Inbox::submit`, with `captured_at` reflecting the trigger time, and
//! `<data_dir>/capture-staging/` is empty afterwards.
//!
//! Spec mapping: US1 #1, #2 / SC-001 / FR-001 / FR-007.

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
use perchstation_core::queue::ClipQueueEntry;
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

fn small_capture_config() -> CaptureConfig {
    CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        cooldown_secs: 0,
        liveness_stuck_secs: 300,
        liveness_poll_secs: 60,
        ..CaptureConfig::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn single_trigger_lands_one_clip_in_pending_with_captured_at_equal_to_trigger() {
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

    let trigger_at = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(trigger_at));

    let state = Arc::new(CaptureState::new());
    let capture = Capture::new(
        Box::new(sensor),
        Box::new(camera),
        inbox,
        state.clone(),
        clock,
        small_capture_config(),
        StagingDir::new(&staging_dir_path),
    );

    let shutdown = CancellationToken::new();
    let buf = CaptureBuffer::new();
    let _guard = install_json_subscriber(&buf);

    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        capture.run(task_shutdown).await;
    });

    // Wait for the staging-purge / capture.ready to land before sending
    // the trigger so we don't race the supervisor's startup.
    wait_for_event(&buf, ev::CAPTURE_READY, Duration::from_secs(2)).await;

    sensor_handle.trigger(trigger_at);

    // Wait for the recording to complete.
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    // Exactly one .mp4 + sidecar in pending/.
    let pending_dir = store.pending_dir();
    let mp4_count = pending_dir
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    let sidecar_count = pending_dir
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count();
    assert_eq!(mp4_count, 1, "expected exactly one mp4 in pending/");
    assert_eq!(sidecar_count, 1, "expected exactly one sidecar in pending/");

    // Sidecar's captured_at matches the trigger time.
    let sidecar = pending_dir
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.path().extension().is_some_and(|x| x == "json"))
        .expect("sidecar");
    let raw = std::fs::read(sidecar.path()).unwrap();
    let entry: ClipQueueEntry = serde_json::from_slice(&raw).expect("sidecar parses");
    assert_eq!(entry.captured_at, trigger_at, "captured_at must mirror trigger time");

    // Staging is empty.
    let staging_files: Vec<_> = std::fs::read_dir(&staging_dir_path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .collect();
    assert!(staging_files.is_empty(), "staging must be empty after submit; got {staging_files:?}");

    // CaptureState reflects the success.
    let snap = state.snapshot();
    assert_eq!(snap.last_recording_at, Some(trigger_at));
    assert_eq!(snap.last_clip_id, Some(entry.clip_id.clone()));
    assert!(snap.last_failure.is_none());

    // Event ordering: capture.ready before capture.recording_started.
    let events = buf.events();
    let ready_idx = events
        .iter()
        .position(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_READY))
        .expect("capture.ready");
    let started_idx = events
        .iter()
        .position(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_RECORDING_STARTED))
        .expect("capture.recording_started");
    assert!(ready_idx < started_idx, "capture.ready must precede capture.recording_started");
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
