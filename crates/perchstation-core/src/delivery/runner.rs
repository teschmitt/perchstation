//! The long-running delivery loop.
//!
//! Layout:
//!
//! 1. Pick the oldest `pending/` entry whose `next_attempt_after` has elapsed.
//! 2. Transition it to `inflight/` (bumps `attempts`, stamps timestamps).
//! 3. Emit `delivery.attempt_started`.
//! 4. Stream the clip via [`PerchpubClient::upload_clip`].
//! 5. Branch on the result:
//!    - 200 → mark `Delivered`, transition to `delivered/`, emit
//!      `delivery.upload_succeeded`.
//!    - transient HTTP / network / decode error → compute the next
//!      attempt via [`BackoffSchedule`], stamp `next_attempt_after` +
//!      `last_error` on the sidecar, move the entry back to `pending/`,
//!      emit `delivery.upload_transient`. If the schedule says
//!      `Exhausted`, fall through to the terminal branch with
//!      `kind = "attempts_exhausted"`.
//!    - terminal HTTP / config error → stamp `outcome = Undeliverable` +
//!      `last_error`, transition to `delivered/`, emit
//!      `delivery.upload_terminal`.
//!
//! Pre-flight readability check (T049) runs before the upload attempt;
//! disk-full handling (T049a) is layered on the queue writes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::time::sleep;

use crate::hw_traits::Clock;
use crate::observability::tracing as obs_tracing;
use crate::perchpub::client::{ClientError, PerchpubClient};
use crate::queue::store::QueueStore;
use crate::queue::{ClipQueueEntry, LastError, Outcome, QueueError};

use super::retry::{BackoffSchedule, FailureKind, NextAction, classify_upload_error, error_kind};

/// Idle tick: how long the loop sleeps when there's nothing eligible to
/// pick up. Keeps us responsive to backoff expiry without hot-looping.
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
    schedule: BackoffSchedule,
}

impl DeliveryRunner {
    #[must_use]
    pub fn new(
        store: QueueStore,
        client: PerchpubClient,
        clock: Arc<dyn Clock>,
        schedule: BackoffSchedule,
    ) -> Self {
        Self { store, client, clock, schedule }
    }

