//! T053 — operator status surface (RED).
//!
//! Drives `perchstation status` and `perchstation status --json` across four
//! pre-baked `data_dir` shapes (idle / queue-building / recent-failure /
//! recent-recovery) plus the two enrollment edge states (`missing`,
//! `expired`). The implementation lands in T056 (snapshot) + T057 (command).
//!
//! Covers spec.md §US3 acceptance #1 and FR-014's status surfacing.

#[path = "support/mod.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{
    build_station_cert_with_validity, build_station_keypair, build_test_ca, write_test_credentials,
};
use support::harness::{perchstation_bin, write_config_toml};

/// Build a fresh `data_dir` with a fully-enrolled identity that expires in
/// the year 2099. Returns `(data_dir tempdir, station_id, config_path)`.
fn enrolled_data_dir(pub_url: &str) -> (tempfile::TempDir, Uuid, PathBuf) {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_url);

    let (ca_cert, ca_key, ca_pem) = build_test_ca();
    let station_id = Uuid::new_v4();
    let station_key = build_station_keypair();
    let station_cert_pem = build_station_cert_with_validity(
        &station_key,
        station_id,
        &ca_cert,
        &ca_key,
        (2026, 1, 1),
        (2099, 1, 1),
    );
    write_test_credentials(
        data_dir.path(),
        station_id,
        pub_url,
        &station_key.serialize_pem(),
        &station_cert_pem,
        &ca_pem,
    )
    .expect("write test credentials");

    (data_dir, station_id, config_path)
}

/// Drop a minimal `pending/` sidecar (plus a non-empty mp4) for `clip_id`.
fn write_pending(data_dir: &Path, clip_id: &str, byte_size: u64) {
    let pending = data_dir.join("queue/pending");
    fs::create_dir_all(&pending).expect("mkdir pending");
    let mp4_size = usize::try_from(byte_size).expect("byte_size fits in usize");
    fs::write(pending.join(format!("{clip_id}.mp4")), vec![0u8; mp4_size]).expect("write mp4");
    let sidecar = json!({
        "clip_id": clip_id,
        "captured_at": "2026-05-27T06:00:00Z",
        "enqueued_at": "2026-05-27T06:00:00Z",
        "byte_size": byte_size,
        "attempts": 0u32,
    });
    fs::write(
        pending.join(format!("{clip_id}.json")),
        serde_json::to_vec_pretty(&sidecar).unwrap(),
    )
    .expect("write sidecar");
}

/// Drop a Delivered sidecar (post-upload, no mp4) into `delivered/`.
#[allow(clippy::too_many_arguments)]
fn write_delivered(
    data_dir: &Path,
    clip_id: &str,
    captured_at_iso: &str,
    delivered_at_iso: &str,
    classify_task_id: Uuid,
    classify_status: &str,
) {
    let delivered = data_dir.join("queue/delivered");
    fs::create_dir_all(&delivered).expect("mkdir delivered");
    let sidecar = json!({
        "clip_id": clip_id,
        "captured_at": captured_at_iso,
        "enqueued_at": captured_at_iso,
        "byte_size": 4096u64,
        "attempts": 1u32,
        "first_attempt_at": delivered_at_iso,
        "last_attempt_at": delivered_at_iso,
        "outcome": "Delivered",
        "classify_task_id": classify_task_id,
        "delivered_at": delivered_at_iso,
        "last_classify_status": classify_status,
    });
    fs::write(
        delivered.join(format!("{clip_id}.json")),
        serde_json::to_vec_pretty(&sidecar).unwrap(),
    )
    .expect("write delivered sidecar");
}

/// Drop an Undeliverable sidecar (terminal failure) into `delivered/`.
fn write_undeliverable(
    data_dir: &Path,
    clip_id: &str,
    captured_at_iso: &str,
    delivered_at_iso: &str,
    error_kind: &str,
    status: Option<u16>,
    message: &str,
) {
    let delivered = data_dir.join("queue/delivered");
    fs::create_dir_all(&delivered).expect("mkdir delivered");
    let mut last_error = json!({
        "kind": error_kind,
        "message": message,
    });
    if let Some(s) = status {
        last_error["status"] = json!(s);
    }
    let sidecar = json!({
        "clip_id": clip_id,
        "captured_at": captured_at_iso,
        "enqueued_at": captured_at_iso,
        "byte_size": 4096u64,
        "attempts": 12u32,
        "first_attempt_at": delivered_at_iso,
        "last_attempt_at": delivered_at_iso,
        "last_error": last_error,
        "outcome": "Undeliverable",
        "delivered_at": delivered_at_iso,
    });
    fs::write(
        delivered.join(format!("{clip_id}.json")),
        serde_json::to_vec_pretty(&sidecar).unwrap(),
    )
    .expect("write undeliverable sidecar");
}

