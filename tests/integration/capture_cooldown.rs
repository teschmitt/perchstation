//! T023 — US2 #2 / FR-004 / FR-006.
//!
//! Two scenarios:
//!
//! 1. Two trigger edges arrive within `cooldown_secs`. Only the first turns
//!    into a recording; the second emits `capture.cooldown_skip` and never
//!    invokes the camera.
//! 2. After the first edge, the fake sensor remains `Asserted` past
//!    `cooldown_secs`. No second recording fires while the sensor is still
//!    asserted — recordings only resume after a fresh Quiescent → Asserted
//!    edge is pushed by the test (FR-004 second clause).
//!
//! Spec mapping: US2 #2 / FR-004 / FR-006.

#[path = "support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use perchstation_core::capture::{Capture, CaptureState, StagingDir};
use perchstation_core::config::{CaptureConfig, EvictionPolicy};
use perchstation_core::hw_traits::{Camera, CameraError, RecordedClip, SensorLevel};
use perchstation_core::observability::tracing::events as ev;
use perchstation_core::queue::inbox::StoreInbox;
use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
use perchstation_core::queue::store::QueueStore;
use perchstation_hw::clock::SystemClock;

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

fn default_policy() -> QueuePolicy {
    QueuePolicy {
        max_clips: 500,
        max_bytes: 2 * 1024 * 1024 * 1024,
        eviction: EvictionPolicy::DropOldestUndelivered,
    }
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
async fn two_edges_within_cooldown_yield_one_clip_and_one_cooldown_skip() {
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
        // Long cooldown so the second edge clearly arrives inside it.
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
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    // Second edge inside the cooldown window.
    sensor_handle.trigger(trigger_at + chrono::Duration::seconds(1));
    wait_for_event(&buf, ev::CAPTURE_COOLDOWN_SKIP, Duration::from_secs(2)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    let events = buf.events();
    let recording_completed = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_RECORDING_COMPLETED))
        .count();
    assert_eq!(recording_completed, 1, "expected exactly one completed recording");

    let cooldown_skips = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_COOLDOWN_SKIP))
        .count();
    assert_eq!(cooldown_skips, 1, "expected exactly one cooldown_skip");

    // Exactly one mp4 in pending/.
    let mp4_count = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(mp4_count, 1);
}

/// A camera that records cleanly the first time, then refuses to be
/// invoked again. Used to assert that no further recordings fire while
/// the sensor stays `Asserted` past cooldown.
struct OnceCamera {
    inner: FakeCamera,
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl Camera for OnceCamera {
    async fn record_clip(&mut self, max_duration: Duration) -> Result<RecordedClip, CameraError> {
        let n = self.invocations.fetch_add(1, Ordering::SeqCst);
        if n >= 1 {
            // Any post-first invocation is a test failure for the sustained-
            // assert scenario; return Err so the supervisor logs it loudly.
            return Err(CameraError::Aborted(
                "OnceCamera invoked a second time — sustained Asserted produced a forbidden recording".into(),
            ));
        }
        self.inner.record_clip(max_duration).await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sustained_asserted_does_not_re_record_until_quiescent_to_asserted_edge() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    let inbox: Arc<_> =
        Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), default_policy()));

    let sensor = FakeMotionSensor::new(SensorLevel::Asserted);
    let sensor_handle = sensor.handle();
    let invocations = Arc::new(AtomicUsize::new(0));
    let camera =
        OnceCamera { inner: FakeCamera::new(&staging_dir_path), invocations: invocations.clone() };

    // This scenario depends on cooldown actually elapsing in wall-clock
    // time, so use a real clock; FakeClock's `now()` is static and would
    // keep the cooldown gate permanently active.
    let trigger_at = Utc::now();
    let clock = Arc::new(SystemClock);

    let cfg = CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        // Very short cooldown so the sustained-Asserted observation
        // sits well past `cooldown_secs`.
        cooldown_secs: 1,
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

    // First edge → records.
    sensor_handle.trigger(trigger_at);
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    // Wait long enough that cooldown has fully elapsed. Sensor stays
    // Asserted; no fresh edge is pushed during this window.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Camera was invoked exactly once — sustained-Asserted alone does not
    // re-fire the loop (US2 #2 second clause).
    assert_eq!(invocations.load(Ordering::SeqCst), 1);

    // Now push the fresh edge — this is the Quiescent → Asserted
    // transition the spec requires before recordings can resume.
    let second = trigger_at + chrono::Duration::seconds(5);
    sensor_handle.trigger(second);

    // A second completion arrives (we know the camera fails on second
    // invocation in this fake, so a recording_failed arrives instead —
    // that proves the loop tried again on the fresh edge).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let saw_second_attempt = loop {
        let events = buf.events();
        let attempts = events
            .iter()
            .filter(|e| {
                let code = e.get("event").and_then(Value::as_str).unwrap_or("");
                code == ev::CAPTURE_RECORDING_STARTED
            })
            .count();
        if attempts >= 2 {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(saw_second_attempt, "fresh edge must produce a second capture attempt");

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}
