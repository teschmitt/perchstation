//! Classify-task poller (T037 + T052).
//!
//! Scans `<data_dir>/queue/delivered/` for entries whose
//! `last_classify_status` is non-terminal and whose `classify_lost_at`
//! is unset, calls [`PerchpubClient::get_classify_task`] for each, and
//! updates the sidecar with the latest status.
//!
//! Per `contracts/perchpub-api.md` §3, polling errors are classified:
//!
//! - Transient (5xx / network / decode) → emit a warning, leave the
//!   sidecar untouched, let the next tick try again.
//! - Terminal (404 / 422 / other 4xx) → emit `classify.lost`, stamp
//!   `classify_lost_at` on the sidecar so subsequent ticks skip the
//!   entry, but leave `outcome: Delivered` intact (the upload itself
//!   succeeded).

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::hw_traits::Clock;
use crate::observability::tracing as obs_tracing;
use crate::perchpub::client::{ClientError, PerchpubClient};
use crate::perchpub::types::{ClassifyTaskStatus, ObservationPublic};
use crate::queue::policy::prune_delivered;
use crate::queue::store::{QueueStore, read_sidecar};
use crate::queue::{ClipQueueEntry, QueueError};

use super::cancellable_sleep;
use super::retry::{FailureKind, classify_poll_error, error_kind};

const IDLE_SCAN_TICK: Duration = Duration::from_millis(50);
const ACTIVE_SCAN_TICK: Duration = Duration::from_millis(5);

/// Default retention (hours) before a finished `delivered/` sidecar is
/// pruned — one week (PS-25). Overridable via `[queue] delivered_retention_hours`.
const DEFAULT_DELIVERED_RETENTION_HOURS: i64 = 24 * 7;

/// How often the loop runs a `delivered/` prune sweep. Coarse so the sweep
/// cost is negligible against the millisecond poll tick.
const PRUNE_INTERVAL: Duration = Duration::from_mins(5);

/// Wall-clock budget (hours, anchored on `delivered_at`) for polling a
/// single classify task before giving up (PS-06). A non-terminal status
/// that never resolves — e.g. an unmodelled `Unknown` perchpub keeps
/// returning — would otherwise be re-polled every tick until the retention
/// prune removes it days later. Past the budget the entry is marked
/// `classify_lost_at`; the upload itself stays `Delivered`.
const CLASSIFY_POLL_MAX_HOURS: i64 = 24;

#[derive(Debug, Error)]
enum PollerError {
    #[error(transparent)]
    Queue(#[from] QueueError),
    #[error("could not read delivered/ entry `{path}`: {source}")]
    DeliveredIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Owner of the classify-task polling loop. The [`Clock`] is consulted
/// when stamping `classify_lost_at`.
pub struct ClassifyPoller {
    store: QueueStore,
    client: PerchpubClient,
    clock: Arc<dyn Clock>,
    /// clip-ids known to have reached a terminal/lost classify state this
    /// process. Such entries can never become pollable again, so later
    /// scans skip them without re-reading + re-parsing the sidecar (PS-25).
    seen_terminal: HashSet<String>,
    /// Retention (hours) before a finished `delivered/` sidecar is pruned.
    delivered_retention_hours: i64,
}

impl ClassifyPoller {
    #[must_use]
    pub fn new(store: QueueStore, client: PerchpubClient, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            client,
            clock,
            seen_terminal: HashSet::new(),
            delivered_retention_hours: DEFAULT_DELIVERED_RETENTION_HOURS,
        }
    }

    /// Override the retention window (hours) before a finished `delivered/`
    /// sidecar is pruned. Wired from `[queue] delivered_retention_hours`.
    #[must_use]
    pub fn with_delivered_retention_hours(mut self, hours: u64) -> Self {
        self.delivered_retention_hours = i64::try_from(hours).unwrap_or(i64::MAX);
        self
    }

    /// Run until the `shutdown` token fires. Alternates a fast tick while
    /// non-terminal entries are pending and a slower tick when the queue is
    /// idle. Every `.await` cooperates with cancellation so a SIGTERM-driven
    /// shutdown stops the loop promptly (PS-04 / PS-09).
    pub async fn run(mut self, shutdown: CancellationToken) {
        // Prune `delivered/` on entry and then on a coarse cadence so the
        // per-tick scan cost stays bounded without re-scanning the whole
        // directory every few milliseconds (PS-25).
        let mut next_prune = Instant::now();
        loop {
            if shutdown.is_cancelled() {
                break;
            }

            if Instant::now() >= next_prune {
                self.prune_round();
                next_prune = Instant::now() + PRUNE_INTERVAL;
            }

            let processed = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                result = self.poll_round() => match result {
                    Ok(n) => n,
                    Err(err) => {
                        tracing::warn!(
                            message = %err,
                            "classify poller iteration failed",
                        );
                        0
                    }
                },
            };
            let tick = if processed > 0 { ACTIVE_SCAN_TICK } else { IDLE_SCAN_TICK };
            if cancellable_sleep(&shutdown, tick).await {
                break;
            }
        }
    }

