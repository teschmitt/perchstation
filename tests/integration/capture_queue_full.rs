//! T028 — US2 #7 / FR-018.
//!
//! When the inbox returns `InboxError::QueueFull` (`PolicyInbox` configured
//! with `EvictionPolicy::RefuseNew` and `max_clips = 1`, pre-loaded with one
//! clip):
//! - `capture.queue_refused { kind: "queue_full" }` is emitted,
//! - the staging file is removed,
//! - cooldown is started so the loop does not tight-loop,
//! - the supervisor keeps running and accepts a subsequent trigger once
//!   capacity returns.
//!
//! Spec mapping: US2 #7 / FR-018.

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
use perchstation_core::queue::inbox::{Inbox, StoreInbox};
use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
use perchstation_core::queue::store::{ClipMeta, QueueStore};

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
async fn queue_full_refusal_cleans_staging_starts_cooldown_and_loop_continues() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    let policy = QueuePolicy {
        max_clips: 1,
        max_bytes: 2 * 1024 * 1024 * 1024,
        eviction: EvictionPolicy::RefuseNew,
    };
    let inbox: Arc<_> =
        Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy));

    // Pre-fill the queue so the supervisor's first submit will be refused.
    let preload_dir = dir.path().join("preload");
    std::fs::create_dir_all(&preload_dir).unwrap();
    let preload_path = preload_dir.join("preload.mp4");
    std::fs::write(&preload_path, vec![0u8; 256]).unwrap();
    let preload_t = Utc.with_ymd_and_hms(2026, 5, 28, 14, 22, 0).unwrap();
    inbox.submit(&preload_path, ClipMeta { captured_at: preload_t }).await.expect("preload submit");

    let sensor = FakeMotionSensor::new(SensorLevel::Quiescent);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path);

    let trigger_at = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(trigger_at));

    let cfg = CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        // Non-zero cooldown so we can verify the supervisor started one
        // (visible as a cooldown_skip on a follow-up trigger).
        cooldown_secs: 60,
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
    wait_for_event(&buf, ev::CAPTURE_QUEUE_REFUSED, Duration::from_secs(5)).await;

    // Refusal carries kind = "queue_full".
    let refusal_kind = buf
        .events()
        .iter()
        .find(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_QUEUE_REFUSED))
        .and_then(|e| e.get("kind").and_then(Value::as_str).map(str::to_string))
        .expect("queue_refused event must have a kind field");
    assert_eq!(refusal_kind, "queue_full");

    // Staging cleaned up.
    let staging_files: Vec<_> = std::fs::read_dir(&staging_dir_path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .collect();
    assert!(
        staging_files.is_empty(),
        "staging must be empty after queue refusal; got {staging_files:?}",
    );

    // CaptureState reflects the queue_full failure.
    let snap = state.snapshot();
    let failure = snap.last_failure.as_ref().expect("last_failure should be set");
    assert_eq!(failure.kind, "queue_full");

    // Follow-up trigger inside the cooldown window observes a cooldown
    // skip — confirms the supervisor started a cooldown after the refusal.
    let second = trigger_at + chrono::Duration::seconds(1);
    sensor_handle.trigger(second);
    wait_for_event(&buf, ev::CAPTURE_COOLDOWN_SKIP, Duration::from_secs(2)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    // No new pending mp4 (the queue is still full and the refusal kept
    // it that way).
    let pending_mp4s = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(
        pending_mp4s, 1,
        "queue should still contain only the pre-loaded clip after the refusal",
    );
}
