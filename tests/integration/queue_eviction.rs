//! T041 — queue eviction policy, RED → in-process integration test.
//!
//! Drives [`perchstation_core::queue::policy::PolicyInbox`] directly so
//! we can inspect on-disk state (and the structured `queue.evicted`
//! event) without spawning a binary. Two scenarios:
//!
//! 1. `drop_oldest_undelivered` — fill the queue to `max_clips`, submit
//!    one more clip, assert the oldest `delivered/` Undeliverable entry
//!    was dropped and the new clip landed in `pending/`. Then drain the
//!    Undeliverable entries and assert the next eviction takes from
//!    `pending/` (oldest first).
//! 2. `refuse_new` — same fill, but the next `submit` returns
//!    `InboxError::QueueFull` and no eviction occurs.
//!
//! Spec coverage: US2 acceptance #3.

#[path = "support/mod.rs"]
mod support;

use std::fs;

use chrono::{Duration, TimeZone, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use perchstation_core::config::EvictionPolicy;
use perchstation_core::observability::tracing::events as ev;
use perchstation_core::queue::inbox::{Inbox, StoreInbox};
use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
use perchstation_core::queue::store::{ClipMeta, QueueStore};
use perchstation_core::queue::{ClipQueueEntry, InboxError, Outcome};

use support::logs::CaptureBuffer;

fn install_json_subscriber(buf: &CaptureBuffer) -> tracing::subscriber::DefaultGuard {
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_default(subscriber)
}

fn write_sidecar(path: &std::path::Path, entry: &ClipQueueEntry) {
    fs::write(path, serde_json::to_vec_pretty(entry).unwrap()).expect("write sidecar");
}

fn write_clip(dir: &std::path::Path, clip_id: &str, payload: &[u8]) {
    fs::write(dir.join(format!("{clip_id}.mp4")), payload).expect("write mp4");
}

#[tokio::test(flavor = "current_thread")]
async fn drop_oldest_undelivered_evicts_undeliverable_first_then_pending() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");

    // ---- pre-populate queue to exactly max_clips = 3 ----
    // Two Undeliverable in delivered/ at captured_at t1 < t2.
    // One pending at t3, freshest of the three.
    let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let t2 = t1 + Duration::seconds(60);
    let t3 = t2 + Duration::seconds(60);

    let undeliverable_a = ClipQueueEntry {
        clip_id: "20260101T000000Z-001".into(),
        captured_at: t1,
        enqueued_at: t1,
        byte_size: 100,
        attempts: 3,
        first_attempt_at: Some(t1),
        last_attempt_at: Some(t1),
        last_error: None,
        next_attempt_after: None,
        outcome: Some(Outcome::Undeliverable),
        classify_task_id: None,
        delivered_at: Some(t1),
        last_classify_status: None,
        classify_lost_at: None,
    };
    let undeliverable_b = ClipQueueEntry {
        clip_id: "20260101T000100Z-002".into(),
        captured_at: t2,
        enqueued_at: t2,
        delivered_at: Some(t2),
        ..undeliverable_a.clone()
    };
    let pending_c = ClipQueueEntry {
        clip_id: "20260101T000200Z-003".into(),
        captured_at: t3,
        enqueued_at: t3,
        byte_size: 100,
        attempts: 0,
        first_attempt_at: None,
        last_attempt_at: None,
        last_error: None,
        next_attempt_after: None,
        outcome: None,
        classify_task_id: None,
        delivered_at: None,
        last_classify_status: None,
        classify_lost_at: None,
    };

    write_sidecar(&store.delivered_dir().join("20260101T000000Z-001.json"), &undeliverable_a);
    write_sidecar(&store.delivered_dir().join("20260101T000100Z-002.json"), &undeliverable_b);
    write_clip(&store.pending_dir(), &pending_c.clip_id, &[0u8; 100]);
    write_sidecar(&store.pending_dir().join("20260101T000200Z-003.json"), &pending_c);

    // ---- prepare a fresh clip to submit ----
    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 100]).expect("write incoming clip");

    let policy = QueuePolicy {
        max_clips: 3,
        max_bytes: 10_000,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox = PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy);

    // Capture tracing in a buffer scoped to the submit call.
    let buf = CaptureBuffer::new();
    {
        let _guard = install_json_subscriber(&buf);
        let _entry = inbox
            .submit(&new_clip, ClipMeta { captured_at: Utc::now() })
            .await
            .expect("submit should evict-and-accept");
    }

    // The oldest Undeliverable (undeliverable_a, captured_at t1) should be gone.
    assert!(
        !store.delivered_dir().join("20260101T000000Z-001.json").exists(),
        "oldest Undeliverable should have been evicted",
    );
    assert!(
        store.delivered_dir().join("20260101T000100Z-002.json").exists(),
        "second-oldest Undeliverable should still be present",
    );
    assert!(
        store.pending_dir().join("20260101T000200Z-003.json").exists(),
        "pre-existing pending should still be present",
    );

    let events = buf.events();
    let evicted = events
        .iter()
        .find(|e| e.get("event").and_then(Value::as_str) == Some(ev::QUEUE_EVICTED))
        .unwrap_or_else(|| panic!("expected queue.evicted event; captured: {events:?}"));
    assert_eq!(evicted.get("clip_id").and_then(Value::as_str), Some("20260101T000000Z-001"));
    assert_eq!(evicted.get("policy").and_then(Value::as_str), Some("drop_oldest_undelivered"));
    let reason = evicted.get("reason").and_then(Value::as_str).expect("reason field");
    assert!(
        matches!(reason, "max_clips_exceeded" | "max_bytes_exceeded"),
        "unexpected reason: {reason}",
    );
    assert!(evicted.get("remaining_clips").is_some(), "remaining_clips required");
    assert!(evicted.get("remaining_bytes").is_some(), "remaining_bytes required");
}

