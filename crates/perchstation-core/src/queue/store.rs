//! Directory-of-files queue store with [`rename`](std::fs::rename)-atomic
//! state transitions.
//!
//! Encodes the on-disk layout from `specs/001-clip-delivery/data-model.md`:
//!
//! ```text
//! <data_dir>/queue/
//! ├── pending/    <clip-id>.mp4 + <clip-id>.json
//! ├── inflight/   <clip-id>.mp4 + <clip-id>.json
//! └── delivered/  <clip-id>.json   (mp4 unlinked on success)
//! ```
//!
//! `<clip-id>` is `<capture_utc_basic>-<seq>` where the basic-ISO-8601 prefix
//! makes lexicographic order match capture order (FR-006).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{DateTime, Utc};

use crate::observability::tracing::events;

use super::{ClipQueueEntry, QueueError};

const QUEUE_DIR: &str = "queue";
const PENDING: &str = "pending";
const INFLIGHT: &str = "inflight";
const DELIVERED: &str = "delivered";
/// Graveyard for sidecars that fail to deserialise (PS-02) — moving them
/// here lets a scan head advance permanently instead of choking on the
/// same corrupt file every tick.
const CORRUPT: &str = "corrupt";

/// Metadata the capture subsystem hands the queue alongside the clip media.
#[derive(Debug, Clone, Copy)]
pub struct ClipMeta {
    /// Wall-clock time the clip was captured. The basic-ISO-8601 form of
    /// this stamps the clip-id prefix so `pending/` orders chronologically.
    pub captured_at: DateTime<Utc>,
}

/// Handle to the on-disk queue rooted at `<data_dir>/queue/`. Construction
/// is cheap (path arithmetic + `mkdir -p` on the three subdirs); the type
/// itself is `Send + Sync` because it owns only a `PathBuf`.
#[derive(Debug, Clone)]
pub struct QueueStore {
    root: PathBuf,
}

impl QueueStore {
    /// Open (and ensure) the queue layout under `<data_dir>/queue/`. The
    /// three state subdirectories are created on first use.
    pub fn open(data_dir: &Path) -> Result<Self, QueueError> {
        let root = data_dir.join(QUEUE_DIR);
        for sub in [PENDING, INFLIGHT, DELIVERED, CORRUPT] {
            let path = root.join(sub);
            fs::create_dir_all(&path)
                .map_err(|source| QueueError::Io { path: path.clone(), source })?;
        }
        Ok(Self { root })
    }

    #[must_use]
    pub fn pending_dir(&self) -> PathBuf {
        self.root.join(PENDING)
    }

    #[must_use]
    pub fn inflight_dir(&self) -> PathBuf {
        self.root.join(INFLIGHT)
    }

    #[must_use]
    pub fn delivered_dir(&self) -> PathBuf {
        self.root.join(DELIVERED)
    }

    #[must_use]
    pub fn corrupt_dir(&self) -> PathBuf {
        self.root.join(CORRUPT)
    }

    /// Move a freshly-captured clip into `pending/`. Stages the mp4 via a
    /// `.tmp` suffix then renames; writes the sidecar via tmp + rename.
    pub fn enqueue(
        &self,
        clip_source: &Path,
        meta: ClipMeta,
    ) -> Result<ClipQueueEntry, QueueError> {
        let clip_id = next_clip_id(meta.captured_at);
        self.enqueue_with_id(&clip_id, clip_source, meta)
    }

