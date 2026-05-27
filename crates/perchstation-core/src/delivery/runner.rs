//! The long-running delivery loop.
//!
//! MVP scope (US1, T036):
//!
//! 1. Pick the oldest `pending/` entry whose `next_attempt_after` has elapsed.
//! 2. Transition it to `inflight/` (bumps `attempts`, stamps timestamps).
//! 3. Emit `delivery.attempt_started`.
//! 4. Stream the clip via [`PerchpubClient::upload_clip`].
//! 5. On 200 — populate `outcome`, `classify_task_id`, `delivered_at`,
//!    `last_classify_status` on the entry and call
//!    [`QueueStore::transition_delivered`], which unlinks the mp4 before
//!    renaming the sidecar into `delivered/`.
//! 6. Emit `delivery.upload_succeeded`.
//!
//! Error classification (transient vs terminal vs attempts-exhausted) is
//! deferred to US2 T046 / T051. In MVP, any failure is logged and the
//! loop backs off briefly — the entry stays in `inflight/` until US2's
//! reconciliation lands.

use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::time::sleep;

use crate::hw_traits::Clock;
use crate::observability::tracing as obs_tracing;
use crate::perchpub::client::{ClientError, PerchpubClient};
use crate::queue::store::QueueStore;
use crate::queue::{Outcome, QueueError};

/// MVP idle tick. Short enough that the classify poller observes the
/// `delivered/` sidecar promptly; long enough not to hot-loop. US2 will
/// replace this with a real backoff schedule (T045).
const IDLE_TICK: Duration = Duration::from_millis(50);

#[derive(Debug, Error)]
enum RunnerError {
    #[error(transparent)]
    Queue(#[from] QueueError),
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// Owns the long-running delivery loop. Cloneable inputs are shared with
/// the [`crate::delivery::classify::ClassifyPoller`] so both share the same
/// mTLS client, on-disk view, and clock source.
pub struct DeliveryRunner {
    store: QueueStore,
    client: PerchpubClient,
    clock: Arc<dyn Clock>,
}

impl DeliveryRunner {
    #[must_use]
    pub fn new(store: QueueStore, client: PerchpubClient, clock: Arc<dyn Clock>) -> Self {
        Self { store, client, clock }
    }

    /// Run until cancelled. Each iteration either delivers exactly one
    /// clip or sleeps for [`IDLE_TICK`] and tries again.
    pub async fn run(self) {
        loop {
            match self.try_once().await {
                Ok(true) => {
                    // Delivered a clip; loop immediately to pick the next.
                }
                Ok(false) => sleep(IDLE_TICK).await,
                Err(err) => {
                    tracing::warn!(
                        message = %err,
                        "delivery iteration failed; MVP has no retry yet (T046)",
                    );
                    sleep(IDLE_TICK).await;
                }
            }
        }
    }

    async fn try_once(&self) -> Result<bool, RunnerError> {
        let now = self.clock.now();
        let Some(entry) = self.store.pick_oldest_pending(now)? else {
            return Ok(false);
        };

        let entry = self.store.transition_inflight(entry, now)?;
        let clip_id = entry.clip_id.clone();
        let attempt = entry.attempts;

        tracing::info!(
            event = obs_tracing::events::DELIVERY_ATTEMPT_STARTED,
            clip_id = %clip_id,
            attempt = attempt,
            "delivery attempt started",
        );

        let mp4_path = self.store.inflight_dir().join(format!("{clip_id}.mp4"));
        let started = Instant::now();
        let task = self.client.upload_clip(&mp4_path, &clip_id).await?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let mut delivered = entry;
        delivered.outcome = Some(Outcome::Delivered);
        delivered.classify_task_id = Some(task.id);
        delivered.delivered_at = Some(self.clock.now());
        delivered.last_classify_status = Some(task.status);

        self.store.transition_delivered(&delivered)?;

        tracing::info!(
            event = obs_tracing::events::DELIVERY_UPLOAD_SUCCEEDED,
            clip_id = %clip_id,
            classify_task_id = %task.id,
            attempt = attempt,
            duration_ms = duration_ms,
            "upload succeeded",
        );

        Ok(true)
    }
}
