//! Queue-bound enforcement and eviction (T047).
//!
//! Wraps any [`Inbox`] implementation with policy logic: count the queue,
//! compare to the configured ceilings, evict or refuse per the
//! [`EvictionPolicy`], then delegate to the inner inbox.
//!
//! Eviction strategy for `drop_oldest_undelivered`:
//!
//! 1. Remove `delivered/` entries whose `outcome == Undeliverable`,
//!    oldest by `captured_at`, until under bounds.
//! 2. If still over bounds, remove `pending/` entries oldest by
//!    `captured_at` (FR-006 ordering is preserved by the basic-ISO
//!    clip-id prefix; sorting filenames is equivalent to sorting by
//!    `captured_at`).
//! 3. `inflight/` is never touched — the runner moves entries out of
//!    `inflight/` quickly enough that a momentary breach is preferable
//!    to disturbing an in-flight upload.

use std::fs;
use std::path::Path;

use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;

use crate::config::{EvictionPolicy, QueueConfig};
use crate::observability::tracing as obs_tracing;
use crate::perchpub::types::ClassifyTaskStatus;

use super::inbox::Inbox;
use super::store::{ClipMeta, QueueStore};
use super::{ClipQueueEntry, InboxError, Outcome, QueueError};

/// Bounds + eviction strategy for a configured queue.
#[derive(Debug, Clone, Copy)]
pub struct QueuePolicy {
    pub max_clips: u32,
    pub max_bytes: u64,
    pub eviction: EvictionPolicy,
}

impl From<&QueueConfig> for QueuePolicy {
    fn from(cfg: &QueueConfig) -> Self {
        Self { max_clips: cfg.max_clips, max_bytes: cfg.max_bytes, eviction: cfg.eviction }
    }
}

/// Why an eviction was issued. Stamped on the `queue.evicted` event so
/// downstream tooling can attribute pressure to clip count vs byte total.
#[derive(Debug, Clone, Copy)]
pub enum EvictionReason {
    MaxClipsExceeded,
    MaxBytesExceeded,
}

impl EvictionReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxClipsExceeded => "max_clips_exceeded",
            Self::MaxBytesExceeded => "max_bytes_exceeded",
        }
    }
}

/// Convert an [`EvictionPolicy`] enum variant to the wire string used in
/// `queue.evicted` events.
#[must_use]
pub const fn policy_as_str(policy: EvictionPolicy) -> &'static str {
    match policy {
        EvictionPolicy::DropOldestUndelivered => "drop_oldest_undelivered",
        EvictionPolicy::RefuseNew => "refuse_new",
    }
}

/// [`Inbox`] wrapper that enforces queue bounds before delegating to the
/// inner inbox. Owns a clone of the [`QueueStore`] so it can introspect
/// and prune entries directly without going back through the trait.
pub struct PolicyInbox<I: Inbox> {
    inner: I,
    store: QueueStore,
    policy: QueuePolicy,
}

impl<I: Inbox> PolicyInbox<I> {
    #[must_use]
    pub fn new(inner: I, store: QueueStore, policy: QueuePolicy) -> Self {
        Self { inner, store, policy }
    }
}

#[async_trait]
impl<I: Inbox> Inbox for PolicyInbox<I> {
    async fn submit(&self, clip_path: &Path, meta: ClipMeta) -> Result<ClipQueueEntry, InboxError> {
        let incoming_bytes = fs::metadata(clip_path).map_or(0, |m| m.len());
        // Policy preflight runs inline (not on `spawn_blocking`) so the
        // `queue.evicted` events fire in the caller's tracing scope —
        // useful for tests that install a scoped subscriber, and harmless
        // in production (sidecar reads + a few unlink calls are fast even
        // at the configured 500-entry / 2 GiB ceiling).
        apply_policy(&self.store, self.policy, incoming_bytes)?;
        self.inner.submit(clip_path, meta).await
    }
}

