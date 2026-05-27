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

use super::{ClipQueueEntry, QueueError};

const QUEUE_DIR: &str = "queue";
const PENDING: &str = "pending";
const INFLIGHT: &str = "inflight";
const DELIVERED: &str = "delivered";

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
        for sub in [PENDING, INFLIGHT, DELIVERED] {
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

    /// Move a freshly-captured clip into `pending/`. Stages the mp4 via a
    /// `.tmp` suffix then renames; writes the sidecar via tmp + rename.
    pub fn enqueue(
        &self,
        clip_source: &Path,
        meta: ClipMeta,
    ) -> Result<ClipQueueEntry, QueueError> {
        let clip_id = next_clip_id(meta.captured_at);
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

        let entry = ClipQueueEntry::new(clip_id.clone(), meta.captured_at, Utc::now(), byte_size);
        let sidecar_target = pending.join(format!("{clip_id}.json"));
        write_sidecar_atomic(&sidecar_target, &entry)?;

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
            let entry = read_sidecar(&path)?;
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

        write_sidecar_atomic(&inflight_sidecar, &updated)?;

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
    fs::write(&tmp, &bytes).map_err(|source| QueueError::Io { path: tmp.clone(), source })?;
    fs::rename(&tmp, target)
        .map_err(|source| QueueError::Io { path: target.to_path_buf(), source })?;
    Ok(())
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
}
