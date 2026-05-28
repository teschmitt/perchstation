//! T055 — secret redaction in logs and on the wire (RED).
//!
//! Drives a full enrollment + delivery exchange against the fake perchpub
//! under `RUST_LOG=trace` (so the maximum possible amount of detail is
//! emitted), then asserts that **no** stderr line contains:
//!
//!   - the QR's `auth_token` marker;
//!   - the device-generated CSR PEM body;
//!   - the device-generated private-key PEM body.
//!
//! And asserts that the private-key marker appears in **zero** captured
//! HTTP request bodies (the upload multipart payload + the
//! `/enrollment/confirm` JSON body). The `auth_token` and CSR markers are
//! expected only inside `/enrollment/confirm` — they are part of that
//! exchange and travel on the wire; they must never travel through any
//! tracing channel.
//!
//! Covers `contracts/log-events.md` §Field discipline and FR-001's
//! "private key MUST never leave the device".

#[path = "support/mod.rs"]
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use support::fakepub::FakePerchpub;
use support::fixtures::{build_qr_png, sample_mp4_bytes};
use support::harness::{perchstation_bin, perchstation_bin_path, write_config_toml};
use uuid::Uuid;

const AUTH_TOKEN_MARKER: &str = "REDACT-AUTH-TOKEN-T055-FF00FF00FF00FF00";

