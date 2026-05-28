//! T040 — outage recovery, end-to-end.
//!
//! Two scenarios cover `spec.md` US2 acceptance #1 and #2:
//!
//! 1. **Backoff schedule** — one clip pending, `FakePerchpub` returns 503
//!    twice then 200. Verify exactly two `delivery.upload_transient`
//!    events fire with `next_attempt_after` values inside the documented
//!    ±20 % jitter envelope (initial 1 s, multiplier 2.0; so attempts
//!    1 → 2 fall in `[0.8, 1.2]` and `[1.6, 2.4]` seconds).
//! 2. **Oldest-first replay** — two clips pending in capture order, one
//!    503 burst, then perchpub recovers; verify both clips ultimately
//!    land in `delivered/` and the older clip is uploaded first.
//!
//! Configuration uses `initial_delay_secs = 1` / `max_attempt_delay_secs
//! = 5` to keep wall-clock under ~10 s per scenario.

#[path = "support/mod.rs"]
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_station_keypair, sample_mp4_bytes, write_test_credentials};
use support::harness::perchstation_bin_path;
use support::logs::{event_codes, find_events, parse_json_events};

fn write_config(data_dir: &std::path::Path, perchpub_url: &str) -> std::path::PathBuf {
    let path = data_dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "perchpub_url = \"{}\"\n\
             data_dir = \"{}\"\n\
             \n\
             [retry]\n\
             initial_delay_secs           = 1\n\
             max_attempt_delay_secs       = 5\n\
             per_clip_max_attempts        = 6\n\
             per_clip_max_wallclock_hours = 1\n",
            perchpub_url,
            data_dir.display(),
        ),
    )
    .expect("write config.toml");
    path
}

fn write_pending_clip(pending: &std::path::Path, clip_id: &str, captured_iso: &str, bytes: &[u8]) {
    std::fs::write(pending.join(format!("{clip_id}.mp4")), bytes).expect("write mp4");
    let sidecar = json!({
        "clip_id": clip_id,
        "captured_at": captured_iso,
        "enqueued_at": captured_iso,
        "byte_size": bytes.len() as u64,
        "attempts": 0u32,
    });
    std::fs::write(
        pending.join(format!("{clip_id}.json")),
        serde_json::to_vec_pretty(&sidecar).unwrap(),
    )
    .expect("write sidecar");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // linear setup → assert sequence; splitting helps neither
async fn outage_recovery_backoff_schedule_matches_research_r7() {
    let pub_ = FakePerchpub::start().await;
    pub_.fail_uploads_transient_503(2); // two transients before recovery

    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config(data_dir.path(), pub_.url());

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
    std::fs::create_dir_all(&pending).expect("mkdir queue/pending");
    let mp4 = sample_mp4_bytes();
    let clip_id = "20260527T120000Z-001";
    write_pending_clip(&pending, clip_id, "2026-05-27T12:00:00Z", &mp4);

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
        "expected clip delivered after the burst\n  events: {codes:?}\n  stderr: {stderr_text}",
    );

    let transients = find_events(&events, "delivery.upload_transient");
    assert_eq!(
        transients.len(),
        2,
        "expected exactly two transient retries; got {transients:?}\n  events: {codes:?}",
    );

    let mut delays_secs = Vec::with_capacity(2);
    let mut prev_next: Option<DateTime<Utc>> = None;
    for (idx, ev) in transients.iter().enumerate() {
        let timestamp =
            ev.get("timestamp").and_then(Value::as_str).expect("`timestamp` on every event");
        let emitted: DateTime<Utc> = timestamp.parse().expect("RFC 3339 timestamp");
        let next_after = ev
            .get("next_attempt_after")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("missing next_attempt_after on transient #{idx}: {ev}"));
        let next: DateTime<Utc> = next_after.parse().expect("RFC 3339 next_attempt_after");

        #[allow(clippy::cast_precision_loss)] // ms always < 2^53 in this test
        let delay = (next - emitted).num_milliseconds() as f64 / 1000.0;
        delays_secs.push(delay);

        if let Some(prev) = prev_next {
            assert!(
                next > prev,
                "transient #{idx} schedules earlier than the previous: {next} <= {prev}",
            );
        }
        prev_next = Some(next);

        assert_eq!(
            ev.get("status").and_then(Value::as_u64),
            Some(503),
            "transient #{idx} status != 503: {ev}",
        );
        assert!(ev.get("attempt").and_then(Value::as_u64).is_some());
        assert_eq!(ev.get("kind").and_then(Value::as_str), Some("http_status"));
    }
    assert!(
        (0.8..=1.2).contains(&delays_secs[0]),
        "attempt 1 backoff outside ±20 % of 1 s: {} s",
        delays_secs[0],
    );
    assert!(
        (1.6..=2.4).contains(&delays_secs[1]),
        "attempt 2 backoff outside ±20 % of 2 s: {} s",
        delays_secs[1],
    );

    let entry: Value =
        serde_json::from_slice(&std::fs::read(&delivered_sidecar).expect("read delivered sidecar"))
            .expect("parse delivered sidecar");
    assert_eq!(entry.get("outcome").and_then(Value::as_str), Some("Delivered"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outage_recovery_uploads_oldest_first_after_brief_outage() {
    let pub_ = FakePerchpub::start().await;
    pub_.fail_uploads_transient_503(2); // one 503 per clip before recovery

    let data_dir = tempfile::tempdir().expect("temp dir");
    let config_path = write_config(data_dir.path(), pub_.url());

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
    std::fs::create_dir_all(&pending).expect("mkdir queue/pending");
    let mp4 = sample_mp4_bytes();

    let older = "20260527T120000Z-001";
    let newer = "20260527T120005Z-002";
    write_pending_clip(&pending, older, "2026-05-27T12:00:00Z", &mp4);
    write_pending_clip(&pending, newer, "2026-05-27T12:00:05Z", &mp4);

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

    let delivered_dir = data_dir.path().join("queue/delivered");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut both_ok = false;
    while Instant::now() < deadline {
        let older_ok = delivered_dir.join(format!("{older}.json")).exists();
        let newer_ok = delivered_dir.join(format!("{newer}.json")).exists();
        if older_ok && newer_ok {
            both_ok = true;
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
        both_ok,
        "expected both clips delivered after the burst\n  events: {codes:?}\n  stderr: {stderr_text}",
    );

    // `delivery.upload_succeeded` for the older clip MUST happen before
    // the same event for the newer clip — that's FR-006 oldest-first.
    let success_order: Vec<&str> = events
        .iter()
        .filter(|ev| ev.get("event").and_then(Value::as_str) == Some("delivery.upload_succeeded"))
        .filter_map(|ev| ev.get("clip_id").and_then(Value::as_str))
        .collect();
    assert!(
        success_order.windows(2).all(|w| w[0] != w[1]),
        "unexpected duplicate success events: {success_order:?}",
    );
    assert!(
        success_order.iter().position(|&id| id == older)
            < success_order.iter().position(|&id| id == newer),
        "older clip must be delivered first; success order was {success_order:?}",
    );

    // No clip lost.
    let pending_remaining: Vec<_> = std::fs::read_dir(&pending)
        .unwrap()
        .filter_map(|e| {
            let path = e.ok()?.path();
            (path.extension().and_then(|e| e.to_str()) == Some("mp4")).then_some(path)
        })
        .collect();
    assert!(
        pending_remaining.is_empty(),
        "pending/ should be empty after recovery; got {pending_remaining:?}",
    );
}