/// Walk every sidecar in `pending/`, `inflight/`, `delivered/` and sum
/// `byte_size`. Returns `(clip_count, byte_total)`.
pub fn count_queue(store: &QueueStore) -> Result<(u32, u64), QueueError> {
    let mut clips = 0_u32;
    let mut bytes = 0_u64;
    for dir in [store.pending_dir(), store.inflight_dir(), store.delivered_dir()] {
        for entry in
            fs::read_dir(&dir).map_err(|source| QueueError::Io { path: dir.clone(), source })?
        {
            let entry = entry.map_err(|source| QueueError::Io { path: dir.clone(), source })?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let bytes_read =
                fs::read(&path).map_err(|source| QueueError::Io { path: path.clone(), source })?;
            let sidecar: ClipQueueEntry = serde_json::from_slice(&bytes_read)
                .map_err(|source| QueueError::Deserialise { path: path.clone(), source })?;
            clips = clips.saturating_add(1);
            bytes = bytes.saturating_add(sidecar.byte_size);
        }
    }
    Ok((clips, bytes))
}

/// Enforce `policy` against the current queue state. On
/// `DropOldestUndelivered`, evict until both bounds would be satisfied
/// once `incoming_bytes` lands in the queue. On `RefuseNew`, return
/// `InboxError::QueueFull` when either ceiling would be breached by the
/// incoming clip.
pub fn apply_policy(
    store: &QueueStore,
    policy: QueuePolicy,
    incoming_bytes: u64,
) -> Result<(), InboxError> {
    let (mut clips, mut bytes) = count_queue(store)?;
    let needs_eviction =
        clips + 1 > policy.max_clips || bytes.saturating_add(incoming_bytes) > policy.max_bytes;
    if !needs_eviction {
        return Ok(());
    }

    match policy.eviction {
        EvictionPolicy::RefuseNew => Err(InboxError::QueueFull {
            current_clips: clips,
            max_clips: policy.max_clips,
            current_bytes: bytes,
            max_bytes: policy.max_bytes,
        }),
        EvictionPolicy::DropOldestUndelivered => {
            let mut candidates = enumerate_evictable(store)?;
            while clips + 1 > policy.max_clips
                || bytes.saturating_add(incoming_bytes) > policy.max_bytes
            {
                let Some(candidate) = candidates.pop_front() else {
                    // Ran out of evictable entries. Surface as QueueFull
                    // so the capture side gets a structured refusal.
                    return Err(InboxError::QueueFull {
                        current_clips: clips,
                        max_clips: policy.max_clips,
                        current_bytes: bytes,
                        max_bytes: policy.max_bytes,
                    });
                };

                let reason = if clips + 1 > policy.max_clips {
                    EvictionReason::MaxClipsExceeded
                } else {
                    EvictionReason::MaxBytesExceeded
                };

                evict(store, &candidate)?;
                clips = clips.saturating_sub(1);
                bytes = bytes.saturating_sub(candidate.entry.byte_size);

                tracing::warn!(
                    event = obs_tracing::events::QUEUE_EVICTED,
                    clip_id = %candidate.entry.clip_id,
                    reason = reason.as_str(),
                    policy = policy_as_str(policy.eviction),
                    remaining_clips = clips,
                    remaining_bytes = bytes,
                    "queue eviction issued",
                );
            }
            Ok(())
        }
    }
}