    /// Core of [`enqueue`] with the clip-id supplied by the caller. Split
    /// out so tests can drive a deterministic id (the production id is a
    /// process-local atomic that parallel tests cannot predict).
    fn enqueue_with_id(
        &self,
        clip_id: &str,
        clip_source: &Path,
        meta: ClipMeta,
    ) -> Result<ClipQueueEntry, QueueError> {
        let pending = self.pending_dir();

        let metadata = fs::metadata(clip_source)
            .map_err(|source| QueueError::Io { path: clip_source.to_path_buf(), source })?;
        let byte_size = metadata.len();

        let mp4_target = pending.join(format!("{clip_id}.mp4"));
        let mp4_tmp = pending.join(format!("{clip_id}.mp4.tmp"));

        // Try rename first (same-filesystem); fall back to copy+remove for
        // cross-filesystem sources (e.g., capture stages clips under /tmp).
        if fs::rename(clip_source, &mp4_target).is_err() {
            fs::copy(clip_source, &mp4_tmp)
                .map_err(|source| QueueError::Io { path: mp4_tmp.clone(), source })?;
            fs::rename(&mp4_tmp, &mp4_target)
                .map_err(|source| QueueError::Io { path: mp4_target.clone(), source })?;
            // Best-effort cleanup of the original; do not surface failures
            // (the clip is safely in `pending/` regardless).
            let _ = fs::remove_file(clip_source);
        }

        let entry = ClipQueueEntry::new(clip_id, meta.captured_at, Utc::now(), byte_size);
        let sidecar_target = pending.join(format!("{clip_id}.json"));
        if let Err(err) = write_sidecar_atomic(&sidecar_target, &entry) {
            // PS-07: the mp4 is already staged in pending/; a failed sidecar
            // write would leave it orphaned (invisible to every json-keyed
            // scan → a silent disk leak that eats the eviction budget).
            // Best-effort remove the media before surfacing the error.
            let _ = fs::remove_file(&mp4_target);
            return Err(err);
        }

        Ok(entry)
    }

