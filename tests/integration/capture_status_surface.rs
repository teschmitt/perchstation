//! T034 — capture-side `perchstation status` surface (RED).
//!
//! Primes the in-process [`CaptureState`] to each of six documented
//! scenarios and asserts both text and JSON rendering of the status
//! snapshot. Plus one subprocess test that confirms the standalone
//! `perchstation status` binary (invoked outside of `serve`) reports
//! `sensor_liveness = NeverObserved` and emits a `Capture:` block in
//! the text rendering, per `contracts/cli.md` §`status`.
//!
//! Scenarios:
//! 1. Standalone (no [`CaptureState`] handed in) → `sensor_liveness =
//!    "never_observed"`, every other capture field null.
//! 2. Capture task started, no triggers yet → `sensor_liveness =
//!    "healthy"`, no recording / failure fields.
//! 3. Recently recorded → `last_recording_at` / `last_clip_id`
//!    populated, `last_failure = null`.
//! 4. Recent failure with kind + message → `last_failure` populated.
//! 5. Sensor degraded — `stuck_asserted` with `sensor_degraded_since`
//!    set.
//! 6. Sensor degraded — `unavailable` with `sensor_degraded_since` set.
//!
//! Spec mapping: US3 #1, #2 / SC-007 / FR-015 / `contracts/cli.md`
//! §`perchstation status`.

#![allow(clippy::items_after_statements)]

#[path = "support/mod.rs"]
mod support;

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use perchstation_core::capture::CaptureState;
use perchstation_core::observability::status::{self, CaptureLivenessSnapshot};

use support::harness::{perchstation_bin, write_config_toml};

fn instant(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// "Now" anchor used by every snapshot in this file. Stable so the JSON
/// shape is reproducible across runs.
fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 27, 12, 0, 0).single().unwrap()
}

fn empty_data_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

fn snapshot_json(data_dir: &Path, capture: Option<&CaptureState>) -> Value {
    let snap = status::snapshot(data_dir, fixed_now(), capture).expect("snapshot");
    serde_json::to_value(&snap).expect("serialise snapshot")
}

fn snapshot_text(data_dir: &Path, capture: Option<&CaptureState>) -> String {
    let snap = status::snapshot(data_dir, fixed_now(), capture).expect("snapshot");
    snap.render_text()
}

#[test]
fn scenario_1_standalone_status_reports_never_observed_and_no_capture_data() {
    let dir = empty_data_dir();

    let json = snapshot_json(dir.path(), None);
    let capture = &json["capture"];
    assert!(capture.is_object(), "snapshot must include `capture` object; got {json}");
    assert!(capture["last_recording_at"].is_null(), "last_recording_at must be null in {json}");
    assert!(capture["last_clip_id"].is_null(), "last_clip_id must be null in {json}");
    assert!(capture["last_failure"].is_null(), "last_failure must be null in {json}");
    assert_eq!(
        capture["sensor_liveness"].as_str(),
        Some("never_observed"),
        "standalone sensor_liveness must be `never_observed`; got {capture}",
    );
    assert!(
        capture["sensor_degraded_since"].is_null(),
        "sensor_degraded_since must be null for never_observed; got {capture}",
    );

    let text = snapshot_text(dir.path(), None);
    assert!(text.contains("Capture:"), "text missing Capture heading:\n{text}");
    assert!(
        text.contains("Last recording:  (none)"),
        "standalone text must mark Last recording as (none):\n{text}",
    );
    assert!(
        text.contains("Last failure:    (none)"),
        "standalone text must mark Last failure as (none):\n{text}",
    );
    assert!(
        text.contains("Sensor:          (never observed)"),
        "standalone text must mark Sensor as (never observed):\n{text}",
    );
}

#[test]
fn scenario_2_healthy_with_no_records_renders_healthy_and_no_capture_data() {
    let dir = empty_data_dir();
    let state = CaptureState::new();
    state.set_liveness(CaptureLivenessSnapshot::Healthy, None);

    let json = snapshot_json(dir.path(), Some(&state));
    assert_eq!(json["capture"]["sensor_liveness"].as_str(), Some("healthy"));
    assert!(json["capture"]["sensor_degraded_since"].is_null());
    assert!(json["capture"]["last_recording_at"].is_null());
    assert!(json["capture"]["last_clip_id"].is_null());
    assert!(json["capture"]["last_failure"].is_null());

    let text = snapshot_text(dir.path(), Some(&state));
    assert!(text.contains("Capture:"), "text missing Capture heading:\n{text}");
    assert!(
        text.contains("Sensor:          healthy"),
        "healthy sensor must render as `healthy`:\n{text}",
    );
    assert!(text.contains("Last recording:  (none)"), "healthy/no-record text:\n{text}");
    assert!(text.contains("Last failure:    (none)"), "healthy/no-failure text:\n{text}");
}

