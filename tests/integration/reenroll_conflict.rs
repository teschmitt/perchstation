//! T022 — refuse to clobber existing credentials, RED.
//!
//! Two passes against the same pre-populated `credentials/` dir:
//!
//! 1. `perchstation enroll` (no `--force`) must refuse with exit 76 and
//!    emit `enrollment.refused_overwrite` carrying the existing
//!    `station_id`. The on-disk credentials must be byte-for-byte
//!    unchanged.
//! 2. `perchstation enroll --force` must overwrite, emit
//!    `enrollment.persisted`, and additionally emit at least one
//!    WARN-or-higher event that names both the old and the new
//!    `station_id` (operator-visible audit trail per cli.md §enroll).
//!
//! Currently RED because `commands::enroll::run` is `unimplemented!()`:
//! the first pass panics → exit code 101 (not 76), and no events fire.
//!
//! Covers spec.md §US1 acceptance #4 and FR-003.

#[path = "support/mod.rs"]
mod support;

use serde_json::Value;
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_qr_png, build_station_keypair, write_test_credentials};
use support::harness::{perchstation_bin, write_config_toml};
use support::logs::{find_event, parse_json_events};

const PERCHSTATION_EXIT_UNRECOVERABLE: i32 = 76;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // linear two-pass setup; splitting hurts readability
async fn reenroll_refuses_then_force_succeeds() {
    let pub_ = FakePerchpub::start().await;
    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    // Pre-populate the credentials dir with a known station_id so we can
    // assert "unchanged" later and check `existing_station_id` in the
    // refusal event.
    let existing_station_id = Uuid::new_v4();
    let existing_key = build_station_keypair();
    let existing_cert = pub_.mint_station_cert(&existing_key, existing_station_id);
    write_test_credentials(
        data_dir.path(),
        existing_station_id,
        pub_.url(),
        &existing_key.serialize_pem(),
        &existing_cert,
        pub_.ca_pem(),
    )
    .expect("seed credentials");

    let identity_path = data_dir.path().join("credentials/identity.json");
    let identity_before = std::fs::read(&identity_path).expect("read identity before");

    // Build a fresh QR that would, absent the existing-credentials check,
    // successfully enrol the station.
    let qr_path = data_dir.path().join("enroll.png");
    let session_id = Uuid::new_v4();
    let auth_token = "test-auth-token-T022";
    std::fs::write(&qr_path, build_qr_png(session_id, auth_token, pub_.ca_pem()))
        .expect("write QR png");

    // ===== Pass 1: enroll without --force; must refuse =====
    let refusal = perchstation_bin()
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
        .expect("run perchstation enroll (no --force)");

    let refusal_events = parse_json_events(&refusal.stderr);

    assert_eq!(
        refusal.status.code(),
        Some(PERCHSTATION_EXIT_UNRECOVERABLE),
        "expected exit 76 (UNRECOVERABLE) on re-enroll without --force; got {:?}\nstderr: {}",
        refusal.status,
        String::from_utf8_lossy(&refusal.stderr),
    );

    let refused =
        find_event(&refusal_events, "enrollment.refused_overwrite").unwrap_or_else(|| {
            panic!(
                "missing enrollment.refused_overwrite event; saw {:?}",
                refusal_events
                    .iter()
                    .filter_map(|e| e.get("event").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
            )
        });
    assert_eq!(
        refused.get("existing_station_id").and_then(Value::as_str),
        Some(existing_station_id.to_string().as_str()),
        "enrollment.refused_overwrite missing/wrong existing_station_id: {refused}",
    );

    // Credentials must not have been touched.
    let identity_after_refusal = std::fs::read(&identity_path).expect("read identity after");
    assert_eq!(
        identity_before, identity_after_refusal,
        "identity.json changed after a refused re-enroll",
    );

    // ===== Pass 2: enroll --force; must overwrite =====
    let forced = perchstation_bin()
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
            "--force",
        ])
        .output()
        .expect("run perchstation enroll --force");

    let forced_events = parse_json_events(&forced.stderr);

    assert!(
        forced.status.success(),
        "expected exit 0 on enroll --force; got {:?}\nstderr: {}",
        forced.status,
        String::from_utf8_lossy(&forced.stderr),
    );

    let persisted = find_event(&forced_events, "enrollment.persisted").unwrap_or_else(|| {
        panic!(
            "missing enrollment.persisted event after --force; saw {:?}",
            forced_events
                .iter()
                .filter_map(|e| e.get("event").and_then(Value::as_str))
                .collect::<Vec<_>>(),
        )
    });
    let new_station_id = persisted
        .get("station_id")
        .and_then(Value::as_str)
        .expect("enrollment.persisted carries station_id");
    assert_ne!(
        new_station_id,
        existing_station_id.to_string(),
        "new station_id matches existing one; fake perchpub should mint a fresh UUID",
    );

    // Prominent audit: at least one WARN-or-higher event mentions both the
    // old and the new station_id. (Contract test T055 will pin the exact
    // event name / field layout once the producer site is wired up.)
    let prominent = forced_events.iter().any(|ev| {
        let level_loud = matches!(
            ev.get("level").and_then(Value::as_str),
            Some("WARN" | "warn" | "ERROR" | "error"),
        );
        let serialized = ev.to_string();
        level_loud
            && serialized.contains(&existing_station_id.to_string())
            && serialized.contains(new_station_id)
    });
    assert!(
        prominent,
        "no WARN+ event names both old ({existing_station_id}) and new ({new_station_id}) station IDs; events: {:?}",
        forced_events
            .iter()
            .map(|e| (e.get("level").cloned(), e.get("event").cloned()))
            .collect::<Vec<_>>(),
    );

    // identity.json now carries the new station_id (and differs from the
    // pre-populated bytes).
    let identity_after_force = std::fs::read(&identity_path).expect("read identity post-force");
    assert_ne!(
        identity_before, identity_after_force,
        "identity.json was not overwritten by --force",
    );
    let identity_json: Value =
        serde_json::from_slice(&identity_after_force).expect("identity.json valid JSON post-force");
    assert_eq!(
        identity_json.get("station_id").and_then(Value::as_str),
        Some(new_station_id),
        "identity.json's station_id does not match the persisted event",
    );
}