fn run_status_json(config_path: &Path) -> Value {
    let output = perchstation_bin()
        .args([
            "--config",
            &config_path.display().to_string(),
            "--log-format",
            "json",
            "status",
            "--json",
        ])
        .output()
        .expect("spawn perchstation status --json");
    assert!(
        output.status.success(),
        "status --json did not exit 0\n  status: {:?}\n  stderr: {}\n  stdout: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "status --json did not emit a JSON object: {err}\n  stdout: {}",
            String::from_utf8_lossy(&output.stdout),
        )
    })
}

fn run_status_text(config_path: &Path) -> String {
    let output = perchstation_bin()
        .args(["--config", &config_path.display().to_string(), "--log-format", "json", "status"])
        .output()
        .expect("spawn perchstation status");
    assert!(
        output.status.success(),
        "status did not exit 0\n  status: {:?}\n  stderr: {}\n  stdout: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    String::from_utf8(output.stdout).expect("status stdout is UTF-8")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_idle_reports_zero_queue_and_ok_enrollment() {
    let pub_ = FakePerchpub::start().await;
    let (_data, station_id, config_path) = enrolled_data_dir(pub_.url());

    let snapshot = run_status_json(&config_path);

    assert_eq!(
        snapshot["enrollment"]["state"].as_str(),
        Some("ok"),
        "expected enrollment.state=ok in {snapshot}",
    );
    assert_eq!(
        snapshot["enrollment"]["station_id"].as_str(),
        Some(station_id.to_string().as_str()),
        "station_id mismatch in {snapshot}",
    );
    assert_eq!(
        snapshot["enrollment"]["perchpub_url"].as_str(),
        Some(pub_.url()),
        "perchpub_url mismatch in {snapshot}",
    );
    assert!(
        snapshot["enrollment"]["cert_not_after"].is_string(),
        "cert_not_after missing in {snapshot}",
    );
    assert_eq!(snapshot["queue"]["pending"].as_u64(), Some(0));
    assert_eq!(snapshot["queue"]["inflight"].as_u64(), Some(0));
    assert_eq!(snapshot["queue"]["bytes_on_disk"].as_u64(), Some(0));
    assert!(snapshot["last_success"].is_null());
    assert!(snapshot["last_failure"].is_null());
    assert!(
        snapshot["recent"].as_array().is_some_and(Vec::is_empty),
        "recent should be an empty array in idle state; got {}",
        snapshot["recent"],
    );

    let text = run_status_text(&config_path);
    assert!(text.contains("Enrollment:"), "text missing Enrollment heading:\n{text}");
    assert!(text.contains("OK"), "idle text should report OK enrollment:\n{text}");
    assert!(text.contains("Queue depth:"), "text missing Queue depth heading:\n{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_queue_building_reports_pending_depth() {
    let pub_ = FakePerchpub::start().await;
    let (data_dir, _station_id, config_path) = enrolled_data_dir(pub_.url());

    for n in 0..3 {
        let clip_id = format!("20260527T06000{n}Z-001");
        write_pending(data_dir.path(), &clip_id, 4096);
    }

    let snapshot = run_status_json(&config_path);
    assert_eq!(snapshot["queue"]["pending"].as_u64(), Some(3));
    assert_eq!(snapshot["queue"]["inflight"].as_u64(), Some(0));
    assert!(
        snapshot["queue"]["bytes_on_disk"].as_u64().is_some_and(|b| b >= 3 * 4096),
        "expected bytes_on_disk >= 12288, got {}",
        snapshot["queue"]["bytes_on_disk"],
    );
    assert!(snapshot["last_success"].is_null(), "no last_success expected with no deliveries");

    let text = run_status_text(&config_path);
    assert!(text.contains("3 clips"), "queue-building text should mention 3 clips:\n{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_recent_failure_reports_last_failure_and_no_success() {
    let pub_ = FakePerchpub::start().await;
    let (data_dir, _station_id, config_path) = enrolled_data_dir(pub_.url());

    write_undeliverable(
        data_dir.path(),
        "20260526T221455Z-007",
        "2026-05-26T22:14:55Z",
        "2026-05-26T22:14:55Z",
        "http_status",
        Some(503),
        "Service Unavailable",
    );

    let snapshot = run_status_json(&config_path);
    assert_eq!(snapshot["queue"]["pending"].as_u64(), Some(0));
    let failure = &snapshot["last_failure"];
    assert!(!failure.is_null(), "last_failure should be set; got {snapshot}");
    assert_eq!(failure["clip_id"].as_str(), Some("20260526T221455Z-007"));
    assert_eq!(failure["status"].as_u64(), Some(503));
    assert_eq!(failure["kind"].as_str(), Some("http_status"));
    assert!(snapshot["last_success"].is_null(), "no successful delivery should be reported");

    let text = run_status_text(&config_path);
    assert!(
        text.contains("Last failure"),
        "recent-failure text should mention 'Last failure':\n{text}",
    );
    assert!(text.contains("503"), "recent-failure text should mention 503:\n{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_recent_recovery_reports_last_success_and_recent_list() {
    let pub_ = FakePerchpub::start().await;
    let (data_dir, _station_id, config_path) = enrolled_data_dir(pub_.url());

    write_delivered(
        data_dir.path(),
        "20260527T062500Z-001",
        "2026-05-27T06:25:00Z",
        "2026-05-27T06:25:00Z",
        Uuid::new_v4(),
        "Queued",
    );
    write_delivered(
        data_dir.path(),
        "20260527T062800Z-001",
        "2026-05-27T06:28:00Z",
        "2026-05-27T06:28:00Z",
        Uuid::new_v4(),
        "Processing",
    );
    let last_id = Uuid::new_v4();
    write_delivered(
        data_dir.path(),
        "20260527T063108Z-001",
        "2026-05-27T06:31:08Z",
        "2026-05-27T06:31:08Z",
        last_id,
        "Success",
    );

    let snapshot = run_status_json(&config_path);
    let success = &snapshot["last_success"];
    assert!(!success.is_null(), "last_success should be set; got {snapshot}");
    assert_eq!(success["clip_id"].as_str(), Some("20260527T063108Z-001"));
    assert_eq!(success["classify_status"].as_str(), Some("Success"));
    assert_eq!(success["classify_task_id"].as_str(), Some(last_id.to_string().as_str()));
    assert!(snapshot["last_failure"].is_null(), "no failures expected; got {snapshot}");
    let recent = snapshot["recent"].as_array().expect("recent is array");
    assert_eq!(recent.len(), 3, "expected three recent deliveries, got {recent:?}");
    // Most recent first.
    assert_eq!(recent[0]["clip_id"].as_str(), Some("20260527T063108Z-001"));
    assert_eq!(recent[0]["classify_status"].as_str(), Some("Success"));
    assert_eq!(recent[1]["clip_id"].as_str(), Some("20260527T062800Z-001"));
    assert_eq!(recent[2]["clip_id"].as_str(), Some("20260527T062500Z-001"));

    let text = run_status_text(&config_path);
    assert!(
        text.contains("Last success"),
        "recent-recovery text should mention 'Last success':\n{text}",
    );
    assert!(
        text.contains("Last 3 deliveries"),
        "recent-recovery text should list recent deliveries:\n{text}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_missing_credentials_reports_missing_state() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), "https://example.invalid");

    let snapshot = run_status_json(&config_path);
    assert_eq!(snapshot["enrollment"]["state"].as_str(), Some("missing"));
    assert!(snapshot["enrollment"]["station_id"].is_null());
    assert!(snapshot["enrollment"]["cert_not_after"].is_null());
    assert_eq!(snapshot["queue"]["pending"].as_u64(), Some(0));

    let text = run_status_text(&config_path);
    assert!(
        text.contains("not enrolled") || text.contains("missing"),
        "missing-creds text should call out the missing enrollment:\n{text}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_expired_cert_reports_expired_state() {
    let pub_ = FakePerchpub::start().await;
    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    let (ca_cert, ca_key, ca_pem) = build_test_ca();
    let station_id = Uuid::new_v4();
    let station_key = build_station_keypair();
    // Cert that expired in 2024 — well before the current date for any
    // realistic test run.
    let station_cert_pem = build_station_cert_with_validity(
        &station_key,
        station_id,
        &ca_cert,
        &ca_key,
        (2023, 1, 1),
        (2024, 1, 1),
    );
    write_test_credentials(
        data_dir.path(),
        station_id,
        pub_.url(),
        &station_key.serialize_pem(),
        &station_cert_pem,
        &ca_pem,
    )
    .expect("write credentials");

    let snapshot = run_status_json(&config_path);
    assert_eq!(
        snapshot["enrollment"]["state"].as_str(),
        Some("expired"),
        "expected enrollment.state=expired in {snapshot}",
    );
    assert_eq!(
        snapshot["enrollment"]["station_id"].as_str(),
        Some(station_id.to_string().as_str()),
    );
    assert_eq!(snapshot["enrollment"]["cert_not_after"].as_str(), Some("2024-01-01T00:00:00Z"),);

    let text = run_status_text(&config_path);
    assert!(
        text.contains("expired") || text.contains("EXPIRED"),
        "expired-cert text should call out the expiry:\n{text}",
    );
}
