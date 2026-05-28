//! T042 — permanent (4xx terminal) failure for a single clip.
//!
//! Verifies `spec.md` US2 acceptance #4 and FR-008: a 422 response for a
//! specific clip transitions that clip to `delivered/` with `outcome:
//! Undeliverable`, emits `delivery.upload_terminal` with `status = 422`,
//! and lets the remaining clips finish uploading normally.

#[path = "support/mod.rs"]
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_station_keypair, sample_mp4_bytes, write_test_credentials};
use support::harness::perchstation_bin_path;
use support::harness::write_config_toml;
use support::logs::{event_codes, find_events, parse_json_events};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // linear setup → assert sequence; splitting helps neither
async fn permanent_failure_for_one_clip_does_not_block_the_rest() {
    let pub_ = FakePerchpub::start().await;

    let cursed_clip = "20260527T120000Z-001";
    let good_a = "20260527T120100Z-002";
    let good_b = "20260527T120200Z-003";

    pub_.fail_uploads_terminal_for(format!("{cursed_clip}.mp4"), 422);

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
    for (clip_id, captured) in [
        (cursed_clip, "2026-05-27T12:00:00Z"),
        (good_a, "2026-05-27T12:01:00Z"),
        (good_b, "2026-05-27T12:02:00Z"),
    ] {
        std::fs::write(pending.join(format!("{clip_id}.mp4")), &mp4_bytes).expect("write mp4");
        let sidecar = json!({
            "clip_id": clip_id,
            "captured_at": captured,
            "enqueued_at": captured,
            "byte_size": mp4_bytes.len() as u64,
            "attempts": 0u32,
        });
        std::fs::write(
            pending.join(format!("{clip_id}.json")),
            serde_json::to_vec_pretty(&sidecar).unwrap(),
        )
        .expect("write sidecar");
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

    let delivered = data_dir.path().join("queue/delivered");
    let deadline = Instant::now() + Duration::from_secs(30); // hard cap per T042
    let mut all_done = false;
    while Instant::now() < deadline {
        let cursed_done = delivered.join(format!("{cursed_clip}.json")).exists();
        let a_done = delivered.join(format!("{good_a}.json")).exists();
        let b_done = delivered.join(format!("{good_b}.json")).exists();
        if cursed_done && a_done && b_done {
            all_done = true;
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
        all_done,
        "expected all three clips in delivered/ within 30 s\n  events: {codes:?}\n  stderr: {stderr_text}",
    );

    let cursed: Value = serde_json::from_slice(
        &std::fs::read(delivered.join(format!("{cursed_clip}.json"))).expect("read cursed"),
    )
    .expect("parse cursed sidecar");
    assert_eq!(
        cursed.get("outcome").and_then(Value::as_str),
        Some("Undeliverable"),
        "cursed clip outcome != Undeliverable: {cursed}",
    );
    let last_err = cursed
        .get("last_error")
        .and_then(Value::as_object)
        .expect("cursed clip should record last_error");
    assert_eq!(last_err.get("status").and_then(Value::as_u64), Some(422));

    for ok in [good_a, good_b] {
        let entry: Value = serde_json::from_slice(
            &std::fs::read(delivered.join(format!("{ok}.json"))).expect("read good"),
        )
        .expect("parse good sidecar");
        assert_eq!(
            entry.get("outcome").and_then(Value::as_str),
            Some("Delivered"),
            "good clip {ok} outcome != Delivered: {entry}",
        );
    }

    // delivery.upload_terminal must fire for the cursed clip with status=422.
    let terminals = find_events(&events, "delivery.upload_terminal");
    let cursed_terminal = terminals
        .iter()
        .find(|ev| ev.get("clip_id").and_then(Value::as_str) == Some(cursed_clip))
        .unwrap_or_else(|| {
            panic!("expected delivery.upload_terminal for {cursed_clip}; events: {codes:?}")
        });
    assert_eq!(cursed_terminal.get("status").and_then(Value::as_u64), Some(422));
    assert!(cursed_terminal.get("kind").and_then(Value::as_str).is_some());
    assert!(cursed_terminal.get("attempt").and_then(Value::as_u64).is_some());

    // No delivery.upload_terminal for the well-behaved clips.
    for ok in [good_a, good_b] {
        let bogus =
            terminals.iter().any(|ev| ev.get("clip_id").and_then(Value::as_str) == Some(ok));
        assert!(!bogus, "good clip {ok} should not have fired delivery.upload_terminal");
    }
}
