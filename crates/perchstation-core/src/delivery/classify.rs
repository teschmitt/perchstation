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
use tokio::time::sleep;

use crate::hw_traits::Clock;
use crate::observability::tracing as obs_tracing;
use crate::perchpub::client::{ClientError, PerchpubClient};
use crate::perchpub::types::{ClassifyTaskStatus, ObservationPublic};
use crate::queue::store::QueueStore;
use crate::queue::{ClipQueueEntry, QueueError};

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
    #[error("could not parse delivered/ entry `{path}`: {source}")]
    DeliveredParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
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

    /// Run until cancelled. Alternates a fast tick while non-terminal
    /// entries are pending and a slower tick when the queue is idle.
    pub async fn run(self) {
        loop {
            let processed = match self.poll_round().await {
                Ok(n) => n,
                Err(err) => {
                    tracing::warn!(
                        message = %err,
                        "classify poller iteration failed",
                    );
                    0
                }
            };
            if processed > 0 {
                sleep(ACTIVE_SCAN_TICK).await;
            } else {
                sleep(IDLE_SCAN_TICK).await;
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
            let bytes = fs::read(&path)
                .map_err(|source| PollerError::DeliveredIo { path: path.clone(), source })?;
            let entry: ClipQueueEntry = serde_json::from_slice(&bytes)
                .map_err(|source| PollerError::DeliveredParse { path: path.clone(), source })?;
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