    /// Run until cancelled. Each iteration either makes progress on one
    /// clip or sleeps for [`IDLE_TICK`] and tries again.
    pub async fn run(self) {
        // Backoff used when the filesystem reports ENOSPC. Capped at
        // `max_attempt_delay` so we wake periodically and retry once
        // space frees up (T049a).
        let disk_full_backoff = self.schedule.max_attempt_delay;
        loop {
            match self.try_once().await {
                Ok(true) => {}
                Ok(false) => sleep(IDLE_TICK).await,
                Err(RunnerError::Queue(QueueError::DiskFull { path })) => {
                    tracing::error!(
                        event = obs_tracing::events::QUEUE_DISK_FULL,
                        path = %path.display(),
                        "queue write failed with ENOSPC; backing off",
                    );
                    sleep(disk_full_backoff).await;
                }
                Err(err) => {
                    tracing::warn!(
                        message = %err,
                        "delivery iteration aborted on internal queue error",
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

        // Pre-flight: zero-length or unreadable → emit warning and mark
        // the entry undeliverable without sending bytes (FR-013, T049).
        if let Some(reason) = preflight_check(&mp4_path) {
            tracing::warn!(
                event = obs_tracing::events::QUEUE_ZERO_LENGTH_SKIPPED,
                clip_id = %clip_id,
                kind = reason,
                "clip failed pre-flight readability check",
            );
            let mut undeliverable = entry;
            undeliverable.outcome = Some(Outcome::Undeliverable);
            undeliverable.delivered_at = Some(self.clock.now());
            undeliverable.last_error = Some(LastError {
                kind: reason.into(),
                status: None,
                message: format!("pre-flight {reason} for {clip_id}"),
            });
            self.store.transition_delivered(&undeliverable)?;
            return Ok(true);
        }

        let started = Instant::now();
        match self.client.upload_clip(&mp4_path, &clip_id).await {
            Ok(task) => {
                let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let mut delivered = entry;
                delivered.outcome = Some(Outcome::Delivered);
                delivered.classify_task_id = Some(task.id);
                delivered.delivered_at = Some(self.clock.now());
                delivered.last_classify_status = Some(task.status);
                delivered.last_error = None;
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
            Err(err) => {
                self.handle_upload_error(entry, &err)?;
                Ok(true)
            }
        }
    }

    fn handle_upload_error(
        &self,
        entry: ClipQueueEntry,
        err: &ClientError,
    ) -> Result<(), RunnerError> {
        let kind = error_kind(err);
        let status = match err {
            ClientError::Http { status, .. } => Some(*status),
            _ => None,
        };
        let retry_after = match err {
            ClientError::Http { retry_after, .. } => *retry_after,
            _ => None,
        };
        let message = err.to_string();
        let clip_id = entry.clip_id.clone();
        let attempt = entry.attempts;
        let first_attempt_at = entry.first_attempt_at;

        let failure = classify_upload_error(err);
        match failure {
            FailureKind::Transient => {
                let next = self.schedule.schedule(
                    self.clock.as_ref(),
                    attempt,
                    first_attempt_at,
                    retry_after,
                );
                match next {
                    NextAction::Retry(next_after) => {
                        let mut requeued = entry;
                        requeued.next_attempt_after = Some(next_after);
                        requeued.last_error =
                            Some(LastError { kind: kind.into(), status, message });
                        self.store.transition_back_to_pending(&requeued)?;
                        emit_transient(&clip_id, attempt, kind, status, next_after);
                        Ok(())
                    }
                    NextAction::Exhausted => {
                        self.mark_attempts_exhausted(entry, kind, status, &message)
                    }
                }
            }
            FailureKind::Terminal => self.mark_terminal(entry, kind, status, &message),
        }
    }

    fn mark_attempts_exhausted(
        &self,
        entry: ClipQueueEntry,
        kind: &str,
        status: Option<u16>,
        message: &str,
    ) -> Result<(), RunnerError> {
        let clip_id = entry.clip_id.clone();
        let attempts = entry.attempts;
        let wallclock_secs = entry
            .first_attempt_at
            .map_or(0, |first| (self.clock.now() - first).num_seconds().max(0));
        let mut undeliverable = entry;
        undeliverable.outcome = Some(Outcome::Undeliverable);
        undeliverable.delivered_at = Some(self.clock.now());
        undeliverable.last_error =
            Some(LastError { kind: kind.into(), status, message: message.to_string() });
        self.store.transition_delivered(&undeliverable)?;
        emit_attempts_exhausted(&clip_id, attempts, wallclock_secs, kind, status, message);
        Ok(())
    }

    fn mark_terminal(
        &self,
        entry: ClipQueueEntry,
        kind: &str,
        status: Option<u16>,
        message: &str,
    ) -> Result<(), RunnerError> {
        let clip_id = entry.clip_id.clone();
        let attempt = entry.attempts;
        let mut undeliverable = entry;
        undeliverable.outcome = Some(Outcome::Undeliverable);
        undeliverable.delivered_at = Some(self.clock.now());
        undeliverable.last_error =
            Some(LastError { kind: kind.into(), status, message: message.to_string() });
        self.store.transition_delivered(&undeliverable)?;
        emit_terminal(&clip_id, attempt, kind, status, message);
        Ok(())
    }
}

/// Emit `delivery.upload_transient` with `status` flattened to a JSON
/// number when present, omitted otherwise. `tracing`'s `?value` syntax
/// debug-prints `Option<u16>` as the string `"Some(422)"`, which breaks
/// downstream tooling that reads `status` as a number — hence the
/// explicit two-branch emission here. Same trick repeats below for the
/// terminal and attempts-exhausted events.
fn emit_transient(
    clip_id: &str,
    attempt: u32,
    kind: &str,
    status: Option<u16>,
    next_after: chrono::DateTime<chrono::Utc>,
) {
    if let Some(s) = status {
        tracing::warn!(
            event = obs_tracing::events::DELIVERY_UPLOAD_TRANSIENT,
            clip_id = %clip_id,
            attempt = attempt,
            kind = kind,
            status = s,
            next_attempt_after = %next_after.to_rfc3339(),
            "upload transient failure; scheduled retry",
        );
    } else {
        tracing::warn!(
            event = obs_tracing::events::DELIVERY_UPLOAD_TRANSIENT,
            clip_id = %clip_id,
            attempt = attempt,
            kind = kind,
            next_attempt_after = %next_after.to_rfc3339(),
            "upload transient failure; scheduled retry",
        );
    }
}

fn emit_terminal(clip_id: &str, attempt: u32, kind: &str, status: Option<u16>, message: &str) {
    if let Some(s) = status {
        tracing::error!(
            event = obs_tracing::events::DELIVERY_UPLOAD_TERMINAL,
            clip_id = %clip_id,
            attempt = attempt,
            kind = kind,
            status = s,
            message = message,
            "upload terminal failure; clip marked undeliverable",
        );
    } else {
        tracing::error!(
            event = obs_tracing::events::DELIVERY_UPLOAD_TERMINAL,
            clip_id = %clip_id,
            attempt = attempt,
            kind = kind,
            message = message,
            "upload terminal failure; clip marked undeliverable",
        );
    }
}

fn emit_attempts_exhausted(
    clip_id: &str,
    attempts: u32,
    wallclock_secs: i64,
    kind: &str,
    status: Option<u16>,
    message: &str,
) {
    if let Some(s) = status {
        tracing::error!(
            event = obs_tracing::events::DELIVERY_ATTEMPTS_EXHAUSTED,
            clip_id = %clip_id,
            attempts = attempts,
            wallclock_secs = wallclock_secs,
            kind = kind,
            status = s,
            message = message,
            "delivery attempts exhausted",
        );
    } else {
        tracing::error!(
            event = obs_tracing::events::DELIVERY_ATTEMPTS_EXHAUSTED,
            clip_id = %clip_id,
            attempts = attempts,
            wallclock_secs = wallclock_secs,
            kind = kind,
            message = message,
            "delivery attempts exhausted",
        );
    }
}

/// Inspect `clip_path` and return `Some(reason)` if the file is missing,
/// unreadable, or zero-length. `None` means the file is OK to upload.
fn preflight_check(clip_path: &std::path::Path) -> Option<&'static str> {
    match std::fs::metadata(clip_path) {
        Ok(meta) if meta.len() == 0 => Some("zero_length"),
        Ok(_) => None,
        Err(_) => Some("unreadable"),
    }
}