    /// Age out finished `delivered/` sidecars older than the retention
    /// window, dropping any pruned clip-ids from the in-memory skip set so
    /// it never outgrows the directory it shadows.
    fn prune_round(&mut self) {
        let retention =
            chrono::Duration::seconds(self.delivered_retention_hours.saturating_mul(3600));
        let cutoff = self.clock.now() - retention;
        match prune_delivered(&self.store, cutoff) {
            Ok(pruned) => {
                for id in &pruned {
                    self.seen_terminal.remove(id);
                }
            }
            Err(err) => {
                tracing::warn!(message = %err, "classify poller delivered-prune round failed");
            }
        }
    }

    async fn poll_round(&mut self) -> Result<usize, PollerError> {
        let entries = self.scan_non_terminal()?;
        let mut count = 0;
        for entry in entries {
            self.poll_one(entry).await?;
            count += 1;
        }
        Ok(count)
    }

    fn scan_non_terminal(&mut self) -> Result<Vec<ClipQueueEntry>, PollerError> {
        let delivered = self.store.delivered_dir();
        let read_dir = fs::read_dir(&delivered)
            .map_err(|source| PollerError::DeliveredIo { path: delivered.clone(), source })?;
        let mut entries = Vec::new();
        for dir_entry in read_dir {
            let dir_entry = dir_entry
                .map_err(|source| PollerError::DeliveredIo { path: delivered.clone(), source })?;
            let path = dir_entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            // PS-25: a sidecar already known terminal/lost this process can
            // never become pollable again — skip it without re-reading and
            // re-parsing. The skip set is keyed by clip-id (the file stem).
            if path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| self.seen_terminal.contains(stem))
            {
                continue;
            }
            let mut entry = match read_sidecar(&path) {
                Ok(entry) => entry,
                Err(QueueError::Deserialise { source, .. }) => {
                    // PS-02: a single corrupt sidecar must not wedge the
                    // classify poller forever. Quarantine it so the scan
                    // head advances permanently.
                    tracing::warn!(
                        event = obs_tracing::events::QUEUE_CORRUPT_SIDECAR,
                        path = %path.display(),
                        error = %source,
                        "quarantining corrupt delivered sidecar",
                    );
                    if let Err(qerr) = self.store.quarantine_corrupt(&path) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %qerr,
                            "failed to quarantine corrupt delivered sidecar",
                        );
                    }
                    continue;
                }
                Err(err) => {
                    // Per-file I/O error (e.g. a concurrent prune removed it).
                    // Skip; do not abort the whole scan.
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "skipping unreadable delivered sidecar",
                    );
                    continue;
                }
            };
            // Not pollable (no classify task / lost / terminal status) →
            // remember it so later scans skip it without re-reading, and do
            // not poll it now (PS-25).
            if !is_pollable(&entry) {
                self.seen_terminal.insert(entry.clip_id.clone());
                continue;
            }
            // PS-06: a non-terminal classify task that has outlived the poll
            // budget is given up on — mark it lost (the upload stays
            // Delivered) and stop re-polling it every tick.
            if self.classify_budget_exhausted(&entry) {
                entry.classify_lost_at = Some(self.clock.now());
                self.store.update_delivered_sidecar(&entry)?;
                self.seen_terminal.insert(entry.clip_id.clone());
                if let Some(task_id) = entry.classify_task_id {
                    emit_classify_lost(&entry.clip_id, task_id, "poll_timeout", None);
                }
                continue;
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    /// `true` once a still-non-terminal entry has been pollable for longer
    /// than [`CLASSIFY_POLL_MAX_HOURS`] since `delivered_at` (PS-06). Entries
    /// with no `delivered_at` (never the production path) are never timed out.
    fn classify_budget_exhausted(&self, entry: &ClipQueueEntry) -> bool {
        let Some(delivered_at) = entry.delivered_at else {
            return false;
        };
        self.clock.now() - delivered_at >= chrono::Duration::hours(CLASSIFY_POLL_MAX_HOURS)
    }

    async fn poll_one(&mut self, mut entry: ClipQueueEntry) -> Result<(), PollerError> {
        let task_id = entry
            .classify_task_id
            .expect("scan_non_terminal filters out entries without classify_task_id");
        match self.client.get_classify_task(task_id).await {
            Ok(task) => {
                entry.last_classify_status = Some(task.status);
                self.store.update_delivered_sidecar(&entry)?;

                if task.status.is_terminal() {
                    // No further transitions possible — never re-read it (PS-25).
                    self.seen_terminal.insert(entry.clip_id.clone());
                    let observation_id =
                        task.observation.as_ref().map(|o: &ObservationPublic| o.id);
                    tracing::info!(
                        event = obs_tracing::events::CLASSIFY_TERMINAL,
                        clip_id = %entry.clip_id,
                        classify_task_id = %task_id,
                        status = ?task.status,
                        observation_id = ?observation_id,
                        "classify task reached terminal status",
                    );
                } else {
                    tracing::info!(
                        event = obs_tracing::events::CLASSIFY_POLLED,
                        clip_id = %entry.clip_id,
                        classify_task_id = %task_id,
                        status = ?task.status,
                        "classify task polled (non-terminal)",
                    );
                }
                Ok(())
            }
            Err(err) => {
                let kind = error_kind(&err);
                let status = match &err {
                    ClientError::Http { status, .. } => Some(*status),
                    _ => None,
                };
                match classify_poll_error(&err) {
                    FailureKind::Transient => {
                        // Leave the sidecar alone; the next tick will retry.
                        emit_poll_transient(
                            &entry.clip_id,
                            task_id,
                            kind,
                            status,
                            &err.to_string(),
                        );
                        Ok(())
                    }
                    FailureKind::Terminal => {
                        entry.classify_lost_at = Some(self.clock.now());
                        self.store.update_delivered_sidecar(&entry)?;
                        // Polling stops here — never re-read it (PS-25).
                        self.seen_terminal.insert(entry.clip_id.clone());
                        emit_classify_lost(&entry.clip_id, task_id, kind, status);
                        Ok(())
                    }
                }
            }
        }
    }
}

