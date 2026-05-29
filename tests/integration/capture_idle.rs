//! T012 — US1 acceptance #4 / SC-008.
//!
//! With the motion sensor never firing, the capture loop performs zero
//! camera invocations, leaves staging empty, produces zero
//! `capture.recording_*` events, and `<data_dir>/queue/pending/` is
//! empty after a multi-second idle window.
//!
//! Spec mapping: US1 #4 / SC-008 / FR-014 (no I/O without trigger).

#[path = "support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use perchstation_core::capture::{Capture, CaptureState, StagingDir};
use perchstation_core::config::{CaptureConfig, EvictionPolicy};
use perchstation_core::hw_traits::{Camera, CameraError, RecordedClip, SensorLevel};
use perchstation_core::observability::tracing::events as ev;
use perchstation_core::queue::inbox::StoreInbox;
use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
use perchstation_core::queue::store::QueueStore;

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

/// Camera that increments a shared counter on every `record_clip` call
/// and refuses to record. Used to detect any unexpected camera
/// invocation by the idle loop.
struct CountingCamera {
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl Camera for CountingCamera {
    async fn record_clip(
        &mut self,
        _recording_id: &str,
        _max_duration: Duration,
    ) -> Result<RecordedClip, CameraError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Err(CameraError::EmptyOutput)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn idle_loop_performs_no_camera_invocations_and_leaves_pending_empty() {
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
    // No handle reference — the sensor never fires.
    let camera_invocations = Arc::new(AtomicUsize::new(0));
    let camera = CountingCamera { invocations: camera_invocations.clone() };

    let now = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(now));

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

    // Idle window: give the supervisor several seconds to run its
    // select! loop without any triggers.
    tokio::time::sleep(Duration::from_secs(2)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    assert_eq!(
        camera_invocations.load(Ordering::SeqCst),
        0,
        "camera must not be invoked while idle",
    );

    let events = buf.events();
    let recording_events: Vec<_> = events
        .iter()
        .filter(|e| {
            let code = e.get("event").and_then(Value::as_str).unwrap_or("");
            code.starts_with("capture.recording")
        })
        .collect();
    assert!(
        recording_events.is_empty(),
        "expected zero capture.recording_* events during idle, saw: {recording_events:?}",
    );

    // Pending is empty.
    let pending: Vec<_> = std::fs::read_dir(store.pending_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .collect();
    assert!(pending.is_empty(), "pending/ must be empty after idle; got {pending:?}");

    // Staging is empty.
    let staging: Vec<_> = std::fs::read_dir(&staging_dir_path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .collect();
    assert!(staging.is_empty(), "staging must be empty; got {staging:?}");

    // CaptureState reflects no recordings and no failures.
    let snap = state.snapshot();
    assert!(snap.last_recording_at.is_none());
    assert!(snap.last_clip_id.is_none());
    assert!(snap.last_failure.is_none());

    // Silence the unused-import warning when DateTime<Utc> isn't otherwise used.
    let _: DateTime<Utc> = now;
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
