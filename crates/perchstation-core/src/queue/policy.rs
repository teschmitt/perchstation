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
//! 3. `inflight/` and `delivered/` `Delivered` entries are never touched —
//!    they form an **un-evictable floor**. The census counts them toward the
//!    ceilings (they occupy slots) but the eviction loop can never free them,
//!    so if the floor alone already breaches a ceiling the submission is
//!    refused *before* deleting any evictable clip (PS-05) — never destroy
//!    fresh `pending/` data that eviction cannot make room for.
//!
//! Byte accounting only ever counts media that is actually on disk:
//! `pending/` and `inflight/` carry their `.mp4`, but `delivered/` entries had
//! it unlinked on success, so their `byte_size` is phantom and excluded
//! (PS-20).

use std::collections::VecDeque;
use std::fs;
use std::path::Path;

use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;

use crate::config::{EvictionPolicy, QueueConfig};
use crate::observability::tracing as obs_tracing;
use crate::perchpub::types::ClassifyTaskStatus;

use super::inbox::Inbox;
use super::store::{ClipMeta, QueueStore, read_sidecar};
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
    /// Both ceilings are breached at this eviction (PS-21). Reported instead of
    /// silently attributing the pressure to clip count alone, which hid byte
    /// pressure from the `queue.evicted` telemetry.
    BothExceeded,
}

impl EvictionReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxClipsExceeded => "max_clips_exceeded",
            Self::MaxBytesExceeded => "max_bytes_exceeded",
            Self::BothExceeded => "both_exceeded",
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
        // PS-27: the preflight does a synchronous `read_dir` + per-sidecar
        // `fs::read` + parse across all three queue dirs (and re-reads on
        // eviction). At the configured 500-entry ceiling on a slow SD card
        // that is enough blocking I/O to stall the async reactor on every
        // capture, delaying delivery/classify polling. Run it on a blocking
        // thread instead.
        let store = self.store.clone();
        let policy = self.policy;
        let outcome =
            tokio::task::spawn_blocking(move || apply_policy(&store, policy, incoming_bytes))
                .await
                .expect("queue policy preflight task panicked");
        // Emit the `queue.evicted` events HERE, after the blocking call
        // returns, so they land in the caller's tracing scope. `spawn_blocking`
        // does not inherit the caller's `DefaultGuard` subscriber, so emitting
        // them from inside `apply_policy` would lose them in scoped-subscriber
        // tests and pin the per-callsite interest cache to `Never`.
        //
        // Emit for EVERY clip that was actually removed from disk *before*
        // propagating `outcome.result` — a later eviction may have failed after
        // earlier ones succeeded, and a deleted clip must never be dropped from
        // the telemetry.
        for eviction in &outcome.evictions {
            tracing::warn!(
                event = obs_tracing::events::QUEUE_EVICTED,
                clip_id = %eviction.clip_id,
                reason = eviction.reason.as_str(),
                policy = policy_as_str(policy.eviction),
                remaining_clips = eviction.remaining_clips,
                remaining_bytes = eviction.remaining_bytes,
                "queue eviction issued",
            );
        }
        outcome.result?;
        self.inner.submit(clip_path, meta).await
    }
}

/// One eviction performed by [`apply_policy`]. Returned (rather than logged
/// in-place) so the caller can emit the `queue.evicted` event from its own
/// tracing scope — the preflight runs on a `spawn_blocking` thread that does
/// not inherit the caller's subscriber (PS-27).
#[derive(Debug, Clone)]
pub struct EvictionRecord {
    pub clip_id: String,
    pub reason: EvictionReason,
    pub remaining_clips: u32,
    pub remaining_bytes: u64,
}

/// Outcome of a policy preflight ([`apply_policy`]).
///
/// `evictions` lists the clips that were actually removed from disk, in order.
/// The caller must emit `queue.evicted` for all of them **regardless of
/// `result`** — a later eviction can fail after earlier ones succeeded, and a
/// deleted clip must not vanish from the telemetry. `result` is `Ok` once the
/// queue has room for the incoming clip, or `Err` if the submission must be
/// refused (`QueueFull`) or a store error aborted the sweep.
pub struct PolicyOutcome {
    pub evictions: Vec<EvictionRecord>,
    pub result: Result<(), InboxError>,
}

