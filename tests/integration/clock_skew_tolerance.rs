//! T044a — clock skew tolerance.
//!
//! Asserts that absurd `captured_at` / `enqueued_at` timestamps on a
//! pending sidecar (clock fell far back or jumped far forward — see
//! `spec.md` edge case "Clock skew or NTP unavailable") do not stop
//! delivery from completing. The system clock used by the binary is the
//! real `SystemClock`; the skew lives on the sidecar's frozen-in-time
//! fields, which the delivery loop must NOT use as a gate.

#[path = "support/mod.rs"]
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_station_keypair, sample_mp4_bytes, write_test_credentials};
use support::harness::{perchstation_bin_path, write_config_toml};
use support::logs::parse_json_events;

async fn run_skew_case(clip_id: &str, captured_at_iso: &str) -> bool {
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

    let pending = data_dir.path().join("queue/pending");
    std::fs::create_dir_all(&pending).expect("mkdir pending");
    let mp4_bytes = sample_mp4_bytes();
    std::fs::write(pending.join(format!("{clip_id}.mp4")), &mp4_bytes).expect("write mp4");
    let sidecar = json!({
        "clip_id": clip_id,
        "captured_at": captured_at_iso,
        "enqueued_at": captured_at_iso,
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

    let delivered_sidecar = data_dir.path().join("queue/delivered").join(format!("{clip_id}.json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ok = false;
    while Instant::now() < deadline {
        if delivered_sidecar.exists() {
            ok = true;
            break;
        }
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("collect output");

    // Verify the delivered sidecar's delivered_at is *not* the absurd
    // captured_at — it should reflect the station's real clock at upload.
    if ok {
        let entry: Value =
            serde_json::from_slice(&std::fs::read(&delivered_sidecar).expect("read delivered"))
                .expect("parse delivered");
        assert_eq!(entry.get("outcome").and_then(Value::as_str), Some("Delivered"));
        // captured_at is preserved verbatim from the incoming sidecar.
        assert_eq!(
            entry.get("captured_at").and_then(Value::as_str),
            Some(captured_at_iso),
            "captured_at must be preserved on the wire",
        );
        // delivered_at is the runner's wall-clock — i.e., recent.
        let delivered_at = entry
            .get("delivered_at")
            .and_then(Value::as_str)
            .expect("delivered_at on Delivered entry");
        let delivered: chrono::DateTime<chrono::Utc> =
            delivered_at.parse().expect("delivered_at is RFC 3339");
        let now = chrono::Utc::now();
        let drift = (now - delivered).num_seconds().abs();
        assert!(
            drift < 60,
            "delivered_at {delivered_at} should be near now ({now}); drift = {drift}s\n  stderr: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    } else {
        let events = parse_json_events(&output.stderr);
        let codes: Vec<_> =
            events.iter().filter_map(|e| e.get("event").and_then(Value::as_str)).collect();
        eprintln!(
            "skew case {clip_id}/{captured_at_iso} did not deliver; events: {codes:?}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
    ok
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_continues_despite_captured_at_far_in_the_past() {
    let ok = run_skew_case("19000101T000000Z-001", "1900-01-01T00:00:00Z").await;
    assert!(ok, "delivery must complete despite captured_at in the distant past");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_continues_despite_captured_at_far_in_the_future() {
    let ok = run_skew_case("99990101T000000Z-001", "9999-01-01T00:00:00Z").await;
    assert!(ok, "delivery must complete despite captured_at in the distant future");
}
