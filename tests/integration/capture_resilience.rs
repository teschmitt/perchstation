//! T029 — US2 #1 / FR-009 / FR-017 / SC-003 / Edge Case "Sensor fires
//! during boot or shutdown".
//!
//! Three scenarios:
//!
//! 1. Crash-restart: a partial staging file from a previous run is purged
//!    on the next boot, the queue is left intact, and a fresh trigger
//!    after restart records normally.
//! 2. Boot edge: an edge that arrived before the supervisor's `select!`
//!    reached its first iteration is observed (mpsc buffers it) and turns
//!    into a clip — no corruption, no partial queue entry.
//! 3. Shutdown edge: an edge that arrives after the `CancellationToken`
//!    has been signalled is dropped cleanly — no new staging file, no
//!    partial queue entry.
//!
//! Spec mapping: US2 #1 / FR-009 / FR-017 / SC-003 / Edge Case "Sensor
//! fires during boot or shutdown".

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

fn default_policy() -> QueuePolicy {
    QueuePolicy {
        max_clips: 500,
        max_bytes: 2 * 1024 * 1024 * 1024,
        eviction: EvictionPolicy::DropOldestUndelivered,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn staging_purge_clears_partial_clip_from_previous_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    // Simulate the on-disk state of a crash-mid-recording: a partial file
    // lingering in capture-staging/, and one happily delivered clip
    // already in pending/.
    std::fs::create_dir_all(&staging_dir_path).unwrap();
    let partial = staging_dir_path.join("20260528T140000Z-cap.mp4");
    std::fs::write(&partial, vec![0u8; 4096]).unwrap();
    assert!(partial.is_file());

    // Pre-populate the queue with one durable clip — the test asserts the
    // staging purge does not touch it.
    let preload_dir = dir.path().join("preload");
    std::fs::create_dir_all(&preload_dir).unwrap();
    let preload_path = preload_dir.join("preload.mp4");
    std::fs::write(&preload_path, vec![0u8; 256]).unwrap();
    let preload_t = Utc.with_ymd_and_hms(2026, 5, 28, 13, 0, 0).unwrap();
    {
        let inbox =
            PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), default_policy());
        inbox.submit(&preload_path, ClipMeta { captured_at: preload_t }).await.expect("preload");
    }

    let sensor = FakeMotionSensor::new(SensorLevel::Quiescent);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path);

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

    let inbox: Arc<_> =
        Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), default_policy()));
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

    // The partial file from the simulated crash is gone.
    assert!(!partial.exists(), "staging purge must remove partial from previous run");

    // The pre-loaded clip in pending/ is untouched.
    let pending_mp4s = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(pending_mp4s, 1, "pre-loaded queue entry must survive boot");

    // Fresh trigger after restart records normally.
    sensor_handle.trigger(trigger_at);
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    let pending_mp4s_after = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(pending_mp4s_after, 2, "post-restart trigger must produce a new clip");
}

#[tokio::test(flavor = "current_thread")]
async fn edge_buffered_before_ready_still_produces_a_clip() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    let inbox: Arc<_> =
        Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), default_policy()));

    let sensor = FakeMotionSensor::new(SensorLevel::Quiescent);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path);

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
        state.clone(),
        clock,
        cfg,
        StagingDir::new(&staging_dir_path),
    );

    let shutdown = CancellationToken::new();
    let buf = CaptureBuffer::new();
    let _guard = install_json_subscriber(&buf);

    // Push the edge BEFORE spawning the capture task — the mpsc buffers
    // it and the supervisor observes it on the first iteration of its
    // select! loop after the staging purge has completed.
    sensor_handle.trigger(trigger_at);

    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move { capture.run(task_shutdown).await });

    wait_for_event(&buf, ev::CAPTURE_READY, Duration::from_secs(2)).await;
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    let mp4_count = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(mp4_count, 1, "buffered edge must produce exactly one clip");
}

#[tokio::test(flavor = "current_thread")]
async fn edge_after_shutdown_signal_does_not_produce_a_clip() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    let inbox: Arc<_> =
        Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), default_policy()));

    let sensor = FakeMotionSensor::new(SensorLevel::Quiescent);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path);

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

    // Cancel the supervisor BEFORE pushing the edge. The supervisor's
    // shutdown branch should fire on the next select! tick, and the
    // buffered edge should never turn into a recording.
    shutdown.cancel();
    // Give the supervisor a moment to observe cancellation.
    tokio::time::sleep(Duration::from_millis(100)).await;
    sensor_handle.trigger(trigger_at);

    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    // No clip recorded.
    let mp4_count = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(mp4_count, 0, "post-shutdown edge must not produce a clip");

    // Staging is empty.
    let staging_files: Vec<_> = std::fs::read_dir(&staging_dir_path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .collect();
    assert!(staging_files.is_empty(), "no staging file after shutdown");
}
