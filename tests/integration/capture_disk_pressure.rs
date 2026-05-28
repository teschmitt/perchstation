//! T027 — US2 #6 / FR-013.
//!
//! When the staging directory already exceeds `max_staging_bytes`
//! (pre-populated with garbage files), a trigger results in:
//!
//! - a `capture.disk_pressure_skip` event,
//! - no `Camera::record_clip` invocation,
//! - the supervisor entering cooldown so it does not tight-loop.
//!
//! Spec mapping: US2 #6 / FR-013.

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

/// Camera that records every invocation. Used to prove that the disk
/// pressure gate refuses to start a recording.
struct CountingCamera {
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl Camera for CountingCamera {
    async fn record_clip(&mut self, _: Duration) -> Result<RecordedClip, CameraError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Err(CameraError::EmptyOutput)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pre_populated_staging_above_ceiling_skips_with_disk_pressure() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    // Pre-populate staging with files that exceed the ceiling. The
    // staging-purge would normally remove anything inside, but we
    // re-create the files *after* the purge by sleeping until ready and
    // then writing the garbage. Simpler: set max_staging_bytes small
    // enough that the post-purge staging directory hits the ceiling after
    // we write garbage in *before* spawning the supervisor.
    //
    // Trick: the supervisor's staging-purge in mod.rs runs at the start
    // of `Capture::run`. We need files that survive that purge — so we
    // write them after wait_for_event(ready). Alternatively (and what
    // this test does) we set the ceiling to 0 and rely on
    // `staging_bytes(...)` reporting > 0 after the purge.
    //
    // Setting `max_staging_bytes = 0` causes any non-empty staging-bytes
    // reading to exceed the ceiling. The purge runs first and clears any
    // pre-existing files; the test asserts that on a trigger, the
    // disk-pressure gate kicks in based on what staging_bytes reports.
    // To produce a non-zero reading we have to put a file in after the
    // purge — see the body below.

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

    // 1024 bytes is the FakeCamera's default payload — but we're not
    // using FakeCamera here; we'll inject a 4 KiB garbage file ourselves.
    // Setting the ceiling to 1024 bytes ensures the 4 KiB injection
    // trips the gate.
    let cfg = CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        // Non-zero so the follow-up trigger sees a cooldown.
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

    // Inject garbage AFTER the staging purge has run so the disk-pressure
    // gate sees it on the upcoming trigger. 4 KiB exceeds the 1 KiB ceiling.
    std::fs::write(staging_dir_path.join("garbage.bin"), vec![0u8; 4096]).unwrap();

    sensor_handle.trigger(trigger_at);
    wait_for_event(&buf, ev::CAPTURE_DISK_PRESSURE_SKIP, Duration::from_secs(5)).await;

    // Camera was never invoked.
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "disk pressure gate must refuse to invoke the camera",
    );

    // Nothing landed in pending/.
    let pending_mp4s = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(pending_mp4s, 0);

    // CaptureState reflects the disk_pressure failure.
    let snap = state.snapshot();
    let failure = snap.last_failure.as_ref().expect("last_failure should be set");
    assert_eq!(failure.kind, "disk_pressure");

    // Cooldown is now active — a follow-up trigger emits cooldown_skip.
    let second = trigger_at + chrono::Duration::seconds(1);
    sensor_handle.trigger(second);
    wait_for_event(&buf, ev::CAPTURE_COOLDOWN_SKIP, Duration::from_secs(2)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}