    /// Lexicographically-smallest entry in `pending/` whose
    /// `next_attempt_after` is `None` or has elapsed. Returns `None` if the
    /// queue is empty or every entry is still in backoff.
    pub fn pick_oldest_pending(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<ClipQueueEntry>, QueueError> {
        let pending = self.pending_dir();
        let mut sidecars: Vec<PathBuf> = Vec::new();
        let read_dir = fs::read_dir(&pending)
            .map_err(|source| QueueError::Io { path: pending.clone(), source })?;
        for entry in read_dir {
            let entry = entry.map_err(|source| QueueError::Io { path: pending.clone(), source })?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                sidecars.push(path);
            }
        }
        sidecars.sort();

        for path in sidecars {
            let entry = match read_sidecar(&path) {
                Ok(entry) => entry,
                Err(QueueError::Deserialise { .. }) => {
                    // PS-02: a single corrupt sidecar must not wedge the
                    // whole scan (and with it the delivery loop). Quarantine
                    // it so the head advances permanently.
                    tracing::warn!(
                        event = events::QUEUE_CORRUPT_SIDECAR,
                        path = %path.display(),
                        "quarantining corrupt pending sidecar",
                    );
                    if let Err(err) = self.quarantine_corrupt(&path) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "failed to quarantine corrupt pending sidecar",
                        );
                    }
                    continue;
                }
                Err(err) => {
                    // Per-file I/O error (e.g. a concurrent eviction removed
                    // the file mid-scan). Skip it; do not abort the scan.
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "skipping unreadable pending sidecar",
                    );
                    continue;
                }
            };
            if let Some(next) = entry.next_attempt_after
                && next > now
            {
                continue;
            }
            return Ok(Some(entry));
        }
        Ok(None)
    }

    /// Move an entry from `pending/` to `inflight/`, bumping `attempts`,
    /// stamping `first_attempt_at` / `last_attempt_at`, and clearing
    /// `next_attempt_after` / `last_error` so the upload runs against a
    /// fresh-attempt sidecar.
    ///
    /// Steps (each is `rename`-atomic w.r.t. readers):
    /// 1. Rename `pending/<id>.mp4` → `inflight/<id>.mp4`.
    /// 2. Write the updated sidecar to `inflight/<id>.json` via tmp + rename.
    /// 3. Remove `pending/<id>.json`.
    pub fn transition_inflight(
        &self,
        entry: ClipQueueEntry,
        now: DateTime<Utc>,
    ) -> Result<ClipQueueEntry, QueueError> {
        let clip_id = entry.clip_id.clone();
        let pending_mp4 = self.pending_dir().join(format!("{clip_id}.mp4"));
        let pending_sidecar = self.pending_dir().join(format!("{clip_id}.json"));
        let inflight_mp4 = self.inflight_dir().join(format!("{clip_id}.mp4"));
        let inflight_sidecar = self.inflight_dir().join(format!("{clip_id}.json"));

        if !pending_mp4.exists() {
            return Err(QueueError::MissingMedia { clip_id });
        }

        let mut updated = entry;
        updated.attempts = updated.attempts.saturating_add(1);
        if updated.first_attempt_at.is_none() {
            updated.first_attempt_at = Some(now);
        }
        updated.last_attempt_at = Some(now);
        updated.next_attempt_after = None;
        updated.last_error = None;

        fs::rename(&pending_mp4, &inflight_mp4)
            .map_err(|source| QueueError::Io { path: inflight_mp4.clone(), source })?;

        if let Err(err) = write_sidecar_atomic(&inflight_sidecar, &updated) {
            // PS-04: roll the mp4 back to pending/ so it isn't stranded in
            // inflight/ with no sidecar (where reconcile_inflight, which
            // enumerates only inflight/*.json, could never recover it).
            let _ = fs::rename(&inflight_mp4, &pending_mp4);
            return Err(err);
        }

        match fs::remove_file(&pending_sidecar) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(QueueError::Io { path: pending_sidecar, source }),
        }

        Ok(updated)
    }

    /// Atomically update the sidecar of an entry that already lives in
    /// `delivered/`. Used by the classify poller (T037) to record the
    /// latest `last_classify_status` and `observation_id` without moving
    /// the entry.
    pub fn update_delivered_sidecar(&self, entry: &ClipQueueEntry) -> Result<(), QueueError> {
        let path = self.delivered_dir().join(format!("{}.json", entry.clip_id));
        write_sidecar_atomic(&path, entry)
    }

    /// Move an entry from `inflight/` back to `pending/` after a
    /// transient failure. Persists the updated sidecar (`attempts`,
    /// `last_error`, `next_attempt_after`) atomically. Idempotent
    /// against missing files so the runner can call this even after a
    /// partial transition.
    pub fn transition_back_to_pending(&self, entry: &ClipQueueEntry) -> Result<(), QueueError> {
        let clip_id = &entry.clip_id;
        let inflight_mp4 = self.inflight_dir().join(format!("{clip_id}.mp4"));
        let inflight_sidecar = self.inflight_dir().join(format!("{clip_id}.json"));
        let pending_mp4 = self.pending_dir().join(format!("{clip_id}.mp4"));
        let pending_sidecar = self.pending_dir().join(format!("{clip_id}.json"));

        if inflight_mp4.exists() {
            fs::rename(&inflight_mp4, &pending_mp4)
                .map_err(|source| QueueError::Io { path: pending_mp4.clone(), source })?;
        }
        write_sidecar_atomic(&pending_sidecar, entry)?;
        match fs::remove_file(&inflight_sidecar) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(QueueError::Io { path: inflight_sidecar, source }),
        }
        Ok(())
    }

    /// Enumerate `inflight/` and move every clip+sidecar pair back into
    /// `pending/`. Resets `next_attempt_after` so the runner picks the
    /// re-queued entries up immediately. Called once at process start
    /// by `commands::serve` before `service.ready`. Idempotent against
    /// a clean `inflight/`.
    pub fn reconcile_inflight(&self) -> Result<Vec<ClipQueueEntry>, QueueError> {
        // PS-07: reclaim orphan media (an `*.mp4` with no matching `*.json`)
        // from pending/ and inflight/ before reconciling — a crash between
        // the media rename and the sidecar write strands media invisible to
        // every json-keyed scan.
        sweep_orphan_media(&self.pending_dir())?;
        sweep_orphan_media(&self.inflight_dir())?;

        let inflight = self.inflight_dir();
        let mut sidecars: Vec<PathBuf> = Vec::new();
        let read_dir = fs::read_dir(&inflight)
            .map_err(|source| QueueError::Io { path: inflight.clone(), source })?;
        for entry in read_dir {
            let entry =
                entry.map_err(|source| QueueError::Io { path: inflight.clone(), source })?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                sidecars.push(path);
            }
        }
        sidecars.sort();

        let mut recovered = Vec::with_capacity(sidecars.len());
        for sidecar in sidecars {
            let mut entry = read_sidecar(&sidecar)?;
            if entry.is_terminal() {
                // PS-01: a crash inside transition_delivered can leave a
                // terminal sidecar in inflight/. Finish the interrupted
                // transition (idempotent — the unlink/rename tolerate
                // NotFound) rather than re-queueing it, which would
                // re-upload an already-delivered clip and spawn a duplicate
                // classify task on perchpub.
                self.transition_delivered(&entry)?;
                continue;
            }
            entry.next_attempt_after = None;
            entry.last_error = None;
            self.transition_back_to_pending(&entry)?;
            recovered.push(entry);
        }
        Ok(recovered)
    }

    /// Move an entry from `inflight/` to `delivered/`.
    ///
    /// The caller pre-populates `entry.outcome`, `classify_task_id`,
    /// `delivered_at`, and `last_classify_status`. This method then:
    ///
    /// 1. Writes the updated sidecar to `inflight/<id>.json` (tmp + rename
    ///    — atomic in-place update of the terminal fields).
    /// 2. **Unlinks `inflight/<id>.mp4` BEFORE renaming the sidecar** —
    ///    the invariant from `data-model.md` §`ClipQueueEntry` so a crash
    ///    mid-transition leaves a recoverable state.
    /// 3. Renames `inflight/<id>.json` → `delivered/<id>.json`.
    pub fn transition_delivered(&self, entry: &ClipQueueEntry) -> Result<(), QueueError> {
        let clip_id = &entry.clip_id;
        let inflight_mp4 = self.inflight_dir().join(format!("{clip_id}.mp4"));
        let inflight_sidecar = self.inflight_dir().join(format!("{clip_id}.json"));
        let delivered_sidecar = self.delivered_dir().join(format!("{clip_id}.json"));

        write_sidecar_atomic(&inflight_sidecar, entry)?;

        match fs::remove_file(&inflight_mp4) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(QueueError::Io { path: inflight_mp4, source }),
        }

        fs::rename(&inflight_sidecar, &delivered_sidecar)
            .map_err(|source| QueueError::Io { path: delivered_sidecar, source })?;

        Ok(())
    }

    /// Resolve a `pending/` entry whose `.mp4` media is missing — a
    /// residual orphan (sidecar with no media) that `transition_inflight`
    /// would reject with [`QueueError::MissingMedia`] on every tick,
    /// wedging the delivery head (PS-04). Removes the orphan sidecar (and
    /// any stray media) idempotently so the head advances.
    pub fn quarantine_orphan(&self, clip_id: &str) -> Result<(), QueueError> {
        let sidecar = self.pending_dir().join(format!("{clip_id}.json"));
        let mp4 = self.pending_dir().join(format!("{clip_id}.mp4"));
        remove_if_exists(&sidecar)?;
        remove_if_exists(&mp4)?;
        Ok(())
    }

    /// Move a sidecar that failed to deserialise (and any same-stem `.mp4`)
    /// into `corrupt/` so the enclosing scan advances permanently instead
    /// of re-failing on the same file every tick (PS-02).
    pub fn quarantine_corrupt(&self, sidecar: &Path) -> Result<(), QueueError> {
        let corrupt = self.corrupt_dir();
        fs::create_dir_all(&corrupt)
            .map_err(|source| QueueError::Io { path: corrupt.clone(), source })?;
        if let Some(name) = sidecar.file_name() {
            let dest = corrupt.join(name);
            fs::rename(sidecar, &dest).map_err(|source| QueueError::Io { path: dest, source })?;
        }
        // Move any same-stem media alongside the bad sidecar (pending/
        // entries carry an `.mp4`; delivered/ ones do not).
        let mp4 = sidecar.with_extension("mp4");
        if mp4.exists()
            && let Some(name) = mp4.file_name()
        {
            let _ = fs::rename(&mp4, corrupt.join(name));
        }
        Ok(())
    }
}

