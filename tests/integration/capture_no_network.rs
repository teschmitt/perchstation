//! T035 — capture loop produces no outbound network traffic (RED).
//!
//! Runs a [`Capture`] task in-process with [`FakeMotionSensor`] firing
//! periodic edges and [`FakeCamera`] in `Mode::Ok` alongside an
//! `outbound_allowlist`-style "rogue" listener. Asserts the listener
//! receives zero accept(s) over the assertion window — i.e. no capture-
//! side code path tries to open an outbound TCP connection, even
//! incidentally (e.g. a leftover metrics shim, a redirect-follow bug
//! cloned from the delivery side, an accidental phone-home).
//!
//! The structural guarantee the test enforces is FR-014: the capture
//! subsystem never opens a new telemetry channel. The existing
//! `tracing` JSON channel (stderr, scrubbed by the redaction layer) is
//! the only egress, and it does not touch the network.
//!
//! Spec mapping: US3 #3 / FR-014 / `contracts/cli.md` §`status`.

#[path = "support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use perchstation_core::capture::{Capture, CaptureState, StagingDir};
use perchstation_core::config::{CaptureConfig, EvictionPolicy};
use perchstation_core::hw_traits::SensorLevel;
use perchstation_core::observability::tracing::events as ev;
use perchstation_core::queue::inbox::StoreInbox;
use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
use perchstation_core::queue::store::QueueStore;
use perchstation_hw::clock::SystemClock;

use support::fake_camera::FakeCamera;
use support::fake_motion_sensor::FakeMotionSensor;
use support::logs::CaptureBuffer;

/// Trivial TCP listener that increments a counter on every accept. Same
/// pattern as `outbound_allowlist.rs`'s `RogueListener` — any capture-
/// side code that tries to phone home to localhost would hit this and
/// fail the assertion.
struct RogueListener {
    addr: String,
    accept_count: Arc<AtomicU32>,
    _task: tokio::task::JoinHandle<()>,
}

impl RogueListener {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind rogue listener");
        let port = listener.local_addr().expect("local_addr").port();
        let addr = format!("127.0.0.1:{port}");
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = count.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                count_clone.fetch_add(1, Ordering::SeqCst);
                let _ = sock.shutdown().await;
            }
        });
        Self { addr, accept_count: count, _task: task }
    }

    fn count(&self) -> u32 {
        self.accept_count.load(Ordering::SeqCst)
    }

    fn addr(&self) -> &str {
        &self.addr
    }
}

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
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
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

fn fast_capture_config() -> CaptureConfig {
    CaptureConfig {
        clip_duration_secs: 1,
        hang_margin_secs: 1,
        cooldown_secs: 0,
        liveness_stuck_secs: 300,
        liveness_poll_secs: 60,
        ..CaptureConfig::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn capture_loop_opens_no_outbound_connections_during_normal_operation() {
    let rogue = RogueListener::start().await;

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
    let clock = Arc::new(SystemClock);
    let state = Arc::new(CaptureState::new());

    let capture = Capture::new(
        Box::new(sensor),
        Box::new(camera),
        inbox,
        state,
        clock,
        fast_capture_config(),
        StagingDir::new(&staging_dir_path),
    );

    let buf = CaptureBuffer::new();
    let _guard = install_json_subscriber(&buf);

    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        capture.run(task_shutdown).await;
    });

    wait_for_event(&buf, ev::CAPTURE_READY, Duration::from_secs(2)).await;

    // Fire several triggers across the assertion window so the loop is
    // actively recording, submitting, cooling down, polling liveness,
    // and emitting structured events. Anything network-shaped that the
    // capture-side touches would have a chance to fire during this run.
    let triggers = 3;
    for n in 0..triggers {
        sensor_handle.trigger(Utc::now());
        // Allow each recording to complete (clip_duration_secs = 1) plus
        // a small grace.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let completed = buf
            .events()
            .iter()
            .filter(|e| {
                e.get("event").and_then(Value::as_str) == Some(ev::CAPTURE_RECORDING_COMPLETED)
            })
            .count();
        assert!(
            completed > n,
            "expected at least {} recording_completed events by trigger #{n}; saw {completed}",
            n + 1,
        );
    }

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    let hits = rogue.count();
    let codes: Vec<String> = buf
        .events()
        .iter()
        .filter_map(|e| e.get("event").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert_eq!(
        hits,
        0,
        "rogue listener at `{}` received {hits} accept(s) — \
         capture loop opened an unexpected outbound connection\n  events: {codes:?}",
        rogue.addr(),
    );
}
