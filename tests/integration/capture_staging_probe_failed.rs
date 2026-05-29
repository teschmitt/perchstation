//! Phase-4 review fix.
//!
//! When the pre-record `staging_bytes` probe itself errors (as opposed
//! to reporting bytes-over-ceiling), the supervisor must emit
//! `capture.staging_probe_failed` and NOT `capture.disk_pressure_skip`.
//! The trigger still falls through to the camera — a failed probe is
//! not by itself a reason to refuse to record. See
//! `specs/002-capture-subsystem/contracts/log-events.md`.
//!
//! Provocation: replace the staging *directory* with a regular *file*
//! after `capture.ready`. The supervisor's next `fs::read_dir(staging)`
//! call then returns `ENOTDIR` (which is not `NotFound`, so it surfaces
//! as `Err` instead of `Ok(0)`).

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

/// Camera that counts invocations and returns `EmptyOutput`. The probe
/// failure under test must NOT prevent the camera from being called —
/// that is the whole point of the fix.
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
async fn staging_probe_io_error_emits_dedicated_event_and_records() {
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
    let camera = CountingCamera { invocations: invocations.clone() };

    let trigger_at = Utc.with_ymd_and_hms(2026, 5, 28, 14, 23, 12).unwrap();
    let clock = Arc::new(FakeClock::new(trigger_at));

    let cfg = CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        cooldown_secs: 60,
        liveness_stuck_secs: 300,
        liveness_poll_secs: 60,
        max_staging_bytes: 1024,
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

    // Replace the staging *directory* with a regular *file*. The next
    // `staging_bytes(...)` call then hits `fs::read_dir`-on-file, which
    // returns `ENOTDIR` — not `NotFound`, so the function returns `Err`
    // instead of the `Ok(0)` shortcut for missing directories.
    std::fs::remove_dir(&staging_dir_path).expect("remove staging dir");
    std::fs::write(&staging_dir_path, b"").expect("write file at staging path");

    sensor_handle.trigger(trigger_at);

    wait_for_event(&buf, ev::CAPTURE_STAGING_PROBE_FAILED, Duration::from_secs(5)).await;

    // The probe failure must NOT masquerade as a disk-pressure skip.
    let events = buf.events();
    let dp_skips: Vec<_> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_DISK_PRESSURE_SKIP))
        .collect();
    assert!(
        dp_skips.is_empty(),
        "probe-failure branch must not emit `{}`; events: {:?}",
        ev::CAPTURE_DISK_PRESSURE_SKIP,
        events.iter().filter_map(|e| e.get("event").and_then(Value::as_str)).collect::<Vec<_>>(),
    );

    // The recording attempt must still have happened: a failed probe is
    // not by itself a reason to refuse to record.
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "probe failure must fall through to a recording attempt",
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}