/// Remove every `*.mp4` in `dir` whose matching `*.json` sidecar is
/// absent — media stranded by a crash between the media rename and the
/// sidecar write (PS-07). Best-effort per file; only a `read_dir` failure
/// aborts.
fn sweep_orphan_media(dir: &Path) -> Result<(), QueueError> {
    let read_dir =
        fs::read_dir(dir).map_err(|source| QueueError::Io { path: dir.to_path_buf(), source })?;
    for entry in read_dir {
        let entry = entry.map_err(|source| QueueError::Io { path: dir.to_path_buf(), source })?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "mp4") && !path.with_extension("json").exists() {
            tracing::warn!(
                event = events::QUEUE_ORPHAN_MEDIA,
                path = %path.display(),
                "sweeping sidecarless media at boot",
            );
            remove_if_exists(&path)?;
        }
    }
    Ok(())
}

/// Generate the next `<capture_utc_basic>-<seq>` clip-id. The basic ISO-8601
/// prefix orders lexicographically by capture time; `seq` (a process-local
/// atomic) breaks ties within a single second.
fn next_clip_id(captured_at: DateTime<Utc>) -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let basic = captured_at.format("%Y%m%dT%H%M%SZ");
    format!("{basic}-{seq:03}")
}

fn write_sidecar_atomic(target: &Path, entry: &ClipQueueEntry) -> Result<(), QueueError> {
    let tmp = target.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(entry).map_err(QueueError::Serialise)?;
    if let Err(err) = fs::write(&tmp, &bytes) {
        return Err(io_to_queue_err(err, &tmp));
    }
    if let Err(err) = fs::rename(&tmp, target) {
        return Err(io_to_queue_err(err, target));
    }
    Ok(())
}