/// Extract a *single contiguous line* from the PEM body so substring
/// searches survive JSON's `\n` escaping. PEM base64 wraps at 64 chars,
/// so the first body line is a 64-char string that appears verbatim in
/// any reasonable log encoding (JSON, plain text, debug-print, …).
fn pem_first_body_line(pem: &str) -> String {
    pem.lines()
        .find(|line| !line.starts_with("-----") && !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Whole body, lines joined without separators. Only used for HTTP-body
/// assertions where the bytes are not JSON-escaped (the multipart upload
/// part is the raw mp4 stream; the enrollment-confirm CSR field is a JSON
/// string with embedded `\n` — but `RecordedEnrollment.csr_pem` is the
/// *deserialised* value, so the body is intact with real newlines).
fn pem_body_joined(pem: &str) -> String {
    pem.lines().filter(|line| !line.starts_with("-----")).map(str::trim).collect::<String>()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn enrollment_and_delivery_do_not_leak_secrets_to_logs() {
    let pub_ = FakePerchpub::start().await;
    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    // --- Step 1: enrol with a distinctive auth_token marker ---
    let session_id = Uuid::new_v4();
    let qr_png = build_qr_png(session_id, AUTH_TOKEN_MARKER, pub_.ca_pem());
    let qr_path = data_dir.path().join("enroll.png");
    std::fs::write(&qr_path, &qr_png).expect("write QR png");

    let enroll_output = perchstation_bin()
        .args([
            "--config",
            &config_path.display().to_string(),
            "--log-format",
            "json",
            "--log-level",
            "trace",
            "enroll",
            "--qr-source",
            "file",
            "--qr-file",
            &qr_path.display().to_string(),
        ])
        .output()
        .expect("spawn perchstation enroll");
    assert!(
        enroll_output.status.success(),
        "enroll did not succeed: status={:?}\n  stderr (partial): {}",
        enroll_output.status,
        // Print only the first 1 KiB so a hex-dump of the key doesn't
        // flood CI output if the redaction layer is broken.
        String::from_utf8_lossy(&enroll_output.stderr).chars().take(1024).collect::<String>(),
    );

    // --- Step 2: collect the freshly-materialised secrets ---
    let key_pem = std::fs::read_to_string(data_dir.path().join("credentials/station.key"))
        .expect("read station.key");
    // Markers used for assertions: a contiguous first-line of base64 for
    // log scanning (survives JSON's `\n` escaping) and the joined body
    // for HTTP-body scanning (where the bytes already have real
    // newlines).
    let key_line_marker = pem_first_body_line(&key_pem);
    let key_joined_marker = pem_body_joined(&key_pem);
    assert!(!key_line_marker.is_empty(), "key first-line marker is empty");

    let recorded_after_enroll = pub_.recorded();
    assert_eq!(
        recorded_after_enroll.enrollment_requests.len(),
        1,
        "expected one enrollment request",
    );
    let csr_pem = &recorded_after_enroll.enrollment_requests[0].csr_pem;
    let csr_line_marker = pem_first_body_line(csr_pem);
    assert!(!csr_line_marker.is_empty(), "csr first-line marker is empty");

    // --- Step 3: enroll stderr must not contain any marker ---
    let enroll_stderr = String::from_utf8_lossy(&enroll_output.stderr).into_owned();
    assert!(
        !enroll_stderr.contains(AUTH_TOKEN_MARKER),
        "enroll stderr contained auth_token marker — log redaction broken",
    );
    assert!(
        !enroll_stderr.contains(&csr_line_marker),
        "enroll stderr contained CSR PEM body line — log redaction broken",
    );
    assert!(
        !enroll_stderr.contains(&key_line_marker),
        "enroll stderr contained station private-key body line — log redaction broken",
    );

    // --- Step 4: drop a clip and run serve under --log-level trace ---
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
        .arg("--log-level")
        .arg("trace")
        .arg("serve")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn perchstation serve");

    let delivered_sidecar = data_dir.path().join("queue/delivered").join(format!("{clip_id}.json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if delivered_sidecar.exists() {
            break;
        }
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let _ = child.kill().await;
    let serve_output = child.wait_with_output().await.expect("collect output");
    let serve_stderr = String::from_utf8_lossy(&serve_output.stderr).into_owned();

    // --- Step 5: serve stderr must not contain any marker ---
    assert!(
        !serve_stderr.contains(AUTH_TOKEN_MARKER),
        "serve stderr contained auth_token marker — log redaction broken",
    );
    assert!(
        !serve_stderr.contains(&csr_line_marker),
        "serve stderr contained CSR PEM body line — log redaction broken",
    );
    assert!(
        !serve_stderr.contains(&key_line_marker),
        "serve stderr contained station private-key body line — log redaction broken",
    );

    // --- Step 6: the wire-side discipline ---
    let recorded = pub_.recorded();
    assert!(
        !recorded.upload_requests.is_empty(),
        "expected at least one upload to have been recorded",
    );
    for upload in &recorded.upload_requests {
        let as_text = String::from_utf8_lossy(&upload.body_bytes);
        assert!(
            !as_text.contains(&key_joined_marker),
            "upload body contained the station private-key body — FR-001 violated",
        );
        assert!(
            !as_text.contains(&key_line_marker),
            "upload body contained a private-key body line — FR-001 violated",
        );
        // The auth_token shouldn't appear in upload bodies — it only
        // lives in `/enrollment/confirm`.
        assert!(
            !as_text.contains(AUTH_TOKEN_MARKER),
            "upload body contained the auth_token marker — token leaked beyond enrollment",
        );
    }

    // The auth_token + CSR markers ARE expected inside the enrollment
    // request body — they are what that exchange transports. Sanity-check
    // their presence so a regression that accidentally scrubs them on
    // the wire (a different kind of broken) is caught here.
    let enrollment = &recorded.enrollment_requests[0];
    assert_eq!(
        enrollment.auth_token, AUTH_TOKEN_MARKER,
        "auth_token marker should be transmitted verbatim to /enrollment/confirm",
    );
    assert!(
        enrollment.csr_pem.contains(&csr_line_marker),
        "CSR PEM should be transmitted verbatim to /enrollment/confirm",
    );
    // And the private-key body must NEVER appear in the enrollment body.
    assert!(
        !enrollment.csr_pem.contains(&key_joined_marker),
        "private-key body leaked into the CSR PEM — keypair handling broken",
    );
    assert!(
        !enrollment.auth_token.contains(&key_joined_marker),
        "private-key body leaked into the auth_token field — keypair handling broken",
    );
}