/// One evictable entry: where it lives + the sidecar contents.
#[derive(Debug, Clone)]
struct EvictableEntry {
    location: EvictableLocation,
    entry: ClipQueueEntry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvictableLocation {
    DeliveredUndeliverable,
    Pending,
}

/// Enumerate evictable entries in the preferred order: oldest
/// `delivered/` Undeliverable first, then oldest `pending/`. Returns a
/// `VecDeque` so `pop_front` walks oldest-first.
fn enumerate_evictable(
    store: &QueueStore,
) -> Result<std::collections::VecDeque<EvictableEntry>, QueueError> {
    let mut undeliverable: Vec<EvictableEntry> = Vec::new();
    for sidecar in read_sidecars(&store.delivered_dir())? {
        if sidecar.outcome == Some(Outcome::Undeliverable) {
            undeliverable.push(EvictableEntry {
                location: EvictableLocation::DeliveredUndeliverable,
                entry: sidecar,
            });
        }
    }
    undeliverable.sort_by_key(|e| sort_key(&e.entry));

    let mut pending: Vec<EvictableEntry> = read_sidecars(&store.pending_dir())?
        .into_iter()
        .map(|entry| EvictableEntry { location: EvictableLocation::Pending, entry })
        .collect();
    pending.sort_by_key(|e| sort_key(&e.entry));

    let mut out = std::collections::VecDeque::with_capacity(undeliverable.len() + pending.len());
    for item in undeliverable {
        out.push_back(item);
    }
    for item in pending {
        out.push_back(item);
    }
    Ok(out)
}

fn sort_key(entry: &ClipQueueEntry) -> (DateTime<Utc>, String) {
    (entry.captured_at, entry.clip_id.clone())
}

fn read_sidecars(dir: &Path) -> Result<Vec<ClipQueueEntry>, QueueError> {
    let mut out = Vec::new();
    let read_dir =
        fs::read_dir(dir).map_err(|source| QueueError::Io { path: dir.to_path_buf(), source })?;
    for entry in read_dir {
        let entry = entry.map_err(|source| QueueError::Io { path: dir.to_path_buf(), source })?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let bytes =
            fs::read(&path).map_err(|source| QueueError::Io { path: path.clone(), source })?;
        let sidecar: ClipQueueEntry = serde_json::from_slice(&bytes)
            .map_err(|source| QueueError::Deserialise { path: path.clone(), source })?;
        out.push(sidecar);
    }
    Ok(out)
}

/// Delete the sidecar (and its mp4 sibling, if any) for `candidate`.
/// `delivered/` Undeliverable entries have no mp4; `pending/` entries do.
fn evict(store: &QueueStore, candidate: &EvictableEntry) -> Result<(), QueueError> {
    let clip_id = &candidate.entry.clip_id;
    let dir = match candidate.location {
        EvictableLocation::DeliveredUndeliverable => store.delivered_dir(),
        EvictableLocation::Pending => store.pending_dir(),
    };
    let sidecar = dir.join(format!("{clip_id}.json"));
    let mp4 = dir.join(format!("{clip_id}.mp4"));

    match fs::remove_file(&mp4) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(QueueError::Io { path: mp4, source }),
    }
    match fs::remove_file(&sidecar) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(QueueError::Io { path: sidecar, source }),
    }
    Ok(())
}

/// Age out `delivered/` sidecars that are fully done with — `outcome ==
/// Delivered` and the classify task has reached a terminal status (or was
/// lost) — and whose `delivered_at` predates `before`. Returns the pruned
/// clip-ids.
///
/// Bounds `delivered/` growth so the classify poller's per-tick scan stays
/// cheap on a long-running station (PS-25). Still-pollable entries
/// (non-terminal classify) are never touched, `Undeliverable` entries are
/// left to the pressure-driven eviction policy (so `status` can still
/// surface the last failure), and a sidecar that fails to read/parse is left
/// in place for the poller's corrupt-quarantine path (PS-02). Only a
/// `read_dir` failure aborts; per-entry removal failures are logged and the
/// entry is retried on the next round.
pub fn prune_delivered(
    store: &QueueStore,
    before: DateTime<Utc>,
) -> Result<Vec<String>, QueueError> {
    let delivered = store.delivered_dir();
    let read_dir = fs::read_dir(&delivered)
        .map_err(|source| QueueError::Io { path: delivered.clone(), source })?;
    let mut pruned = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|source| QueueError::Io { path: delivered.clone(), source })?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(sidecar) = serde_json::from_slice::<ClipQueueEntry>(&bytes) else {
            continue;
        };
        if !is_prunable(&sidecar, before) {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to prune delivered sidecar; will retry next round",
                );
                continue;
            }
        }
        // `delivered/` sidecars have no mp4 by invariant; sweep any stray
        // sibling defensively (best-effort).
        let _ = fs::remove_file(path.with_extension("mp4"));
        tracing::debug!(
            event = obs_tracing::events::QUEUE_PRUNED_DELIVERED,
            clip_id = %sidecar.clip_id,
            "pruned terminal delivered sidecar past retention",
        );
        pruned.push(sidecar.clip_id);
    }
    Ok(pruned)
}