/// A snapshot of the queue split into the part eviction can free and the part
/// it cannot, with byte totals counting only media that is actually on disk.
struct Census {
    /// Clips eviction can never free: `inflight/` (mid-upload) and
    /// `delivered/` `Delivered`. They occupy slots but stay put.
    floor_clips: u32,
    /// Bytes held by the un-evictable floor. Only `inflight/` carries media;
    /// `delivered/` had its mp4 unlinked on success (PS-20).
    floor_bytes: u64,
    /// Evictable entries oldest-first: `delivered/` Undeliverable, then
    /// `pending/`. `pop_front` walks oldest-first.
    candidates: VecDeque<EvictableEntry>,
}

/// Bytes the whole queue currently occupies on disk, and the clip slots it
/// fills, as the ceilings see them.
fn census_totals(census: &Census) -> (u32, u64) {
    let candidate_clips = u32::try_from(census.candidates.len()).unwrap_or(u32::MAX);
    let candidate_bytes: u64 = census.candidates.iter().map(EvictableEntry::on_disk_bytes).sum();
    (
        census.floor_clips.saturating_add(candidate_clips),
        census.floor_bytes.saturating_add(candidate_bytes),
    )
}

/// Enforce `policy` against the current queue state. On
/// `DropOldestUndelivered`, evict oldest-first until both bounds would be
/// satisfied once `incoming_bytes` lands in the queue — but refuse *without*
/// evicting anything if the un-evictable floor alone already breaches a
/// ceiling (PS-05). On `RefuseNew`, refuse with `InboxError::QueueFull` when
/// either ceiling would be breached by the incoming clip.
///
/// Returns a [`PolicyOutcome`]: the evictions actually performed (so the caller
/// can emit `queue.evicted` from its own tracing scope — PS-27) plus the
/// `Ok`/`Err` verdict. Evictions are reported even when the verdict is `Err`,
/// since a store error can abort the sweep after earlier clips were removed.
#[must_use]
pub fn apply_policy(store: &QueueStore, policy: QueuePolicy, incoming_bytes: u64) -> PolicyOutcome {
    let census = match take_census(store) {
        Ok(census) => census,
        Err(err) => return PolicyOutcome { evictions: Vec::new(), result: Err(err.into()) },
    };
    let (total_clips, total_bytes) = census_totals(&census);

    let breaches = |clips: u32, bytes: u64| {
        clips.saturating_add(1) > policy.max_clips
            || bytes.saturating_add(incoming_bytes) > policy.max_bytes
    };
    if !breaches(total_clips, total_bytes) {
        return PolicyOutcome { evictions: Vec::new(), result: Ok(()) };
    }

    let queue_full = || InboxError::QueueFull {
        current_clips: total_clips,
        max_clips: policy.max_clips,
        current_bytes: total_bytes,
        max_bytes: policy.max_bytes,
    };

    match policy.eviction {
        EvictionPolicy::RefuseNew => {
            PolicyOutcome { evictions: Vec::new(), result: Err(queue_full()) }
        }
        EvictionPolicy::DropOldestUndelivered => {
            // PS-05: if the floor that eviction CANNOT free already breaches a
            // ceiling, evicting every candidate still won't make room — refuse
            // up front rather than destroying fresh pending clips for nothing.
            if breaches(census.floor_clips, census.floor_bytes) {
                return PolicyOutcome { evictions: Vec::new(), result: Err(queue_full()) };
            }

            let mut clips = total_clips;
            let mut bytes = total_bytes;
            let mut candidates = census.candidates;
            let mut evictions = Vec::new();
            while breaches(clips, bytes) {
                let Some(candidate) = candidates.pop_front() else {
                    // Unreachable given the floor check above (evicting all
                    // candidates lands us at the floor, which is under bounds),
                    // but keep a structured refusal as a defensive backstop.
                    return PolicyOutcome { evictions, result: Err(queue_full()) };
                };

                // PS-21: attribute the eviction to the ceiling(s) actually
                // breached so byte pressure isn't hidden behind clip count.
                let over_clips = clips.saturating_add(1) > policy.max_clips;
                let over_bytes = bytes.saturating_add(incoming_bytes) > policy.max_bytes;
                let reason = match (over_clips, over_bytes) {
                    (true, true) => EvictionReason::BothExceeded,
                    (false, true) => EvictionReason::MaxBytesExceeded,
                    _ => EvictionReason::MaxClipsExceeded,
                };

                // A store error here aborts the sweep, but the clips already
                // removed are returned so the caller still logs them.
                if let Err(err) = evict(store, &candidate) {
                    return PolicyOutcome { evictions, result: Err(err.into()) };
                }
                clips = clips.saturating_sub(1);
                bytes = bytes.saturating_sub(candidate.on_disk_bytes());

                evictions.push(EvictionRecord {
                    clip_id: candidate.entry.clip_id,
                    reason,
                    remaining_clips: clips,
                    remaining_bytes: bytes,
                });
            }
            PolicyOutcome { evictions, result: Ok(()) }
        }
    }
}

