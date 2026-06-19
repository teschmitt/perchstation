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
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
            // PS-20: the two pre-existing entries live in `delivered/` with
            // their mp4 unlinked, so they contribute ZERO bytes on disk. The
            // refusal here is driven purely by the clip ceiling.
            assert_eq!(current_bytes, 0);
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

// ---- PS-20: delivered/ sidecars carry no media bytes ----

#[tokio::test(flavor = "current_thread")]
async fn delivered_bytes_not_counted_against_max_bytes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");

    // Three `delivered/` Delivered sidecars each declaring a large `byte_size`
    // — but their mp4 was unlinked on success, so they hold ZERO bytes on
    // disk. If (wrongly) summed they total 30_000, far over `max_bytes` below.
    let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for i in 0..3 {
        let id = format!("20260101T0000{i:02}Z-001");
        let mut e = ClipQueueEntry::new(&id, t, t, 10_000);
        e.outcome = Some(Outcome::Delivered);
        e.delivered_at = Some(t);
        write_sidecar(&store.delivered_dir().join(format!("{id}.json")), &e);
    }

    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 100]).expect("write incoming");

    // Plenty of clip headroom (delivered entries still occupy a slot); the
    // only thing that could refuse the clip is phantom delivered bytes.
    let policy = QueuePolicy {
        max_clips: 100,
        max_bytes: 1_000,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox = PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy);

    let buf = CaptureBuffer::new();
    let entry = {
        let _guard = install_json_subscriber(&buf);
        inbox
            .submit(&new_clip, ClipMeta { captured_at: Utc::now() })
            .await
            .expect("submit must succeed: delivered bytes are phantom (PS-20)")
    };

    let evicted: Vec<_> = buf
        .events()
        .into_iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::QUEUE_EVICTED))
        .collect();
    assert!(
        evicted.is_empty(),
        "no eviction expected when only phantom bytes 'breach': {evicted:?}"
    );

    assert!(
        store.pending_dir().join(format!("{}.mp4", entry.clip_id)).is_file(),
        "the fresh clip should have been accepted into pending/",
    );
}

// ---- PS-05: census must agree with the evictable set ----

#[tokio::test(flavor = "current_thread")]
async fn delivered_backlog_does_not_destroy_fresh_pending_then_queuefull() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");

    // Un-evictable floor: three `delivered/` Delivered sidecars (mp4 unlinked).
    // This alone meets `max_clips = 3`, so no amount of eviction can free a
    // slot for a new clip.
    let t = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap();
    for i in 0..3 {
        let id = format!("20260401T0000{i:02}Z-001");
        let mut e = ClipQueueEntry::new(&id, t, t, 100);
        e.outcome = Some(Outcome::Delivered);
        e.delivered_at = Some(t);
        write_sidecar(&store.delivered_dir().join(format!("{id}.json")), &e);
    }

    // One pre-existing pending clip — the fresh data we must NOT destroy.
    let pending_id = "20260401T010000Z-001";
    write_clip(&store.pending_dir(), pending_id, &[0u8; 100]);
    let pending = ClipQueueEntry::new(pending_id, t, t, 100);
    write_sidecar(&store.pending_dir().join(format!("{pending_id}.json")), &pending);

    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 100]).expect("write incoming");

    let policy = QueuePolicy {
        max_clips: 3,
        max_bytes: 10_000_000,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox = PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy);

    let buf = CaptureBuffer::new();
    let result = {
        let _guard = install_json_subscriber(&buf);
        inbox.submit(&new_clip, ClipMeta { captured_at: Utc::now() }).await
    };

    // The un-evictable floor already fills the ceiling, so the submission must
    // be refused WITHOUT first deleting the fresh pending clip.
    match result {
        Err(InboxError::QueueFull { .. }) => {}
        Ok(entry) => panic!("expected QueueFull, but the clip was accepted as {}", entry.clip_id),
        Err(InboxError::Queue(other)) => panic!("expected QueueFull, got store error: {other:?}"),
    }
    assert!(
        store.pending_dir().join(format!("{pending_id}.json")).exists(),
        "the fresh pending clip must NOT be evicted when eviction cannot free a slot",
    );
    assert!(
        store.pending_dir().join(format!("{pending_id}.mp4")).exists(),
        "the fresh pending clip's media must survive",
    );
    let evicted: Vec<_> = buf
        .events()
        .into_iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::QUEUE_EVICTED))
        .collect();
    assert!(evicted.is_empty(), "nothing should have been evicted: {evicted:?}");
}

// ---- PS-21: eviction reason reflects byte pressure when both ceilings breach ----

#[tokio::test(flavor = "current_thread")]
async fn eviction_reason_reflects_byte_pressure_when_both_breached() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");

    // Three pending clips, 100 bytes each.
    let base = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
    for i in 0..3 {
        let id = format!("20260501T0000{i:02}Z-001");
        write_clip(&store.pending_dir(), &id, &[0u8; 100]);
        let e = ClipQueueEntry::new(&id, base + Duration::seconds(i64::from(i)), base, 100);
        write_sidecar(&store.pending_dir().join(format!("{id}.json")), &e);
    }

    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 100]).expect("write incoming");

    // The incoming clip breaches BOTH ceilings at once: 4 clips > 3, and
    // 400 bytes > 250.
    let policy = QueuePolicy {
        max_clips: 3,
        max_bytes: 250,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox = PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy);

    let buf = CaptureBuffer::new();
    {
        let _guard = install_json_subscriber(&buf);
        inbox.submit(&new_clip, ClipMeta { captured_at: Utc::now() }).await.expect("submit");
    }

    let events = buf.events();
    let reasons: Vec<&str> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::QUEUE_EVICTED))
        .map(|e| e.get("reason").and_then(Value::as_str).expect("reason field"))
        .collect();
    assert!(!reasons.is_empty(), "expected a queue.evicted event; captured: {events:?}");

    // The old code mislabelled the first eviction as `max_clips_exceeded`; the
    // fix attributes it to both ceilings since byte pressure is also breached.
    assert_eq!(
        reasons[0], "both_exceeded",
        "the first eviction must report both ceilings when both are breached",
    );
    // In this scenario the clip ceiling is only ever breached together with the
    // byte ceiling, so no eviction may be attributed to clip count alone — that
    // would be the very mislabel PS-21 fixes.
    assert!(
        !reasons.contains(&"max_clips_exceeded"),
        "byte pressure was hidden behind clip count: {reasons:?}",
    );
}

