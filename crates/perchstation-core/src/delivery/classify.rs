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

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::hw_traits::Clock;
use crate::observability::tracing as obs_tracing;
use crate::perchpub::client::{ClientError, PerchpubClient};
use crate::perchpub::types::{ClassifyTaskStatus, ObservationPublic};
use crate::queue::store::QueueStore;
use crate::queue::{ClipQueueEntry, QueueError};

use super::cancellable_sleep;
use super::retry::{FailureKind, classify_poll_error, error_kind};

const IDLE_SCAN_TICK: Duration = Duration::from_millis(50);
const ACTIVE_SCAN_TICK: Duration = Duration::from_millis(5);

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
}

impl ClassifyPoller {
    #[must_use]
    pub fn new(store: QueueStore, client: PerchpubClient, clock: Arc<dyn Clock>) -> Self {
        Self { store, client, clock }
    }

    /// Run until the `shutdown` token fires. Alternates a fast tick while
    /// non-terminal entries are pending and a slower tick when the queue is
    /// idle. Every `.await` cooperates with cancellation so a SIGTERM-driven
    /// shutdown stops the loop promptly (PS-04 / PS-09).
    pub async fn run(self, shutdown: CancellationToken) {
        loop {
            if shutdown.is_cancelled() {
                break;
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

    async fn poll_round(&self) -> Result<usize, PollerError> {
        let entries = self.scan_non_terminal()?;
        let mut count = 0;
        for entry in entries {
            self.poll_one(entry).await?;
            count += 1;
        }
        Ok(count)
    }

    fn scan_non_terminal(&self) -> Result<Vec<ClipQueueEntry>, PollerError> {
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
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
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
            let entry: ClipQueueEntry = match serde_json::from_slice(&bytes) {
                Ok(entry) => entry,
                Err(err) => {
                    // PS-02: a single corrupt sidecar must not wedge the
                    // classify poller forever. Quarantine it so the scan
                    // head advances permanently.
                    tracing::warn!(
                        event = obs_tracing::events::QUEUE_CORRUPT_SIDECAR,
                        path = %path.display(),
                        error = %err,
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
            };
            if entry.classify_task_id.is_none() {
                continue;
            }
            if entry.classify_lost_at.is_some() {
                continue;
            }
            if entry.last_classify_status.is_some_and(ClassifyTaskStatus::is_terminal) {
                continue;
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    async fn poll_one(&self, mut entry: ClipQueueEntry) -> Result<(), PollerError> {
        let task_id = entry
            .classify_task_id
            .expect("scan_non_terminal filters out entries without classify_task_id");
        match self.client.get_classify_task(task_id).await {
            Ok(task) => {
                entry.last_classify_status = Some(task.status);
                self.store.update_delivered_sidecar(&entry)?;

                if task.status.is_terminal() {
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
                        emit_classify_lost(&entry.clip_id, task_id, kind, status);
                        Ok(())
                    }
                }
            }
        }
    }
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

        let poller = poller(dir.path(), store.clone());
        let entries = poller.scan_non_terminal().expect("scan must not error on corrupt sidecar");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].clip_id, good);
        assert!(
            store.corrupt_dir().join(format!("{bad}.json")).is_file(),
            "corrupt sidecar quarantined to corrupt/",
        );
        assert!(!store.delivered_dir().join(format!("{bad}.json")).exists());
    }

    #[tokio::test]
    async fn poll_round_advances_despite_corrupt() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        // Only a corrupt sidecar — no pollable entry, so poll_round makes no
        // network call but must still complete (Ok) rather than erroring
        // every tick.
        std::fs::write(store.delivered_dir().join("20260527T120000Z-001.json"), b"{ bad").unwrap();

        let poller = poller(dir.path(), store.clone());
        let n = poller.poll_round().await.expect("poll_round must advance past corrupt sidecar");
        assert_eq!(n, 0);
        assert!(store.corrupt_dir().join("20260527T120000Z-001.json").is_file());
    }
}