/// `true` while a `delivered/` entry still needs polling: it has a classify
/// task, has not been marked lost, and its last observed status is
/// non-terminal.
fn is_pollable(entry: &ClipQueueEntry) -> bool {
    entry.classify_task_id.is_some()
        && entry.classify_lost_at.is_none()
        && !entry.last_classify_status.is_some_and(ClassifyTaskStatus::is_terminal)
}

fn emit_poll_transient(
    clip_id: &str,
    task_id: uuid::Uuid,
    kind: &str,
    status: Option<u16>,
    message: &str,
) {
    if let Some(s) = status {
        tracing::warn!(
            clip_id = %clip_id,
            classify_task_id = %task_id,
            kind = kind,
            status = s,
            message = message,
            "classify poll transient failure; will retry",
        );
    } else {
        tracing::warn!(
            clip_id = %clip_id,
            classify_task_id = %task_id,
            kind = kind,
            message = message,
            "classify poll transient failure; will retry",
        );
    }
}

fn emit_classify_lost(clip_id: &str, task_id: uuid::Uuid, kind: &str, status: Option<u16>) {
    if let Some(s) = status {
        tracing::error!(
            event = obs_tracing::events::CLASSIFY_LOST,
            clip_id = %clip_id,
            classify_task_id = %task_id,
            kind = kind,
            status = s,
            "classify task lost; stopped polling",
        );
    } else {
        tracing::error!(
            event = obs_tracing::events::CLASSIFY_LOST,
            clip_id = %clip_id,
            classify_task_id = %task_id,
            kind = kind,
            "classify task lost; stopped polling",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::test_support::{arc_clock, fake_client};
    use crate::queue::store::QueueStore;
    use crate::queue::{ClipQueueEntry, Outcome};
    use chrono::{DateTime, Utc};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn instant(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn write_delivered(store: &QueueStore, entry: &ClipQueueEntry) {
        let path = store.delivered_dir().join(format!("{}.json", entry.clip_id));
        std::fs::write(path, serde_json::to_vec_pretty(entry).unwrap()).unwrap();
    }

    fn poller(dir: &std::path::Path, store: QueueStore) -> ClassifyPoller {
        ClassifyPoller::new(store, fake_client(dir), arc_clock())
    }

    #[test]
    fn scan_skips_corrupt_sidecar_and_continues() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        // A corrupt delivered sidecar ...
        let bad = "20260527T120000Z-001";
        std::fs::write(store.delivered_dir().join(format!("{bad}.json")), b"{ not json").unwrap();

        // ... and a still-pollable (non-terminal) one.
        let good = "20260527T120100Z-001";
        let mut e = ClipQueueEntry::new(good, instant("2026-05-27T12:01:00Z"), Utc::now(), 1);
        e.outcome = Some(Outcome::Delivered);
        e.classify_task_id = Some(Uuid::new_v4());
        e.last_classify_status = Some(ClassifyTaskStatus::Processing);
        write_delivered(&store, &e);

        let mut poller = poller(dir.path(), store.clone());
        let entries = poller.scan_non_terminal().expect("scan must not error on corrupt sidecar");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].clip_id, good);
        assert!(
            store.corrupt_dir().join(format!("{bad}.json")).is_file(),
            "corrupt sidecar quarantined to corrupt/",
        );
        assert!(!store.delivered_dir().join(format!("{bad}.json")).exists());
    }

    #[test]
    fn scan_skips_already_terminal_without_rereading() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        // A terminal (Success) delivered entry — never pollable again.
        let id = "20260527T120000Z-001";
        let mut e = ClipQueueEntry::new(id, instant("2026-05-27T12:00:00Z"), Utc::now(), 1);
        e.outcome = Some(Outcome::Delivered);
        e.classify_task_id = Some(Uuid::new_v4());
        e.last_classify_status = Some(ClassifyTaskStatus::Success);
        write_delivered(&store, &e);

        let mut poller = poller(dir.path(), store.clone());

        // First scan reads it, sees it is terminal, and remembers it.
        assert!(poller.scan_non_terminal().expect("scan 1").is_empty());

        // Corrupt the sidecar on disk. If the next scan RE-READ it, the
        // corrupt JSON would trip the quarantine path and move it to
        // corrupt/. A skip-without-reading leaves it untouched (PS-25).
        std::fs::write(store.delivered_dir().join(format!("{id}.json")), b"{ now corrupt").unwrap();

        assert!(poller.scan_non_terminal().expect("scan 2").is_empty());
        assert!(
            store.delivered_dir().join(format!("{id}.json")).is_file(),
            "a known-terminal sidecar must not be re-read on later ticks",
        );
        assert!(
            !store.corrupt_dir().join(format!("{id}.json")).exists(),
            "skipped terminal sidecar must not be quarantined",
        );
    }

    #[test]
    fn scan_stops_polling_after_budget_and_marks_lost() {
        // PS-06: a non-terminal classify status (e.g. an unmodelled
        // `Unknown`) that never resolves must not be polled forever. Past
        // the poll budget (anchored on delivered_at) the entry is marked
        // lost and dropped from the pollable set. The poller's clock is
        // fixed at 2026-05-27T12:00:00Z.
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        let id = "20260526T100000Z-001";
        let mut e = ClipQueueEntry::new(id, instant("2026-05-26T10:00:00Z"), Utc::now(), 1);
        e.outcome = Some(Outcome::Delivered);
        e.classify_task_id = Some(Uuid::new_v4());
        e.last_classify_status = Some(ClassifyTaskStatus::Unknown);
        e.delivered_at = Some(instant("2026-05-26T10:00:00Z")); // >24h before clock
        write_delivered(&store, &e);

        let mut poller = poller(dir.path(), store.clone());
        let entries = poller.scan_non_terminal().expect("scan");
        assert!(entries.is_empty(), "a budget-exhausted entry must not be polled");

        let reread: ClipQueueEntry = serde_json::from_slice(
            &std::fs::read(store.delivered_dir().join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert!(reread.classify_lost_at.is_some(), "budget exhaustion stamps classify_lost_at");
    }

    #[test]
    fn scan_keeps_polling_within_budget() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        let id = "20260527T115900Z-001";
        let mut e = ClipQueueEntry::new(id, instant("2026-05-27T11:59:00Z"), Utc::now(), 1);
        e.outcome = Some(Outcome::Delivered);
        e.classify_task_id = Some(Uuid::new_v4());
        e.last_classify_status = Some(ClassifyTaskStatus::Processing);
        e.delivered_at = Some(instant("2026-05-27T11:59:00Z")); // 1 min before clock
        write_delivered(&store, &e);

        let mut poller = poller(dir.path(), store);
        let entries = poller.scan_non_terminal().expect("scan");
        assert_eq!(entries.len(), 1, "an in-budget entry is still polled");
    }

    #[tokio::test]
    async fn poll_round_advances_despite_corrupt() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        // Only a corrupt sidecar — no pollable entry, so poll_round makes no
        // network call but must still complete (Ok) rather than erroring
        // every tick.
        std::fs::write(store.delivered_dir().join("20260527T120000Z-001.json"), b"{ bad").unwrap();

        let mut poller = poller(dir.path(), store.clone());
        let n = poller.poll_round().await.expect("poll_round must advance past corrupt sidecar");
        assert_eq!(n, 0);
        assert!(store.corrupt_dir().join("20260527T120000Z-001.json").is_file());
    }
}
