//! T020 — happy-path enrollment, RED.
//!
//! Drives `perchstation enroll --qr-source file --qr-file <png>` against a
//! fresh fake perchpub. Currently fails because `commands::enroll::run` is
//! `unimplemented!()` — no events fire and no credentials are written.
//!
//! Covers spec.md §US1 acceptance #1.

#[path = "support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt;

use serde_json::Value;
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::build_qr_png;
use support::harness::{perchstation_bin, write_config_toml};
use support::logs::{event_codes, parse_json_events};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enrollment_happy_path() {
    let pub_ = FakePerchpub::start().await;
    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    let session_id = Uuid::new_v4();
    let auth_token = "test-auth-token-T020";
    let qr_png = build_qr_png(session_id, auth_token, pub_.ca_pem());
    let qr_path = data_dir.path().join("enroll.png");
    std::fs::write(&qr_path, &qr_png).expect("write QR png");

    // `assert_cmd::Command::output` is synchronous, but the multi-thread
    // runtime keeps the axum task on the other worker so the fake
    // perchpub can serve the enrollment-confirm call.
    let output = perchstation_bin()
        .args([
            "--config",
            &config_path.display().to_string(),
            "--log-format",
            "json",
            "enroll",
            "--qr-source",
            "file",
            "--qr-file",
            &qr_path.display().to_string(),
        ])
        .output()
        .expect("spawn perchstation enroll");

    let events = parse_json_events(&output.stderr);
    let codes = event_codes(&events);

    assert!(
        output.status.success(),
        "perchstation enroll did not exit 0\n  status: {:?}\n  events: {codes:?}\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    // --- credentials/identity.json exists and has the documented shape ---
    let creds = data_dir.path().join("credentials");
    let identity_text = std::fs::read_to_string(creds.join("identity.json"))
        .expect("read credentials/identity.json");
    let identity: Value =
        serde_json::from_str(&identity_text).expect("identity.json is valid JSON");
    assert!(identity.get("station_id").is_some(), "identity.station_id missing: {identity}");
    assert!(identity.get("enrolled_at").is_some(), "identity.enrolled_at missing: {identity}");
    assert!(identity.get("perchpub_url").is_some(), "identity.perchpub_url missing: {identity}");
    assert_eq!(
        identity.get("perchpub_url").and_then(Value::as_str),
        Some(pub_.url()),
        "identity.perchpub_url does not match config",
    );

    // --- station.crt is parseable PEM ---
    let cert_pem =
        std::fs::read_to_string(creds.join("station.crt")).expect("read credentials/station.crt");
    assert!(
        cert_pem.contains("-----BEGIN CERTIFICATE-----"),
        "station.crt is not a PEM-encoded certificate:\n{cert_pem}",
    );

    // --- station.key exists with mode 0600 ---
    let key_meta =
        std::fs::metadata(creds.join("station.key")).expect("stat credentials/station.key");
    let mode = key_meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "station.key permissions are 0o{mode:o}, expected 0o600");

    // --- ca_chain.pem matches what the fake perchpub advertises ---
    let ca_on_disk =
        std::fs::read_to_string(creds.join("ca_chain.pem")).expect("read credentials/ca_chain.pem");
    assert_eq!(
        ca_on_disk,
        pub_.ca_pem(),
        "credentials/ca_chain.pem does not equal the fake perchpub's CA chain",
    );

    // --- log events fire in the documented order ---
    // Per contracts/log-events.md §Enrollment:
    //   enrollment.qr_decoded → enrollment.csr_generated → enrollment.sent → enrollment.persisted
    let expected = [
        "enrollment.qr_decoded",
        "enrollment.csr_generated",
        "enrollment.sent",
        "enrollment.persisted",
    ];
    let mut cursor = 0;
    for want in expected {
        let offset = codes[cursor..]
            .iter()
            .position(|code| code == want)
            .unwrap_or_else(|| panic!("missing event {want}; saw {codes:?}"));
        cursor += offset + 1;
    }

    // --- fake perchpub recorded exactly one enrollment request ---
    let recorded = pub_.recorded();
    assert_eq!(
        recorded.enrollment_requests.len(),
        1,
        "expected exactly one /enrollment/confirm call, got {:?}",
        recorded.enrollment_requests,
    );
    let req = &recorded.enrollment_requests[0];
    assert_eq!(req.session_id, session_id.to_string(), "session_id mismatch in recorded request");
    assert_eq!(req.auth_token, auth_token, "auth_token mismatch in recorded request");
    assert!(req.csr_pem.contains("BEGIN CERTIFICATE REQUEST"), "recorded csr_pem is not PEM");
}