/// Map an `io::Error` to a [`QueueError`], detecting `ENOSPC`
/// (`io::ErrorKind::StorageFull`) so the runner can react with the
/// `queue.disk_full` event and back off instead of tight-looping (T049a).
fn io_to_queue_err(err: io::Error, path: &Path) -> QueueError {
    if err.kind() == io::ErrorKind::StorageFull {
        QueueError::DiskFull { path: path.to_path_buf() }
    } else {
        QueueError::Io { path: path.to_path_buf(), source: err }
    }
}

/// Remove `path`, tolerating an already-absent file so callers can be
/// idempotent against partial/repeated cleanup.
fn remove_if_exists(path: &Path) -> Result<(), QueueError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(QueueError::Io { path: path.to_path_buf(), source }),
    }
}

fn read_sidecar(path: &Path) -> Result<ClipQueueEntry, QueueError> {
    let bytes =
        fs::read(path).map_err(|source| QueueError::Io { path: path.to_path_buf(), source })?;
    serde_json::from_slice(&bytes)
        .map_err(|source| QueueError::Deserialise { path: path.to_path_buf(), source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Outcome;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn instant(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn open_creates_pending_inflight_delivered() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).expect("open");
        assert!(store.pending_dir().is_dir());
        assert!(store.inflight_dir().is_dir());
        assert!(store.delivered_dir().is_dir());
    }

    #[test]
    fn enqueue_stages_clip_into_pending() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let src = dir.path().join("clip.mp4");
        fs::write(&src, b"the bytes").unwrap();

        let entry = store
            .enqueue(&src, ClipMeta { captured_at: instant("2026-05-27T12:00:00Z") })
            .expect("enqueue");

        assert_eq!(entry.byte_size, 9);
        assert!(entry.clip_id.starts_with("20260527T120000Z-"));
        let mp4 = store.pending_dir().join(format!("{}.mp4", entry.clip_id));
        let sidecar = store.pending_dir().join(format!("{}.json", entry.clip_id));
        assert!(mp4.is_file(), "mp4 should land in pending/");
        assert!(sidecar.is_file(), "sidecar should land in pending/");
        // Original source is gone (rename consumed it).
        assert!(!src.exists());
    }

    #[test]
    fn pick_oldest_pending_returns_lexicographically_smallest() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        // Hand-write two entries with deterministic IDs so we can assert order.
        for id in ["20260527T120100Z-001", "20260527T120000Z-001"] {
            fs::write(store.pending_dir().join(format!("{id}.mp4")), b"x").unwrap();
            let entry = ClipQueueEntry::new(id, instant("2026-05-27T12:00:00Z"), Utc::now(), 1);
            write_sidecar_atomic(&store.pending_dir().join(format!("{id}.json")), &entry).unwrap();
        }
        let picked = store.pick_oldest_pending(Utc::now()).unwrap().expect("some");
        assert_eq!(picked.clip_id, "20260527T120000Z-001");
    }

    #[test]
    fn pick_oldest_pending_skips_entries_blocked_by_backoff() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let id = "20260527T120000Z-001";
        fs::write(store.pending_dir().join(format!("{id}.mp4")), b"x").unwrap();
        let mut entry = ClipQueueEntry::new(id, Utc::now(), Utc::now(), 1);
        entry.next_attempt_after = Some(instant("2030-01-01T00:00:00Z"));
        write_sidecar_atomic(&store.pending_dir().join(format!("{id}.json")), &entry).unwrap();

        let now = instant("2026-05-27T12:00:00Z");
        assert!(store.pick_oldest_pending(now).unwrap().is_none());
    }

    #[test]
    fn transition_inflight_moves_pair_and_bumps_attempt_counters() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let id = "20260527T120000Z-001";
        fs::write(store.pending_dir().join(format!("{id}.mp4")), b"the bytes").unwrap();
        let entry = ClipQueueEntry::new(id, Utc::now(), Utc::now(), 9);
        write_sidecar_atomic(&store.pending_dir().join(format!("{id}.json")), &entry).unwrap();

        let now = instant("2026-05-27T12:00:30Z");
        let updated = store.transition_inflight(entry, now).expect("transition");

        assert_eq!(updated.attempts, 1);
        assert_eq!(updated.first_attempt_at, Some(now));
        assert_eq!(updated.last_attempt_at, Some(now));
        assert!(store.inflight_dir().join(format!("{id}.mp4")).is_file());
        assert!(store.inflight_dir().join(format!("{id}.json")).is_file());
        assert!(!store.pending_dir().join(format!("{id}.mp4")).exists());
        assert!(!store.pending_dir().join(format!("{id}.json")).exists());
    }

    #[test]
    fn transition_delivered_unlinks_mp4_before_renaming_sidecar() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let id = "20260527T120000Z-001";
        fs::write(store.inflight_dir().join(format!("{id}.mp4")), b"the bytes").unwrap();
        let mut entry = ClipQueueEntry::new(id, Utc::now(), Utc::now(), 9);
        entry.outcome = Some(Outcome::Delivered);
        entry.classify_task_id = Some(Uuid::new_v4());
        entry.delivered_at = Some(Utc::now());
        write_sidecar_atomic(&store.inflight_dir().join(format!("{id}.json")), &entry).unwrap();

        store.transition_delivered(&entry).expect("transition delivered");

        assert!(store.delivered_dir().join(format!("{id}.json")).is_file());
        assert!(
            !store.delivered_dir().join(format!("{id}.mp4")).exists(),
            "mp4 must not appear in delivered/",
        );
        assert!(!store.inflight_dir().join(format!("{id}.mp4")).exists(), "mp4 must be unlinked");
        assert!(!store.inflight_dir().join(format!("{id}.json")).exists());

        let on_disk: ClipQueueEntry = serde_json::from_slice(
            &fs::read(store.delivered_dir().join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk.outcome, Some(Outcome::Delivered));
    }

    #[test]
    fn clip_ids_are_monotonically_increasing_within_second() {
        let captured = instant("2026-05-27T12:00:00Z");
        let a = next_clip_id(captured);
        let b = next_clip_id(captured);
        assert!(b > a, "{b} should sort after {a}");
        // Basic-ISO prefix preserved.
        assert!(a.starts_with("20260527T120000Z-"));
        assert!(b.starts_with("20260527T120000Z-"));
    }

    #[test]
    fn pick_oldest_pending_returns_none_on_empty_queue() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        assert!(store.pick_oldest_pending(Utc::now()).unwrap().is_none());
    }

    #[test]
    fn transition_inflight_errors_when_mp4_missing() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let id = "20260527T120000Z-001";
        let entry = ClipQueueEntry::new(id, Utc::now(), Utc::now(), 9);
        write_sidecar_atomic(&store.pending_dir().join(format!("{id}.json")), &entry).unwrap();

        let err = store.transition_inflight(entry, Utc::now()).expect_err("must fail");
        match err {
            QueueError::MissingMedia { clip_id } => assert_eq!(clip_id, id),
            other => panic!("expected MissingMedia, got {other:?}"),
        }
    }

    // ---- PS-07: enqueue / boot orphan-media safety ----

    #[test]
    fn enqueue_removes_mp4_when_sidecar_write_fails() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let src = dir.path().join("clip.mp4");
        fs::write(&src, b"bytes").unwrap();

        // Force the sidecar write to fail *after* the mp4 has been staged:
        // occupy the sidecar's tmp path with a directory so `fs::write` errors.
        let id = "20260527T120000Z-700";
        fs::create_dir(store.pending_dir().join(format!("{id}.json.tmp"))).unwrap();

        let err = store
            .enqueue_with_id(id, &src, ClipMeta { captured_at: instant("2026-05-27T12:00:00Z") })
            .expect_err("sidecar write must fail");
        assert!(matches!(err, QueueError::Io { .. }), "got {err:?}");

        // The staged mp4 must not be left orphaned in pending/.
        assert!(
            !store.pending_dir().join(format!("{id}.mp4")).exists(),
            "orphan mp4 must be removed when the sidecar write fails",
        );
    }

    #[test]
    fn reconcile_sweeps_sidecarless_mp4() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        // An orphan mp4 in pending/ with no sidecar (crash between renames).
        let orphan = "20260527T120000Z-009";
        fs::write(store.pending_dir().join(format!("{orphan}.mp4")), b"orphan").unwrap();
        // A complete pending pair that must survive the sweep.
        let good = "20260527T120100Z-001";
        fs::write(store.pending_dir().join(format!("{good}.mp4")), b"x").unwrap();
        let entry = ClipQueueEntry::new(good, instant("2026-05-27T12:01:00Z"), Utc::now(), 1);
        write_sidecar_atomic(&store.pending_dir().join(format!("{good}.json")), &entry).unwrap();

        store.reconcile_inflight().expect("reconcile");

        assert!(
            !store.pending_dir().join(format!("{orphan}.mp4")).exists(),
            "sidecarless mp4 must be swept at boot",
        );
        assert!(store.pending_dir().join(format!("{good}.mp4")).exists(), "valid pair survives");
        assert!(store.pending_dir().join(format!("{good}.json")).exists(), "valid pair survives");
    }

    // ---- PS-04: transition rollback + orphan quarantine ----

    #[test]
    fn transition_inflight_rolls_back_mp4_on_sidecar_write_failure() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let id = "20260527T120000Z-001";
        fs::write(store.pending_dir().join(format!("{id}.mp4")), b"the bytes").unwrap();
        let entry = ClipQueueEntry::new(id, instant("2026-05-27T12:00:00Z"), Utc::now(), 9);
        write_sidecar_atomic(&store.pending_dir().join(format!("{id}.json")), &entry).unwrap();

        // Block the inflight sidecar's tmp path so write_sidecar_atomic fails
        // *after* the mp4 has been renamed into inflight/.
        fs::create_dir(store.inflight_dir().join(format!("{id}.json.tmp"))).unwrap();

        let err = store
            .transition_inflight(entry, instant("2026-05-27T12:00:30Z"))
            .expect_err("sidecar write must fail");
        assert!(matches!(err, QueueError::Io { .. }), "got {err:?}");

        // The mp4 must be rolled back to pending/, not stranded in inflight/.
        assert!(
            store.pending_dir().join(format!("{id}.mp4")).exists(),
            "mp4 must be rolled back to pending/",
        );
        assert!(
            !store.inflight_dir().join(format!("{id}.mp4")).exists(),
            "mp4 must not be stranded in inflight/",
        );
    }

    #[test]
    fn quarantine_orphan_removes_sidecar_idempotently() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let id = "20260527T120000Z-001";
        let entry = ClipQueueEntry::new(id, instant("2026-05-27T12:00:00Z"), Utc::now(), 9);
        write_sidecar_atomic(&store.pending_dir().join(format!("{id}.json")), &entry).unwrap();

        store.quarantine_orphan(id).expect("quarantine");
        assert!(!store.pending_dir().join(format!("{id}.json")).exists(), "orphan sidecar removed");

        // Idempotent: a second call against the now-absent sidecar is fine.
        store.quarantine_orphan(id).expect("quarantine is idempotent");
    }

    // ---- PS-01: reconcile must finish terminal entries, not re-queue ----

    #[test]
    fn reconcile_inflight_finishes_terminal_entry_left_in_inflight() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let id = "20260527T120000Z-001";
        // Simulate a crash inside transition_delivered: terminal sidecar still
        // in inflight/, mp4 not yet unlinked.
        fs::write(store.inflight_dir().join(format!("{id}.mp4")), b"the bytes").unwrap();
        let mut entry = ClipQueueEntry::new(id, instant("2026-05-27T12:00:00Z"), Utc::now(), 9);
        entry.outcome = Some(Outcome::Delivered);
        entry.classify_task_id = Some(Uuid::new_v4());
        entry.delivered_at = Some(instant("2026-05-27T12:00:30Z"));
        write_sidecar_atomic(&store.inflight_dir().join(format!("{id}.json")), &entry).unwrap();

        let recovered = store.reconcile_inflight().expect("reconcile");

        assert!(recovered.is_empty(), "terminal entry must not be re-queued");
        assert!(
            store.delivered_dir().join(format!("{id}.json")).is_file(),
            "interrupted transition must be finished to delivered/",
        );
        assert!(
            !store.pending_dir().join(format!("{id}.json")).exists(),
            "terminal entry must not land in pending/",
        );
        assert!(
            !store.inflight_dir().join(format!("{id}.mp4")).exists(),
            "mp4 must be unlinked while finishing the transition",
        );
        assert!(!store.inflight_dir().join(format!("{id}.json")).exists());
    }

    #[test]
    fn reconcile_inflight_skips_terminal_does_not_requeue() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        // A terminal entry (crashed mid-delivery) ...
        let terminal = "20260527T120000Z-001";
        fs::write(store.inflight_dir().join(format!("{terminal}.mp4")), b"a").unwrap();
        let mut t = ClipQueueEntry::new(terminal, instant("2026-05-27T12:00:00Z"), Utc::now(), 1);
        t.outcome = Some(Outcome::Delivered);
        write_sidecar_atomic(&store.inflight_dir().join(format!("{terminal}.json")), &t).unwrap();

        // ... alongside a genuinely-interrupted (non-terminal) entry.
        let pending = "20260527T120100Z-001";
        fs::write(store.inflight_dir().join(format!("{pending}.mp4")), b"b").unwrap();
        let p = ClipQueueEntry::new(pending, instant("2026-05-27T12:01:00Z"), Utc::now(), 1);
        write_sidecar_atomic(&store.inflight_dir().join(format!("{pending}.json")), &p).unwrap();

        let recovered = store.reconcile_inflight().expect("reconcile");

        let recovered_ids: Vec<&str> = recovered.iter().map(|e| e.clip_id.as_str()).collect();
        assert_eq!(recovered_ids, vec![pending], "only the non-terminal entry is re-queued");
        assert!(store.pending_dir().join(format!("{pending}.json")).is_file());
        assert!(!store.pending_dir().join(format!("{terminal}.json")).exists());
        assert!(store.delivered_dir().join(format!("{terminal}.json")).is_file());
    }

    // ---- PS-02: a corrupt sidecar must not wedge the pick loop ----

    #[test]
    fn pick_oldest_pending_skips_corrupt_sidecar() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        // Older, corrupt sidecar (sorts first) ...
        let bad = "20260527T120000Z-001";
        fs::write(store.pending_dir().join(format!("{bad}.json")), b"{ not json").unwrap();
        // ... and a newer, valid one.
        let good = "20260527T120100Z-001";
        fs::write(store.pending_dir().join(format!("{good}.mp4")), b"x").unwrap();
        let entry = ClipQueueEntry::new(good, instant("2026-05-27T12:01:00Z"), Utc::now(), 1);
        write_sidecar_atomic(&store.pending_dir().join(format!("{good}.json")), &entry).unwrap();

        let picked = store.pick_oldest_pending(Utc::now()).expect("pick").expect("some");
        assert_eq!(picked.clip_id, good, "corrupt head must be skipped, not wedge the scan");

        // The corrupt sidecar is quarantined out of pending/.
        assert!(
            store.corrupt_dir().join(format!("{bad}.json")).is_file(),
            "corrupt sidecar moved to corrupt/",
        );
        assert!(!store.pending_dir().join(format!("{bad}.json")).exists());
    }
}