/// `true` when a `delivered/` entry is safe to age out: a successful upload
/// whose classify task is finished (terminal status or lost) and which has
/// sat in `delivered/` since before `before`.
fn is_prunable(entry: &ClipQueueEntry, before: DateTime<Utc>) -> bool {
    if entry.outcome != Some(Outcome::Delivered) {
        return false;
    }
    let classify_done = entry.classify_lost_at.is_some()
        || entry.last_classify_status.is_some_and(ClassifyTaskStatus::is_terminal);
    classify_done && entry.delivered_at.is_some_and(|t| t < before)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perchpub::types::ClassifyTaskStatus;
    use crate::queue::store::QueueStore;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn instant(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn write_delivered(store: &QueueStore, entry: &ClipQueueEntry) {
        let path = store.delivered_dir().join(format!("{}.json", entry.clip_id));
        std::fs::write(path, serde_json::to_vec_pretty(entry).unwrap()).unwrap();
    }

    fn delivered_success(id: &str, delivered_at: &str) -> ClipQueueEntry {
        let mut e =
            ClipQueueEntry::new(id, instant("2026-01-01T00:00:00Z"), instant(delivered_at), 100);
        e.outcome = Some(Outcome::Delivered);
        e.classify_task_id = Some(Uuid::new_v4());
        e.last_classify_status = Some(ClassifyTaskStatus::Success);
        e.delivered_at = Some(instant(delivered_at));
        e
    }

    #[test]
    fn delivered_terminal_entries_are_aged_out() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        // (1) Old + terminal-classify Delivered → prunable.
        let old = "20260101T000000Z-001";
        write_delivered(&store, &delivered_success(old, "2026-01-01T00:00:01Z"));

        // (2) Fresh terminal Delivered → retained (inside the window).
        let fresh = "20260601T000000Z-001";
        write_delivered(&store, &delivered_success(fresh, "2026-06-01T00:00:01Z"));

        // (3) Old but still-pollable (non-terminal classify) → retained.
        let pollable = "20260101T000000Z-002";
        let mut p = delivered_success(pollable, "2026-01-01T00:00:01Z");
        p.last_classify_status = Some(ClassifyTaskStatus::Processing);
        write_delivered(&store, &p);

        // Cutoff: anything delivered before 2026-03-01 ages out.
        let pruned = prune_delivered(&store, instant("2026-03-01T00:00:00Z")).expect("prune");

        assert_eq!(pruned, vec![old.to_string()], "only the old terminal entry is pruned");
        assert!(!store.delivered_dir().join(format!("{old}.json")).exists());
        assert!(
            store.delivered_dir().join(format!("{fresh}.json")).exists(),
            "fresh terminal entry retained",
        );
        assert!(
            store.delivered_dir().join(format!("{pollable}.json")).exists(),
            "still-pollable entry retained",
        );
    }

    #[test]
    fn prune_skips_corrupt_sidecar_without_erroring() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        std::fs::write(store.delivered_dir().join("20260101T000000Z-001.json"), b"{ not json")
            .unwrap();

        // A corrupt sidecar is left in place for the poller's quarantine path.
        let pruned = prune_delivered(&store, instant("2099-01-01T00:00:00Z")).expect("prune");
        assert!(pruned.is_empty());
        assert!(store.delivered_dir().join("20260101T000000Z-001.json").is_file());
    }
}
