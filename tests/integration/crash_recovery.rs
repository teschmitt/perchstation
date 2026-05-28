//! T043 — boot reconciliation after a crash mid-upload.
//!
//! Simulates a crash by pre-populating `queue/inflight/` with a
//! `<clip-id>.mp4` + `<clip-id>.json` pair (i.e., the state a process
//! kill leaves on disk after `transition_inflight` ran but
//! `transition_delivered` did not). Spawns a fresh `perchstation serve`
//! and asserts:
//!
//! 1. `queue.recovered_inflight` fires for the entry, before
//!    `service.ready`.
//! 2. The clip ultimately lands in `queue/delivered/`.
//! 3. No orphan `.mp4` survives in `queue/inflight/`.
//!
//! Spec coverage: US2 acceptance #5 / FR-002 / FR-004.

#[path = "support/mod.rs"]
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_station_keypair, sample_mp4_bytes, write_test_credentials};
use support::harness::{perchstation_bin_path, write_config_toml};
use support::logs::{event_codes, find_event, find_events, parse_json_events};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // linear setup → assert sequence; splitting helps neither
async fn boot_reconciliation_requeues_inflight_pair_and_completes_delivery() {
    let pub_ = FakePerchpub::start().await;

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

    // Pre-populate inflight/ as if a previous process crashed mid-upload.
    let inflight = data_dir.path().join("queue/inflight");
    std::fs::create_dir_all(&inflight).expect("mkdir inflight");
    let clip_id = "20260527T120000Z-001";
    let mp4_bytes = sample_mp4_bytes();
    std::fs::write(inflight.join(format!("{clip_id}.mp4")), &mp4_bytes).expect("write mp4");
    // Sidecar has attempts=1, first/last_attempt_at set, no next_attempt_after,
    // no outcome — mid-flight state.
    let sidecar = json!({
        "clip_id": clip_id,
        "captured_at": "2026-05-27T12:00:00Z",
        "enqueued_at": "2026-05-27T12:00:00Z",
        "byte_size": mp4_bytes.len() as u64,
        "attempts": 1u32,
        "first_attempt_at": "2026-05-27T12:00:01Z",
        "last_attempt_at": "2026-05-27T12:00:01Z",
    });
    std::fs::write(
        inflight.join(format!("{clip_id}.json")),
        serde_json::to_vec_pretty(&sidecar).unwrap(),
    )
    .expect("write sidecar");

    // ---- spawn fresh serve ----
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

    let delivered_sidecar = data_dir.path().join("queue/delivered").join(format!("{clip_id}.json"));
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut delivered = false;
    while Instant::now() < deadline {
        if delivered_sidecar.exists() {
            delivered = true;
            break;
        }
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("collect output");
    let events = parse_json_events(&output.stderr);
    let codes = event_codes(&events);
    let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        delivered,
        "expected clip to land in delivered/ after recovery\n  events: {codes:?}\n  stderr: {stderr_text}",
    );

    // queue.recovered_inflight fires for the clip with the matching id.
    let recovered = find_events(&events, "queue.recovered_inflight");
    assert_eq!(
        recovered.len(),
        1,
        "expected exactly one queue.recovered_inflight, got {recovered:?}\n  events: {codes:?}",
    );
    assert_eq!(recovered[0].get("clip_id").and_then(Value::as_str), Some(clip_id));

    // service.ready fires AFTER queue.recovered_inflight — the contract is
    // "reconcile then announce ready".
    let recovered_idx = codes
        .iter()
        .position(|c| c == "queue.recovered_inflight")
        .expect("recovered_inflight in codes");
    let ready_idx = codes
        .iter()
        .position(|c| c == "service.ready")
        .unwrap_or_else(|| panic!("missing service.ready; codes: {codes:?}"));
    assert!(
        recovered_idx < ready_idx,
        "queue.recovered_inflight must precede service.ready: {codes:?}",
    );

    // No orphan mp4 in inflight/.
    assert!(
        !inflight.join(format!("{clip_id}.mp4")).exists(),
        "orphan mp4 in inflight/ — boot reconciliation should have moved it",
    );
    assert!(
        !inflight.join(format!("{clip_id}.json")).exists(),
        "orphan sidecar in inflight/ — boot reconciliation should have moved it",
    );

    // The delivered sidecar carries outcome = Delivered (clip was re-uploaded
    // and succeeded the second time).
    let entry: Value =
        serde_json::from_slice(&std::fs::read(&delivered_sidecar).expect("read delivered"))
            .expect("parse delivered");
    assert_eq!(entry.get("outcome").and_then(Value::as_str), Some("Delivered"));
    // attempts should be at least 2 (the recovered original + the new attempt).
    let attempts = entry.get("attempts").and_then(Value::as_u64).unwrap_or(0);
    assert!(attempts >= 2, "expected at least 2 attempts after recovery; got {attempts}");

    // service.ready emits pending_at_start reflecting the recovered entry —
    // i.e., > 0. Belt-and-braces against a regression that runs reconcile
    // after counting.
    let ready = find_event(&events, "service.ready").expect("service.ready event");
    let pending_at_start =
        ready.get("pending_at_start").and_then(Value::as_u64).expect("pending_at_start field");
    assert!(
        pending_at_start >= 1,
        "service.ready.pending_at_start should reflect the recovered entry: {ready}",
    );
}
