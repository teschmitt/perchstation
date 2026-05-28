//! T029a — Edge Case "Camera adapter hangs" / FR-005 / `capture.recording_hung`.
//!
//! `FakeCamera::Mode::Hang` never resolves. The supervisor's outer
//! `tokio::time::timeout(clip_duration + hang_margin)` fires, the future is
//! dropped (which runs the fake camera's staging-file guard), and:
//!
//! - exactly one `capture.recording_hung` event is emitted,
//! - the staging file is removed,
//! - no clip is created in `pending/`,
//! - `CaptureState::record_failure` is updated with `kind = "camera_hang"`,
//! - a subsequent trigger (after the fault clears) records cleanly.
//!
//! Spec mapping: Edge Case "Camera adapter hangs" / FR-005 outer bound /
//! `capture.recording_hung`.

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
async fn hung_camera_emits_recording_hung_cleans_staging_and_loop_recovers() {
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
    let camera = camera.with_mode(Mode::Hang);

    let trigger_at = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(trigger_at));

    // Short clip duration + small hang margin so the outer timeout fires
    // quickly. clip_duration_secs + hang_margin_secs = 2s in total.
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
    wait_for_event(&buf, ev::CAPTURE_RECORDING_HUNG, Duration::from_secs(5)).await;

    // Exactly one capture.recording_hung event.
    let hungs = buf
        .events()
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_RECORDING_HUNG))
        .count();
    assert_eq!(hungs, 1);

    // Staging cleaned up (drop-cancellation path of the fake camera).
    let staging_files: Vec<_> = std::fs::read_dir(&staging_dir_path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .collect();
    assert!(
        staging_files.is_empty(),
        "staging must be empty after a hung recording; got {staging_files:?}",
    );

    // No clip in pending/.
    let pending_mp4s = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(pending_mp4s, 0);

    // CaptureState's last_failure.kind is camera_hang.
    let snap = state.snapshot();
    let failure = snap.last_failure.as_ref().expect("last_failure should be set");
    assert_eq!(failure.kind, "camera_hang");

    // Hang fault clears → loop accepts the next trigger.
    camera_handle.set_mode(Mode::Ok);
    let second = trigger_at + chrono::Duration::seconds(3);
    sensor_handle.trigger(second);
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    let pending_mp4s = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(pending_mp4s, 1);
}