#[test]
fn scenario_3_recent_recording_populates_last_recording_fields() {
    let dir = empty_data_dir();
    let state = CaptureState::new();
    state.set_liveness(CaptureLivenessSnapshot::Healthy, None);
    let recorded_at = instant("2026-05-27T06:31:00Z");
    state.record_success("20260527T063100Z-001".to_string(), recorded_at);

    let json = snapshot_json(dir.path(), Some(&state));
    assert_eq!(
        json["capture"]["last_recording_at"].as_str(),
        Some("2026-05-27T06:31:00Z"),
        "last_recording_at must serialise as RFC 3339 UTC with `Z` suffix; got {}",
        json["capture"],
    );
    assert_eq!(json["capture"]["last_clip_id"].as_str(), Some("20260527T063100Z-001"));
    assert!(json["capture"]["last_failure"].is_null(), "no failure after a fresh success");
    assert_eq!(json["capture"]["sensor_liveness"].as_str(), Some("healthy"));

    let text = snapshot_text(dir.path(), Some(&state));
    assert!(
        text.contains("Last recording:  2026-05-27 06:31:00 UTC  20260527T063100Z-001"),
        "recently-recorded text mismatch:\n{text}",
    );
    assert!(text.contains("Last failure:    (none)"), "no-failure text:\n{text}");
}

#[test]
fn scenario_4_recent_failure_populates_kind_and_message() {
    let dir = empty_data_dir();
    let state = CaptureState::new();
    state.set_liveness(CaptureLivenessSnapshot::Healthy, None);
    let failed_at = instant("2026-05-27T06:30:12Z");
    state.record_failure(failed_at, "recording_failed", "io error reading from camera".into());

    let json = snapshot_json(dir.path(), Some(&state));
    let failure = &json["capture"]["last_failure"];
    assert!(failure.is_object(), "last_failure must be an object; got {}", json["capture"]);
    assert_eq!(failure["at"].as_str(), Some("2026-05-27T06:30:12Z"));
    assert_eq!(failure["kind"].as_str(), Some("recording_failed"));
    assert_eq!(failure["message"].as_str(), Some("io error reading from camera"));

    let text = snapshot_text(dir.path(), Some(&state));
    assert!(
        text.contains(
            "Last failure:    2026-05-27 06:30:12 UTC  recording_failed: io error reading from camera",
        ),
        "recent-failure text mismatch:\n{text}",
    );
}

#[test]
fn scenario_5_stuck_asserted_renders_degraded_state_with_since() {
    let dir = empty_data_dir();
    let state = CaptureState::new();
    let since = instant("2026-05-27T06:25:00Z");
    state.set_liveness(CaptureLivenessSnapshot::StuckAsserted, Some(since));

    let json = snapshot_json(dir.path(), Some(&state));
    assert_eq!(json["capture"]["sensor_liveness"].as_str(), Some("stuck_asserted"));
    assert_eq!(
        json["capture"]["sensor_degraded_since"].as_str(),
        Some("2026-05-27T06:25:00Z"),
        "sensor_degraded_since must be populated when stuck_asserted; got {}",
        json["capture"],
    );

    let text = snapshot_text(dir.path(), Some(&state));
    assert!(
        text.contains("Sensor:          stuck_asserted (since 2026-05-27 06:25:00 UTC)"),
        "stuck_asserted text mismatch:\n{text}",
    );
}

#[test]
fn scenario_6_unavailable_renders_degraded_state_with_since() {
    let dir = empty_data_dir();
    let state = CaptureState::new();
    let since = instant("2026-05-27T06:25:00Z");
    state.set_liveness(CaptureLivenessSnapshot::Unavailable, Some(since));

    let json = snapshot_json(dir.path(), Some(&state));
    assert_eq!(json["capture"]["sensor_liveness"].as_str(), Some("unavailable"));
    assert_eq!(
        json["capture"]["sensor_degraded_since"].as_str(),
        Some("2026-05-27T06:25:00Z"),
        "sensor_degraded_since must be populated when unavailable; got {}",
        json["capture"],
    );

    let text = snapshot_text(dir.path(), Some(&state));
    assert!(
        text.contains("Sensor:          unavailable (since 2026-05-27 06:25:00 UTC)"),
        "unavailable text mismatch:\n{text}",
    );
}

#[test]
fn standalone_binary_status_emits_capture_block_and_never_observed() {
    let dir = empty_data_dir();
    let config_path = write_config_toml(dir.path(), "https://example.invalid");

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
        "status --json must exit 0 even with no enrollment;\n  status: {:?}\n  stderr: {}\n  stdout: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "status --json must emit a JSON object: {err}\n  stdout: {}",
            String::from_utf8_lossy(&output.stdout),
        )
    });
    assert!(json["capture"].is_object(), "binary status JSON missing capture field; got {json}");
    assert_eq!(
        json["capture"]["sensor_liveness"].as_str(),
        Some("never_observed"),
        "binary status (outside serve) must report never_observed; got {}",
        json["capture"],
    );

    let text_output = perchstation_bin()
        .args(["--config", &config_path.display().to_string(), "--log-format", "json", "status"])
        .output()
        .expect("spawn perchstation status");
    assert!(text_output.status.success());
    let text = String::from_utf8(text_output.stdout).expect("status stdout is UTF-8");
    assert!(text.contains("Capture:"), "binary text status missing Capture heading:\n{text}");
    assert!(
        text.contains("Sensor:          (never observed)"),
        "binary text status (outside serve) must mark Sensor as (never observed):\n{text}",
    );
}