// ---- PS-27 corollary: a clip removed from disk is never dropped from telemetry ----

#[tokio::test(flavor = "current_thread")]
async fn eviction_events_emitted_for_clips_removed_before_a_later_eviction_fails() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");

    let base = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();

    // Oldest pending clip — evicts cleanly first.
    let first = "20260701T000000Z-001";
    write_clip(&store.pending_dir(), first, &[0u8; 100]);
    write_sidecar(
        &store.pending_dir().join(format!("{first}.json")),
        &ClipQueueEntry::new(first, base, base, 100),
    );

    // Next pending clip — a VALID sidecar (so the census enrols it as a
    // candidate), but its `.mp4` is a directory, so `fs::remove_file` fails with
    // a non-NotFound I/O error partway through the eviction sweep.
    let second = "20260701T000100Z-001";
    fs::create_dir(store.pending_dir().join(format!("{second}.mp4"))).expect("dir mp4");
    write_sidecar(
        &store.pending_dir().join(format!("{second}.json")),
        &ClipQueueEntry::new(second, base + Duration::seconds(60), base, 100),
    );

    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 100]).expect("write incoming");

    let policy = QueuePolicy {
        max_clips: 1,
        max_bytes: 10_000_000,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox = PolicyInbox::new(StoreInbox::new(store.clone()), store.clone(), policy);

    let buf = CaptureBuffer::new();
    let result = {
        let _guard = install_json_subscriber(&buf);
        inbox.submit(&new_clip, ClipMeta { captured_at: Utc::now() }).await
    };

    // The submit fails (the second eviction hit an I/O error) ...
    assert!(result.is_err(), "submit should surface the eviction I/O failure");
    // ... but the clip that WAS removed before the failure must still produce a
    // queue.evicted event: a deleted clip is never silently dropped from the
    // telemetry.
    assert!(
        !store.pending_dir().join(format!("{first}.json")).exists(),
        "the first clip was evicted from disk",
    );
    let evicted: Vec<_> = buf
        .events()
        .into_iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some(ev::QUEUE_EVICTED))
        .collect();
    assert_eq!(evicted.len(), 1, "exactly one event for the clip actually removed: {evicted:?}");
    assert_eq!(evicted[0].get("clip_id").and_then(Value::as_str), Some(first));
}

// ---- PS-27: the policy preflight must run off the async reactor ----

/// An [`Inbox`] whose `submit` is fully synchronous (it never reaches an
/// `.await` that yields), so the ONLY async work inside `PolicyInbox::submit`
/// is the policy preflight. That lets this test observe whether the preflight
/// blocks the single reactor thread.
struct SyncInbox {
    store: QueueStore,
}

#[async_trait::async_trait]
impl Inbox for SyncInbox {
    async fn submit(&self, clip_path: &Path, meta: ClipMeta) -> Result<ClipQueueEntry, InboxError> {
        Ok(self.store.enqueue(clip_path, meta)?)
    }
}

// `current_thread` is essential here: it runs the test body AND the spawned
// counter task on a single executor thread, so a synchronous (inline) preflight
// genuinely starves the counter. A multi-thread runtime would drive `block_on`
// on a separate thread from the spawned task and mask the blocking.
#[tokio::test(flavor = "current_thread")]
async fn submit_preflight_runs_off_the_reactor() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = QueueStore::open(dir.path()).expect("open store");

    // A pre-populated queue so the preflight does real scanning work. These are
    // un-evictable `delivered/` Delivered entries, so no eviction fires — the
    // test isolates the directory scan, not the eviction loop.
    let t = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    for i in 0..64 {
        let id = format!("20260601T0000{i:02}Z-001");
        let mut e = ClipQueueEntry::new(&id, t, t, 10);
        e.outcome = Some(Outcome::Delivered);
        e.delivered_at = Some(t);
        write_sidecar(&store.delivered_dir().join(format!("{id}.json")), &e);
    }

    let new_clip = dir.path().join("incoming.mp4");
    fs::write(&new_clip, vec![0u8; 10]).expect("write incoming");

    let policy = QueuePolicy {
        max_clips: 100_000,
        max_bytes: 1_000_000,
        eviction: EvictionPolicy::DropOldestUndelivered,
    };
    let inbox = PolicyInbox::new(SyncInbox { store: store.clone() }, store.clone(), policy);

    // A lightweight task that advances a counter, yielding between ticks. On a
    // single reactor thread it can only advance while the reactor is free.
    let counter = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let counter_task = {
        let counter = counter.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        })
    };

    // Let the counter task take a few turns and re-park.
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    let before = counter.load(Ordering::Relaxed);

    inbox.submit(&new_clip, ClipMeta { captured_at: Utc::now() }).await.expect("submit ok");

    let after = counter.load(Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);
    counter_task.await.expect("counter task join");

    // If the preflight ran inline on the reactor, `submit` would complete in a
    // single non-yielding poll and the counter would be frozen (after == before).
    assert!(
        after > before,
        "the policy preflight blocked the reactor: a concurrent task made no \
         progress during submit (before={before}, after={after})",
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
