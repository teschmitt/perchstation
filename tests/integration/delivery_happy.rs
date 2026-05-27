//! T021 — happy-path delivery, RED.
//!
//! Pre-populates credentials, drops a clip into `queue/pending/`, runs
//! `perchstation serve` until the clip lands in `queue/delivered/`,
//! then SIGKILLs the process and inspects on-disk + log state.
//!
//! Currently fails because `commands::serve::run` is `unimplemented!()` —
//! the subprocess panics, the delivered sidecar never appears, and no
//! events fire.
//!
//! Covers spec.md §US1 acceptance #2 and #3.

#[path = "support/mod.rs"]
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_station_keypair, sample_mp4_bytes, write_test_credentials};
use support::harness::{perchstation_bin_path, write_config_toml};
use support::logs::{event_codes, parse_json_events};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // linear setup → assert sequence; splitting helps neither
async fn delivery_happy_path() {
    let pub_ = FakePerchpub::start().await;
    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    // --- pre-populate credentials (no enrollment step) ---
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

    // --- drop a clip into queue/pending/ ---
    let clip_id = "20260527T120000Z-001";
    let pending = data_dir.path().join("queue/pending");
    std::fs::create_dir_all(&pending).expect("mkdir queue/pending");
    let mp4_bytes = sample_mp4_bytes();
    let mp4_path = pending.join(format!("{clip_id}.mp4"));
    std::fs::write(&mp4_path, &mp4_bytes).expect("write mp4");
    let sidecar = json!({
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

    // --- spawn `serve` via tokio so we can kill it once delivery settles ---
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

    // --- poll for the delivered sidecar (or process death, in RED) ---
    let delivered_dir = data_dir.path().join("queue/delivered");
    let delivered_sidecar = delivered_dir.join(format!("{clip_id}.json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if delivered_sidecar.exists() {
            break;
        }
        if child.try_wait().expect("try_wait").is_some() {
            // RED: serve panicked; no point spinning
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Terminate (no-op if already dead from the unimplemented!() panic).
    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("collect subprocess output");

    let events = parse_json_events(&output.stderr);
    let codes = event_codes(&events);

    // --- on-disk state: clip ended up in delivered/, mp4 unlinked ---
    let pending_mp4 = pending.join(format!("{clip_id}.mp4"));
    let pending_sidecar = pending.join(format!("{clip_id}.json"));
    let inflight_dir = data_dir.path().join("queue/inflight");
    let inflight_sidecar = inflight_dir.join(format!("{clip_id}.json"));
    let delivered_mp4 = delivered_dir.join(format!("{clip_id}.mp4"));

    let fs_state = || {
        format!(
            "pending_mp4={}, pending_sidecar={}, inflight_sidecar={}, delivered_sidecar={}, delivered_mp4={}",
            pending_mp4.exists(),
            pending_sidecar.exists(),
            inflight_sidecar.exists(),
            delivered_sidecar.exists(),
            delivered_mp4.exists(),
        )
    };

    assert!(
        delivered_sidecar.exists(),
        "delivered sidecar missing\n  events: {codes:?}\n  fs: {}\n  stderr: {}",
        fs_state(),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!pending_mp4.exists(), "pending mp4 still present: {}", pending_mp4.display());
    assert!(!pending_sidecar.exists(), "pending sidecar still present");
    assert!(!inflight_sidecar.exists(), "inflight sidecar still present");
    assert!(
        !delivered_mp4.exists(),
        "delivered mp4 still present (should be unlinked before sidecar rename)",
    );

    // --- delivered sidecar carries the post-upload fields ---
    let entry: Value =
        serde_json::from_slice(&std::fs::read(&delivered_sidecar).expect("read delivered sidecar"))
            .expect("parse delivered sidecar");
    assert_eq!(
        entry.get("outcome").and_then(Value::as_str),
        Some("Delivered"),
        "outcome != Delivered in delivered sidecar: {entry}",
    );
    assert!(entry.get("classify_task_id").is_some(), "classify_task_id missing");
    assert!(entry.get("delivered_at").is_some(), "delivered_at missing");

    // --- log events fired (order is allowed to vary across producers) ---
    for want in [
        "service.ready",
        "delivery.attempt_started",
        "delivery.upload_succeeded",
        "classify.polled",
        "classify.terminal",
    ] {
        assert!(
            codes.iter().any(|c| c == want),
            "missing event {want} in {codes:?}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    // --- fake perchpub received exactly one upload, matching byte_size ---
    let recorded = pub_.recorded();
    assert_eq!(recorded.upload_requests.len(), 1, "expected one upload, got {recorded:?}");
    assert_eq!(
        recorded.upload_requests[0].byte_size,
        mp4_bytes.len(),
        "uploaded byte_size mismatch",
    );
}
