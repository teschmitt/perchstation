//! T022a — enrollment fails when perchpub returns 422, RED.
//!
//! The fake perchpub is configured to return an `HTTPValidationError`
//! body on `POST /enrollment/confirm/{session_id}`. The station must:
//!
//! - exit 76 (UNRECOVERABLE),
//! - emit `enrollment.session_invalid` carrying `status = 422`,
//! - leave the on-disk `credentials/` directory absent.
//!
//! Currently RED because `commands::enroll::run` is `unimplemented!()`.
//!
//! ⚠ Contract drift: the constant `events::ENROLLMENT_SESSION_INVALID`
//! exists in `crates/perchstation-core/src/observability/tracing.rs` but
//! is not yet listed in `specs/001-clip-delivery/contracts/log-events.md`.
//! Group 3 (T025) resolves the drift — either by adding the row to the
//! contract or by re-routing through `enrollment.failed` with
//! `kind = "session_invalid"`. This test asserts on the constant's
//! current name; if Group 3 picks the reroute, update both the producer
//! and this test together.
//!
//! Covers spec.md edge case "Enrollment session expires before the station
//! can confirm" and the 4xx branch of `contracts/perchpub-api.md` §1.

#[path = "support/mod.rs"]
mod support;

use serde_json::Value;
use uuid::Uuid;

use support::fakepub::{EnrollmentResponseMode, FakePerchpub};
use support::fixtures::build_qr_png;
use support::harness::{perchstation_bin, write_config_toml};
use support::logs::{find_event, parse_json_events};

const PERCHSTATION_EXIT_UNRECOVERABLE: i32 = 76;
const ENROLLMENT_SESSION_INVALID: &str = "enrollment.session_invalid";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_surfaces_perchpub_422_as_session_invalid() {
    let pub_ = FakePerchpub::start().await;
    pub_.set_enrollment_response(EnrollmentResponseMode::SessionExpired);

    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    let qr_path = data_dir.path().join("enroll.png");
    let session_id = Uuid::new_v4();
    let auth_token = "test-auth-token-T022a";
    std::fs::write(&qr_path, build_qr_png(session_id, auth_token, pub_.ca_pem()))
        .expect("write QR png");

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
        .expect("run perchstation enroll");

    let events = parse_json_events(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(PERCHSTATION_EXIT_UNRECOVERABLE),
        "expected exit 76 (UNRECOVERABLE) when perchpub returns 422; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let session_invalid = find_event(&events, ENROLLMENT_SESSION_INVALID).unwrap_or_else(|| {
        panic!(
            "missing {ENROLLMENT_SESSION_INVALID} event; saw {:?}",
            events
                .iter()
                .filter_map(|e| e.get("event").and_then(Value::as_str))
                .collect::<Vec<_>>(),
        )
    });
    let status_field = session_invalid.get("status").and_then(Value::as_u64).or_else(|| {
        session_invalid.get("status").and_then(Value::as_str).and_then(|s| s.parse::<u64>().ok())
    });
    assert_eq!(
        status_field,
        Some(422),
        "{ENROLLMENT_SESSION_INVALID} missing/wrong status field: {session_invalid}",
    );

    // No credentials should have been persisted on the failure path.
    let creds_dir = data_dir.path().join("credentials");
    assert!(
        !creds_dir.exists(),
        "credentials/ directory exists after a failed enrollment: {}",
        creds_dir.display(),
    );
}
