//! T029b — US2 #5 / R-5.
//!
//! Two synthetic motion edges arrive within a single recording's duration.
//! The supervisor's `select!` does not advance until `handle_trigger`
//! returns, so:
//!
//! - exactly one `Camera::record_clip` invocation occurs,
//! - exactly one clip lands in `pending/`.
//!
//! Spec mapping: US2 #5 / R-5 (supervisor's `select!` does not advance
//! until `handle_trigger` returns).

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
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

/// Wraps [`FakeCamera`] and counts `record_clip` invocations so the test
/// can assert that the second edge did not produce a second concurrent
/// recording.
struct CountingCamera {
    inner: FakeCamera,
    invocations: Arc<AtomicUsize>,
}

impl CountingCamera {
    fn new(staging_dir: PathBuf, invocations: Arc<AtomicUsize>) -> Self {
        Self { inner: FakeCamera::new(staging_dir), invocations }
    }
}

#[async_trait]
impl Camera for CountingCamera {
    async fn record_clip(
        &mut self,
        recording_id: &str,
        max_duration: Duration,
    ) -> Result<RecordedClip, CameraError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        self.inner.record_clip(recording_id, max_duration).await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn second_edge_during_active_recording_does_not_start_a_second_recording() {
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
    let invocations = Arc::new(AtomicUsize::new(0));
    let camera = CountingCamera::new(staging_dir_path.clone(), invocations.clone());

    let trigger_at = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(trigger_at));

    // 2-second recording window so the second edge clearly arrives while
    // the first recording is still in progress.
    let cfg = CaptureConfig {
        clip_duration_secs: 2,
        hang_margin_secs: 1,
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

    // Wait until the first recording is in progress.
    wait_for_event(&buf, ev::CAPTURE_RECORDING_STARTED, Duration::from_secs(2)).await;
    // Give the recording a moment to enter its inner sleep before pushing
    // the second edge.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Second edge while the first recording is in flight.
    sensor_handle.trigger(trigger_at + chrono::Duration::milliseconds(500));

    // Wait for the first recording to complete.
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "Camera::record_clip must be invoked exactly once across two overlapping edges",
    );

    let mp4_count = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(mp4_count, 1, "exactly one clip should land in pending/");
}
