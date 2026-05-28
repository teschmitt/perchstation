//! T054 — outbound allowlist invariant (RED).
//!
//! Verifies that during a normal delivery cycle the station opens TCP
//! connections only to its configured perchpub authority. A "rogue"
//! listener on a different ephemeral port is bound for the duration of
//! the test; if the station ever tries to follow a redirect, mis-parse
//! a URL, or contact `127.0.0.1:<rogue>` for any other reason, the
//! listener's `accept_count` increments and the test fails.
//!
//! Covers spec.md §US3 acceptance #3 and SC-007.

#[path = "support/mod.rs"]
mod support;

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_station_keypair, sample_mp4_bytes, write_test_credentials};
use support::harness::{perchstation_bin_path, write_config_toml};
use support::logs::{event_codes, parse_json_events};

/// Trivial TCP listener that increments a counter every time it accepts
/// a connection. Used to detect any rogue outbound from the station —
/// any successful accept here means the station contacted a host that
/// isn't its configured perchpub authority.
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
                // Immediately close so a buggy client doesn't hang inside
                // the connect/recv handshake.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn serve_contacts_no_host_outside_perchpub_authority() {
    let pub_ = FakePerchpub::start().await;
    let rogue = RogueListener::start().await;

    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    let station_id = Uuid::new_v4();
    let station_key = build_station_keypair();
    let station_cert_pem = pub_.mint_station_cert(&station_key, station_id);
    write_test_credentials(
        data_dir.path(),
        station_id,
        pub_.url(),
        &station_key.serialize_pem(),
        &station_cert_pem,
        pub_.ca_pem(),
    )
    .expect("write credentials");

    // Drop a handful of clips so the delivery loop runs multiple uploads
    // in quick succession — increases the surface area for any accidental
    // off-allowlist connect (e.g. broken retry path, redirect-follow bug,
    // DNS-mistargeted clone of the URL).
    let pending = data_dir.path().join("queue/pending");
    std::fs::create_dir_all(&pending).expect("mkdir pending");
    let mp4_bytes = sample_mp4_bytes();
    let clip_count = 5;
    let mut clip_ids = Vec::with_capacity(clip_count);
    for n in 0..clip_count {
        let clip_id = format!("20260527T1200{n:02}Z-001");
        std::fs::write(pending.join(format!("{clip_id}.mp4")), &mp4_bytes).expect("write mp4");
        let sidecar = serde_json::json!({
            "clip_id": clip_id,
            "captured_at": format!("2026-05-27T12:00:{n:02}Z"),
            "enqueued_at": format!("2026-05-27T12:00:{n:02}Z"),
            "byte_size": mp4_bytes.len() as u64,
            "attempts": 0u32,
        });
        std::fs::write(
            pending.join(format!("{clip_id}.json")),
            serde_json::to_vec_pretty(&sidecar).unwrap(),
        )
        .expect("write sidecar");
        clip_ids.push(clip_id);
    }

    let mut child = tokio::process::Command::new(perchstation_bin_path())
        .arg("--config")
        .arg(&config_path)
        .arg("--log-format")
        .arg("json")
        .arg("serve")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn perchstation serve");

    // Wait until every clip has reached delivered/.
    let delivered_dir = data_dir.path().join("queue/delivered");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let all_done = clip_ids.iter().all(|id| delivered_dir.join(format!("{id}.json")).exists());
        if all_done {
            break;
        }
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("collect output");

    let rogue_hits = rogue.count();
    let events = parse_json_events(&output.stderr);
    let codes = event_codes(&events);

    assert_eq!(
        rogue_hits,
        0,
        "rogue listener at `{}` received {rogue_hits} accept(s) — \
         station contacted a host outside the perchpub allowlist\n  events: {codes:?}\n  stderr: {}",
        rogue.addr(),
        String::from_utf8_lossy(&output.stderr),
    );

    let recorded = pub_.recorded();
    assert!(
        recorded.upload_requests.len() >= clip_count,
        "expected at least {clip_count} uploads to fake perchpub, got {}\n  events: {codes:?}",
        recorded.upload_requests.len(),
    );
}

/// Confirms the redirect-policy strengthening from T060: even if the
/// fake perchpub returns a 3xx redirect pointing at a rogue host, the
/// station must NOT follow it. The rogue listener stays silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn upload_does_not_follow_3xx_redirect_to_rogue_host() {
    let pub_ = FakePerchpub::start().await;
    let rogue = RogueListener::start().await;

    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    let station_id = Uuid::new_v4();
    let station_key = build_station_keypair();
    let station_cert_pem = pub_.mint_station_cert(&station_key, station_id);
    write_test_credentials(
        data_dir.path(),
        station_id,
        pub_.url(),
        &station_key.serialize_pem(),
        &station_cert_pem,
        pub_.ca_pem(),
    )
    .expect("write credentials");

    // Have fake perchpub redirect all uploads to the rogue authority.
    let rogue_url = format!("https://{}/api/v1/upload/", rogue.addr());
    pub_.redirect_uploads_to(rogue_url.clone());

    let pending = data_dir.path().join("queue/pending");
    std::fs::create_dir_all(&pending).expect("mkdir pending");
    let mp4_bytes = sample_mp4_bytes();
    let clip_id = "20260527T120000Z-001";
    std::fs::write(pending.join(format!("{clip_id}.mp4")), &mp4_bytes).expect("write mp4");
    let sidecar = serde_json::json!({
        "clip_id": clip_id,
        "captured_at": "2026-05-27T12:00:00Z",
        "enqueued_at": "2026-05-27T12:00:00Z",
        "byte_size": mp4_bytes.len() as u64,
        "attempts": 0u32,
    });
    std::fs::write(
        pending.join(format!("{clip_id}.json")),
        serde_json::to_vec_pretty(&sidecar).unwrap(),
    )
    .expect("write sidecar");

    let mut child = tokio::process::Command::new(perchstation_bin_path())
        .arg("--config")
        .arg(&config_path)
        .arg("--log-format")
        .arg("json")
        .arg("serve")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn perchstation serve");

    let delivered_dir = data_dir.path().join("queue/delivered");
    let delivered_sidecar = delivered_dir.join(format!("{clip_id}.json"));
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if delivered_sidecar.exists() {
            break;
        }
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("collect output");

    let rogue_hits = rogue.count();
    let events = parse_json_events(&output.stderr);
    let codes = event_codes(&events);

    assert_eq!(
        rogue_hits,
        0,
        "station followed a redirect to rogue host `{}` ({rogue_hits} accept(s))\n  events: {codes:?}\n  stderr: {}",
        rogue.addr(),
        String::from_utf8_lossy(&output.stderr),
    );

    // The upload should either have been classified transient or terminal,
    // but NEVER reported as a success — perchpub redirected away from us.
    let succeeded = events.iter().any(|ev: &Value| {
        ev.get("event").and_then(Value::as_str) == Some("delivery.upload_succeeded")
            && ev.get("clip_id").and_then(Value::as_str) == Some(clip_id)
    });
    assert!(
        !succeeded,
        "upload should not have succeeded when perchpub redirected to a rogue host;\n  events: {codes:?}\n  stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}
