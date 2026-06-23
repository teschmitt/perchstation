//! Re-enroll keypair semantics (device-cert contract §2/§8).
//!
//! Two passes against the same pre-populated `credentials/` dir:
//!
//! 1. `perchstation enroll` (no `--force`) must **reuse** the persisted
//!    keypair: it succeeds (exit 0), the on-disk `station.key` keeps the
//!    **same SPKI** (so perchpub sees the same station), the certificate is
//!    refreshed, and no scary overwrite/refusal event fires — this is the §8
//!    manual-renewal path.
//! 2. `perchstation enroll --force` must enroll as a **new** station: it
//!    succeeds, mints a **fresh** keypair (a *different* SPKI), and emits at
//!    least one WARN-or-higher audit event naming both the old and the new
//!    `station_id` (`enrollment.overwritten`, per cli.md §enroll).
//!
//! Supersedes the original "refuse to clobber" behavior: FR-003's protected
//! asset is now the keypair *identity* (preserved on a plain re-enroll), not
//! the whole credentials directory.

#[path = "support/mod.rs"]
mod support;

use rcgen::KeyPair;
use serde_json::Value;
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_qr_png, build_station_keypair, write_test_credentials};
use support::harness::{perchstation_bin, write_config_toml};
use support::logs::{find_event, parse_json_events};

/// SPKI (`SubjectPublicKeyInfo`, DER) of the persisted `station.key` — the
/// value perchpub pins. Equal SPKI ⇒ same station identity.
fn station_key_spki(data_dir: &std::path::Path) -> Vec<u8> {
    let pem = std::fs::read_to_string(data_dir.join("credentials/station.key"))
        .expect("read station.key");
    KeyPair::from_pem(&pem).expect("parse station.key").public_key_der()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // linear two-pass setup; splitting hurts readability
async fn reenroll_reuses_key_then_force_mints_new() {
    let pub_ = FakePerchpub::start().await;
    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config_toml(data_dir.path(), pub_.url());

    // Pre-populate credentials with a known station_id + keypair so we can
    // assert "same SPKI" (reuse) and "different SPKI" (force) afterwards.
    let existing_station_id = Uuid::new_v4();
    let existing_key = build_station_keypair();
    let existing_spki = existing_key.public_key_der();
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

    // A fresh QR that drives the enrollment exchange.
    let qr_path = data_dir.path().join("enroll.png");
    let session_id = Uuid::new_v4();
    let auth_token = "test-auth-token-reenroll";
    std::fs::write(&qr_path, build_qr_png(session_id, auth_token, pub_.ca_pem()))
        .expect("write QR png");

    // ===== Pass 1: enroll WITHOUT --force; must reuse the key =====
    let reuse = perchstation_bin()
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

    let reuse_events = parse_json_events(&reuse.stderr);
    assert!(
        reuse.status.success(),
        "expected exit 0 on a same-station re-enroll; got {:?}\nstderr: {}",
        reuse.status,
        String::from_utf8_lossy(&reuse.stderr),
    );

    // The keypair — and therefore the station identity — is preserved.
    assert_eq!(
        station_key_spki(data_dir.path()),
        existing_spki,
        "a non-force re-enroll must REUSE the persisted keypair (same SPKI)",
    );

    // It persisted, and did NOT fire the refusal or the loud overwrite audit.
    assert!(
        find_event(&reuse_events, "enrollment.persisted").is_some(),
        "missing enrollment.persisted after a reusing re-enroll; saw {:?}",
        reuse_events
            .iter()
            .filter_map(|e| e.get("event").and_then(Value::as_str))
            .collect::<Vec<_>>(),
    );
    assert!(
        find_event(&reuse_events, "enrollment.refused_overwrite").is_none(),
        "a reusing re-enroll must not refuse",
    );
    assert!(
        find_event(&reuse_events, "enrollment.overwritten").is_none(),
        "reusing the same keypair must not emit the new-identity audit",
    );

    // The station_id perchpub assigned on this confirm (now in identity.json).
    let id_after_reuse = std::fs::read(data_dir.path().join("credentials/identity.json"))
        .expect("read identity after reuse");
    let id_after_reuse: Value = serde_json::from_slice(&id_after_reuse).expect("identity json");
    let station_after_reuse =
        id_after_reuse.get("station_id").and_then(Value::as_str).expect("station_id").to_owned();

    // ===== Pass 2: enroll --force; must mint a NEW keypair =====
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

    // --force mints a brand-new keypair: a different SPKI = a new station.
    assert_ne!(
        station_key_spki(data_dir.path()),
        existing_spki,
        "--force must mint a FRESH keypair (different SPKI)",
    );

    let persisted = find_event(&forced_events, "enrollment.persisted").unwrap_or_else(|| {
        panic!(
            "missing enrollment.persisted after --force; saw {:?}",
            forced_events
                .iter()
                .filter_map(|e| e.get("event").and_then(Value::as_str))
                .collect::<Vec<_>>(),
        )
    });
    let new_station_id =
        persisted.get("station_id").and_then(Value::as_str).expect("persisted station_id");

    // Prominent audit: at least one WARN-or-higher event names both the old
    // (post-reuse) and the new `station_id` (the `enrollment.overwritten` audit).
    let prominent = forced_events.iter().any(|ev| {
        let loud = matches!(
            ev.get("level").and_then(Value::as_str),
            Some("WARN" | "warn" | "ERROR" | "error"),
        );
        let serialized = ev.to_string();
        loud && serialized.contains(&station_after_reuse) && serialized.contains(new_station_id)
    });
    assert!(
        prominent,
        "no WARN+ event names both old ({station_after_reuse}) and new ({new_station_id}) station IDs; events: {:?}",
        forced_events
            .iter()
            .map(|e| (e.get("level").cloned(), e.get("event").cloned()))
            .collect::<Vec<_>>(),
    );
}