#[tokio::test(flavor = "current_thread")]
async fn drop_oldest_undelivered_falls_back_to_pending_when_no_undeliverable_remains() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");

    // Two pending entries; no Undeliverable to fall back on.
    let t1 = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
    let t2 = t1 + Duration::seconds(60);
    let entry_a = ClipQueueEntry::new("20260201T000000Z-001", t1, t1, 100);
    let entry_b = ClipQueueEntry::new("20260201T000100Z-002", t2, t2, 100);
    write_clip(&store.pending_dir(), &entry_a.clip_id, &[0u8; 100]);
    write_sidecar(&store.pending_dir().join("20260201T000000Z-001.json"), &entry_a);
    write_clip(&store.pending_dir(), &entry_b.clip_id, &[0u8; 100]);
    write_sidecar(&store.pending_dir().join("20260201T000100Z-002.json"), &entry_b);

    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 100]).expect("write incoming");

    let policy = QueuePolicy {
        max_clips: 2,
        max_bytes: 10_000,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox = PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy);

    // Install a scoped subscriber even though we don't assert on events.
    // Triggering `tracing::warn!(QUEUE_EVICTED, ...)` from a thread without
    // a default subscriber poisons tracing-core's per-callsite interest
    // cache to `Never` (via the `Rebuilder::JustOne` fast path that consults
    // `dispatcher::get_default` on the *registering* thread). That cache is
    // process-global and would silently break the sibling
    // `drop_oldest_undelivered_evicts_*` test, which shares this binary and
    // expects to capture the same event.
    let buf = CaptureBuffer::new();
    let _guard = install_json_subscriber(&buf);

    inbox
        .submit(&new_clip, ClipMeta { captured_at: Utc::now() })
        .await
        .expect("submit should evict-and-accept");

    // Oldest pending (entry_a) evicted; entry_b remains; new clip joined.
    assert!(
        !store.pending_dir().join("20260201T000000Z-001.json").exists(),
        "oldest pending should have been evicted as fallback",
    );
    assert!(
        store.pending_dir().join("20260201T000100Z-002.json").exists(),
        "younger pending should remain",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn refuse_new_returns_inbox_error_without_eviction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");

    // Fill to max_clips = 2 with Undeliverable entries (which would have
    // been evictable under drop_oldest_undelivered).
    let t1 = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
    let undeliverable_a = ClipQueueEntry {
        clip_id: "20260301T000000Z-001".into(),
        captured_at: t1,
        enqueued_at: t1,
        byte_size: 50,
        attempts: 3,
        first_attempt_at: Some(t1),
        last_attempt_at: Some(t1),
        last_error: None,
        next_attempt_after: None,
        outcome: Some(Outcome::Undeliverable),
        classify_task_id: None,
        delivered_at: Some(t1),
        last_classify_status: None,
        classify_lost_at: None,
    };
    let undeliverable_b = ClipQueueEntry {
        clip_id: "20260301T000100Z-002".into(),
        captured_at: t1 + Duration::seconds(60),
        ..undeliverable_a.clone()
    };
    write_sidecar(&store.delivered_dir().join("20260301T000000Z-001.json"), &undeliverable_a);
    write_sidecar(&store.delivered_dir().join("20260301T000100Z-002.json"), &undeliverable_b);

    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 50]).expect("write incoming");

    let policy =
        QueuePolicy { max_clips: 2, max_bytes: 10_000, eviction: EvictionPolicy::RefuseNew };
    let inbox = PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy);

    let err = inbox
        .submit(&new_clip, ClipMeta { captured_at: Utc::now() })
        .await
        .expect_err("refuse_new must reject submissions when at the bound");

    match err {
        InboxError::QueueFull { current_clips, max_clips, current_bytes, max_bytes } => {
            assert_eq!(current_clips, 2);
            assert_eq!(max_clips, 2);
            assert!(current_bytes > 0);
            assert_eq!(max_bytes, 10_000);
        }
        InboxError::Queue(other) => panic!("expected QueueFull, got store-level error: {other:?}"),
    }

    // No eviction: both pre-existing entries remain.
    assert!(store.delivered_dir().join("20260301T000000Z-001.json").exists());
    assert!(store.delivered_dir().join("20260301T000100Z-002.json").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn under_bounds_path_is_a_no_op_with_no_events() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");
    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 50]).expect("write incoming");

    let policy = QueuePolicy {
        max_clips: 10,
        max_bytes: 10_000,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox = PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy);

    let buf = CaptureBuffer::new();
    {
        let _guard = install_json_subscriber(&buf);
        inbox.submit(&new_clip, ClipMeta { captured_at: Utc::now() }).await.expect("submit ok");
    }
    let evicted: Vec<_> = buf
        .events()
        .into_iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::QUEUE_EVICTED))
        .collect();
    assert!(evicted.is_empty(), "no eviction expected when under bounds: {evicted:?}");

    // The new clip lives in pending/.
    let pending: Vec<_> =
        fs::read_dir(store.pending_dir()).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(
        pending.iter().filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mp4")).count(),
        1,
        "expected exactly one mp4 in pending/, got {pending:?}",
    );
}

// Helper test to keep imports honest: assert that the same JSON wire shape we
// expect downstream is produced when serializing an Undeliverable sidecar.
#[test]
fn undeliverable_sidecar_serialises_as_expected() {
    let entry = ClipQueueEntry {
        clip_id: "x".into(),
        captured_at: Utc::now(),
        enqueued_at: Utc::now(),
        byte_size: 0,
        attempts: 0,
        first_attempt_at: None,
        last_attempt_at: None,
        last_error: None,
        next_attempt_after: None,
        outcome: Some(Outcome::Undeliverable),
        classify_task_id: None,
        delivered_at: None,
        last_classify_status: None,
        classify_lost_at: None,
    };
    let v = serde_json::to_value(&entry).unwrap();
    assert_eq!(v["outcome"], json!("Undeliverable"));
    // unused: keep imports honest
    let _ = Uuid::nil();
}
