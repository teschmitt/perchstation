//! Phase 3 review nit #1 — the operator-visible `staging_purged_files`
//! count on `capture.ready` must reflect the purge that ran in `serve`
//! *before* `service.ready` notified systemd, not a purge that
//! `Capture::run` did on its own task (which would happen too late and
//! would break the documented coupling in `plan.md` §Summary).
//!
//! The supervisor is fed the report via `Capture::with_purge_report` at
//! construction; `Capture::run` no longer performs the purge itself.

#[path = "support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use perchstation_core::capture::staging::PurgeReport;
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
async fn capture_ready_reflects_purge_report_passed_via_builder() {
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
    let camera = FakeCamera::new(&staging_dir_path);
    let clock = Arc::new(FakeClock::new(Utc.with_ymd_and_hms(2026, 5, 28, 14, 0, 0).unwrap()));
    let state = Arc::new(CaptureState::new());

    let cfg = CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        cooldown_secs: 0,
        liveness_stuck_secs: 300,
        liveness_poll_secs: 60,
        ..CaptureConfig::default()
    };

    // Simulate a purge in `serve` that cleaned up 7 leftover staging
    // files from a previous boot before this Capture was constructed.
    let purge_report = PurgeReport { removed_files: 7, removed_bytes: 123_456 };

    let capture = Capture::new(
        Box::new(sensor),
        Box::new(camera),
        inbox,
        state,
        clock,
        cfg,
        StagingDir::new(&staging_dir_path),
    )
    .with_purge_report(purge_report);

    let shutdown = CancellationToken::new();
    let buf = CaptureBuffer::new();
    let _guard = install_json_subscriber(&buf);

    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move { capture.run(task_shutdown).await });

    wait_for_event(&buf, ev::CAPTURE_READY, Duration::from_secs(2)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    let events = buf.events();
    let ready = events
        .iter()
        .find(|e| e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_READY))
        .expect("capture.ready must fire");
    let purged_files =
        ready.get("staging_purged_files").and_then(Value::as_u64).expect("staging_purged_files");
    assert_eq!(
        purged_files, 7,
        "capture.ready must report the count handed in via `with_purge_report`",
    );
}
