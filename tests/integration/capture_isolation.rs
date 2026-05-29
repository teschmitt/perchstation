//! T029c — SC-009 / FR-012 / `contracts/cli.md` §Failure isolation.
//!
//! A panic in the capture task does NOT terminate the delivery task and
//! vice versa. The `spawn_supervised` wrapper (T033) catches the panic
//! at the inner `JoinHandle` boundary, emits
//! `service.task_panicked { task: "<name>" }`, and lets the sibling task
//! keep running.
//!
//! This test verifies both directions of the isolation guarantee:
//!
//! 1. A panic in a synthetic "delivery" task does not stop the capture
//!    supervisor from continuing to observe triggers.
//! 2. A panic in the capture task (injected via
//!    [`support::fake_motion_sensor::FakeMotionSensor::panic_on_next_trigger`])
//!    does not stop a synthetic "delivery" task from finishing its work.
//!
//! Modelling delivery as a counter-incrementing future is deliberate:
//! the isolation property is a guarantee of the wrapper, not of any
//! particular worker, so the test focuses on the property itself rather
//! than wiring up a full [`DeliveryRunner`] with `PerchpubClient`,
//! identity, and so on.
//!
//! Spec mapping: SC-009 / FR-012.

#[path = "support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use perchstation_core::capture::{Capture, CaptureState, StagingDir};
use perchstation_core::config::{CaptureConfig, EvictionPolicy};
use perchstation_core::hw_traits::SensorLevel;
use perchstation_core::observability::tracing::events as ev;
use perchstation_core::queue::inbox::StoreInbox;
use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
use perchstation_core::queue::store::QueueStore;
use perchstation_core::supervision::spawn_supervised;
use perchstation_hw::clock::SystemClock;

use support::fake_camera::FakeCamera;
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
async fn capture_panic_does_not_stop_delivery_sibling() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    let inbox: Arc<_> =
        Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), default_policy()));

    let sensor = FakeMotionSensor::new(SensorLevel::Quiescent);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path);
    let clock = Arc::new(SystemClock);

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

    let buf = CaptureBuffer::new();
    let _guard = install_json_subscriber(&buf);

    // Stand-in "delivery" task: count ticks. Will reach `target` only if
    // it is not aborted by an unrelated panic.
    let target: usize = 10;
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_for_task = counter.clone();
    let delivery_task = spawn_supervised("delivery", async move {
        for _ in 0..target {
            counter_for_task.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    let capture_shutdown = CancellationToken::new();
    let capture_task = spawn_supervised("capture", capture.run(capture_shutdown.clone()));

    wait_for_event(&buf, ev::CAPTURE_READY, Duration::from_secs(2)).await;

    // Trigger the capture panic.
    sensor_handle.panic_on_next_trigger("synthetic capture-side panic for T029c");

    // The wrapper logs the panic and resolves Ok.
    let _ = tokio::time::timeout(Duration::from_secs(5), capture_task)
        .await
        .expect("capture wrapper must resolve without propagating panic");

    // The supervisor emitted service.task_panicked { task: "capture" }.
    wait_for_event(&buf, ev::SERVICE_TASK_PANICKED, Duration::from_secs(2)).await;
    let panic_task = buf
        .events()
        .iter()
        .find(|e| e.get("event").and_then(Value::as_str) == Some(ev::SERVICE_TASK_PANICKED))
        .and_then(|e| e.get("task").and_then(Value::as_str).map(str::to_string))
        .expect("service.task_panicked must carry a `task` field");
    assert_eq!(panic_task, "capture");

    // Delivery sibling completes its full count regardless.
    let _ = tokio::time::timeout(Duration::from_secs(5), delivery_task).await;
    assert_eq!(
        counter.load(Ordering::SeqCst),
        target,
        "delivery sibling must reach its target despite a capture-side panic",
    );

    // Tidy up — cancel the (already-finished) capture's shutdown token.
    capture_shutdown.cancel();

    // Silence the unused warning for Utc when it is not otherwise used.
    let _: chrono::DateTime<Utc> = Utc::now();
}

#[tokio::test(flavor = "current_thread")]
async fn delivery_panic_does_not_stop_capture_sibling() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let staging_dir_path = dir.path().join("capture-staging");

    let inbox: Arc<_> =
        Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), default_policy()));

    let sensor = FakeMotionSensor::new(SensorLevel::Quiescent);
    let sensor_handle = sensor.handle();
    let camera = FakeCamera::new(&staging_dir_path);
    let clock = Arc::new(SystemClock);

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

    let buf = CaptureBuffer::new();
    let _guard = install_json_subscriber(&buf);

    let capture_shutdown = CancellationToken::new();
    let capture_task = spawn_supervised("capture", capture.run(capture_shutdown.clone()));

    // Synthetic "delivery" task that panics immediately.
    let delivery_task = spawn_supervised("delivery", async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        panic!("synthetic delivery-side panic for T029c");
    });

    wait_for_event(&buf, ev::CAPTURE_READY, Duration::from_secs(2)).await;

    // Delivery panics and is contained.
    let _ = tokio::time::timeout(Duration::from_secs(5), delivery_task)
        .await
        .expect("delivery wrapper must resolve without propagating panic");

    // Confirm the panic was logged with task=delivery.
    wait_for_event(&buf, ev::SERVICE_TASK_PANICKED, Duration::from_secs(2)).await;
    let panic_task = buf
        .events()
        .iter()
        .find(|e| e.get("event").and_then(Value::as_str) == Some(ev::SERVICE_TASK_PANICKED))
        .and_then(|e| e.get("task").and_then(Value::as_str).map(str::to_string))
        .expect("service.task_panicked must carry a `task` field");
    assert_eq!(panic_task, "delivery");

    // Capture sibling still records a fresh trigger after the panic.
    let trigger_at = Utc::now();
    sensor_handle.trigger(trigger_at);
    wait_for_event(&buf, ev::CAPTURE_RECORDING_COMPLETED, Duration::from_secs(5)).await;

    let pending_mp4s = store
        .pending_dir()
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .count();
    assert_eq!(pending_mp4s, 1, "capture must continue recording after delivery panic");

    capture_shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), capture_task).await;
}