/// One evictable entry: where it lives + the sidecar contents.
#[derive(Debug, Clone)]
struct EvictableEntry {
    location: EvictableLocation,
    entry: ClipQueueEntry,
}

impl EvictableEntry {
    /// Bytes this entry's media occupies on disk. `pending/` entries carry
    /// their `.mp4`; `delivered/` Undeliverable entries had it unlinked, so
    /// they free no bytes when evicted (PS-20).
    fn on_disk_bytes(&self) -> u64 {
        match self.location {
            EvictableLocation::Pending => self.entry.byte_size,
            EvictableLocation::DeliveredUndeliverable => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvictableLocation {
    DeliveredUndeliverable,
    Pending,
}

/// Snapshot the queue: tally the un-evictable floor (`inflight/` +
/// `delivered/` `Delivered`) and collect the evictable candidates
/// (`delivered/` Undeliverable, then `pending/`) oldest-first.
fn take_census(store: &QueueStore) -> Result<Census, QueueError> {
    let mut floor_clips = 0_u32;
    let mut floor_bytes = 0_u64;

    // `inflight/`: un-evictable, media on disk.
    for entry in read_sidecars(&store.inflight_dir())? {
        floor_clips = floor_clips.saturating_add(1);
        floor_bytes = floor_bytes.saturating_add(entry.byte_size);
    }

    // `delivered/`: Undeliverable is evictable (no media); everything else
    // (Delivered, or a defensive outcome-less sidecar) is un-evictable floor
    // and holds no media bytes (PS-20).
    let mut undeliverable: Vec<EvictableEntry> = Vec::new();
    for entry in read_sidecars(&store.delivered_dir())? {
        if entry.outcome == Some(Outcome::Undeliverable) {
            undeliverable.push(EvictableEntry {
                location: EvictableLocation::DeliveredUndeliverable,
                entry,
            });
        } else {
            floor_clips = floor_clips.saturating_add(1);
        }
    }
    undeliverable.sort_by_key(|e| sort_key(&e.entry));

    // `pending/`: evictable, media on disk.
    let mut pending: Vec<EvictableEntry> = read_sidecars(&store.pending_dir())?
        .into_iter()
        .map(|entry| EvictableEntry { location: EvictableLocation::Pending, entry })
        .collect();
    pending.sort_by_key(|e| sort_key(&e.entry));

    let mut candidates = VecDeque::with_capacity(undeliverable.len() + pending.len());
    candidates.extend(undeliverable);
    candidates.extend(pending);

    Ok(Census { floor_clips, floor_bytes, candidates })
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
        match read_sidecar(&path) {
            Ok(sidecar) => out.push(sidecar),
            // PS-10: a concurrent eviction or `transition_inflight` may unlink
            // this sidecar between `read_dir` listing it and this read. Treat a
            // vanished file as absent rather than failing the whole census.
            Err(QueueError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                tracing::trace!(path = %path.display(), "sidecar vanished mid-census; skipping");
            }
            Err(err) => return Err(err),
        }
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
    let sidecar = dir.join(QueueStore::sidecar_name(clip_id));
    let mp4 = dir.join(QueueStore::media_name(clip_id));

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

    // PS-10: the eviction census must not hard-error when a sidecar it
    // enumerated vanishes before it can be read (a concurrent transition /
    // eviction unlinked it). A dangling symlink is a deterministic stand-in:
    // `read_dir` lists it, but `fs::read` follows it and returns `NotFound`.
    #[cfg(unix)]
    #[test]
    fn apply_policy_tolerates_sidecar_that_vanishes_mid_census() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        // A `pending/<id>.json` symlink pointing at a non-existent target.
        let dangling = store.pending_dir().join("20260101T000000Z-001.json");
        std::os::unix::fs::symlink(dir.path().join("does-not-exist.json"), &dangling).unwrap();

        // Huge bounds → no eviction is needed, but the census still reads every
        // pending sidecar. It must skip the vanished one rather than failing.
        let policy = QueuePolicy {
            max_clips: u32::MAX,
            max_bytes: u64::MAX,
            eviction: EvictionPolicy::DropOldestUndelivered,
        };
        assert!(
            apply_policy(&store, policy, 0).result.is_ok(),
            "a sidecar that vanished mid-census must be skipped, not fail the whole preflight",
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
