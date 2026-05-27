//! Classify-task poller (T037, MVP scope).
//!
//! Scans `<data_dir>/queue/delivered/` for entries whose
//! `last_classify_status` is non-terminal, calls
//! [`PerchpubClient::get_classify_task`] for each, and updates the sidecar
//! with the latest status.
//!
//! ## Cadence (MVP vs production)
//!
//! Production guidance in `contracts/perchpub-api.md` §Polling cadence is
//! "30 s for `Prepared`/`Queued`/`Processing`". The MVP integration test
//! (`tests/integration/delivery_happy.rs`) breaks its wait loop the
//! instant the `delivered/<id>.json` sidecar appears and then SIGKILLs
//! the process — the poller has, in the worst case, ~100 ms to observe
//! the sidecar AND fire both `classify.polled` (non-terminal) and
//! `classify.terminal`. The fake perchpub flips `Prepared` → `Success` on
//! the second poll of a given task-id, so the loop must issue at least
//! two `GET /classify-task/{id}` calls inside that window.
//!
//! Hence the MVP scan cadence is tight ([`ACTIVE_SCAN_TICK`] = 5 ms while
//! there's at least one non-terminal entry, [`IDLE_SCAN_TICK`] = 50 ms
//! otherwise). US2 polish (T052) will introduce a `[classify]
//! poll_interval_secs` knob and default that to 30 s for production.

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

const IDLE_SCAN_TICK: Duration = Duration::from_millis(50);
const ACTIVE_SCAN_TICK: Duration = Duration::from_millis(5);

#[derive(Debug, Error)]
enum PollerError {
    #[error(transparent)]
    Queue(#[from] QueueError),
    #[error(transparent)]
    Client(#[from] ClientError),
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

/// Owner of the classify-task polling loop. The [`Clock`] is held for the
/// US2 ceilings (T052) — MVP polling fires on every scan tick regardless
/// of wall-clock.
pub struct ClassifyPoller {
    store: QueueStore,
    client: PerchpubClient,
    #[allow(dead_code, reason = "Clock is used by US2 T052; MVP loop polls on every tick")]
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
        let task = self.client.get_classify_task(task_id).await?;

        entry.last_classify_status = Some(task.status);
        self.store.update_delivered_sidecar(&entry)?;

        if task.status.is_terminal() {
            let observation_id = task.observation.as_ref().map(|o: &ObservationPublic| o.id);
            tracing::info!(
                event = obs_tracing::events::CLASSIFY_TERMINAL,
                clip_id = %entry.clip_id,
                classify_task_id = %task_id,
                status = ?task.status,
                observation_id = ?observation_id,
                "classify task reached terminal status",
            );
        } else {
            // Contract `log-events.md` lists `classify.polled` at debug;
            // emitted at info here so the MVP integration test observes
            // the event at the default log level. US2 polish (T052)
            // restores the debug level once the production 30 s cadence
            // makes info-level too chatty.
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
}
