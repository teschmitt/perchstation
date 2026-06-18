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
//!
//! **At-least-once delivery (PS-01).** There is an irreducible window
//! between perchpub *accepting* the bytes (`upload_clip` returns `Ok`) and
//! the station *recording* that success (`transition_delivered`). If the
//! process dies — or `transition_delivered` itself fails (e.g. ENOSPC
//! writing the sidecar) — in that window, the entry stays non-terminal in
//! `inflight/` and boot reconciliation re-queues it, so the clip is
//! re-uploaded. This is by design: the window cannot be closed locally, so
//! perchpub MUST treat `POST /api/v1/upload/` as **idempotent keyed on
//! `clip_id`** (passed as the multipart filename) and de-duplicate
//! re-sends rather than spawn a second classify task. Crashes *after* the
//! terminal sidecar is written but *before* the `delivered/` rename are
//! handled separately — `reconcile_inflight` finishes that interrupted
//! transition instead of re-queueing it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::hw_traits::Clock;
use crate::identity::StationIdentity;
use crate::observability::tracing as obs_tracing;
use crate::perchpub::client::{ClientError, PerchpubClient};
use crate::queue::store::QueueStore;
use crate::queue::{ClipQueueEntry, LastError, Outcome, QueueError};

use super::cancellable_sleep;
use super::retry::{BackoffSchedule, FailureKind, NextAction, classify_upload_error, error_kind};

/// Idle tick: how long the loop sleeps when there's nothing eligible to
/// pick up. Keeps us responsive to backoff expiry without hot-looping.
const IDLE_TICK: Duration = Duration::from_millis(50);

/// How long the loop sleeps between cert-expiry checks once the cert has
/// expired. We do NOT exit the process — `status` should keep reporting
/// `expired` until the operator re-enrolls (FR-014) — but we do stop
/// trying to upload, so this tick is comfortably long.
const CERT_EXPIRED_TICK: Duration = Duration::from_mins(1);

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
    identity: StationIdentity,
    cert_expired_logged: AtomicBool,
}

impl DeliveryRunner {
    #[must_use]
    pub fn new(
        store: QueueStore,
        client: PerchpubClient,
        clock: Arc<dyn Clock>,
        schedule: BackoffSchedule,
        identity: StationIdentity,
    ) -> Self {
        Self {
            store,
            client,
            clock,
            schedule,
            identity,
            cert_expired_logged: AtomicBool::new(false),
        }
    }

    /// Run until the `shutdown` token fires. Each iteration either makes
    /// progress on one clip or sleeps for [`IDLE_TICK`] and tries again.
    /// Every `.await` point cooperates with cancellation so a SIGTERM-driven
    /// shutdown stops the loop promptly (PS-04 / PS-09) — the worker no
    /// longer relies on a detaching `abort()`.
    pub async fn run(self, shutdown: CancellationToken) {
        // Backoff used when the filesystem reports ENOSPC. Capped at
        // `max_attempt_delay` so we wake periodically and retry once
        // space frees up (T049a).
        let disk_full_backoff = self.schedule.max_attempt_delay;
        loop {
            if shutdown.is_cancelled() {
                break;
            }

            // Pre-flight cert expiry check (T058 / FR-014). Halts the
            // loop's productive work without exiting the process so
            // `status` continues to surface the expired state.
            if self.identity.cert_is_expired(self.clock.now()) {
                if !self.cert_expired_logged.swap(true, Ordering::SeqCst) {
                    tracing::error!(
                        event = obs_tracing::events::DELIVERY_CERT_EXPIRED,
                        cert_not_after = %self.identity.cert_not_after.to_rfc3339(),
                        "station cert has expired; halting delivery loop until re-enrollment",
                    );
                }
                if cancellable_sleep(&shutdown, CERT_EXPIRED_TICK).await {
                    break;
                }
                continue;
            }

            let outcome = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                outcome = self.try_once() => outcome,
            };
            match outcome {
                Ok(true) => {}
                Ok(false) => {
                    if cancellable_sleep(&shutdown, IDLE_TICK).await {
                        break;
                    }
                }
                Err(RunnerError::Queue(QueueError::DiskFull { path })) => {
                    tracing::error!(
                        event = obs_tracing::events::QUEUE_DISK_FULL,
                        path = %path.display(),
                        "queue write failed with ENOSPC; backing off",
                    );
                    if cancellable_sleep(&shutdown, disk_full_backoff).await {
                        break;
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        message = %err,
                        "delivery iteration aborted on internal queue error",
                    );
                    if cancellable_sleep(&shutdown, IDLE_TICK).await {
                        break;
                    }
                }
            }
        }
    }

    async fn try_once(&self) -> Result<bool, RunnerError> {
        let now = self.clock.now();
        let Some(entry) = self.store.pick_oldest_pending(now)? else {
            return Ok(false);
        };

        let entry = match self.store.transition_inflight(entry, now) {
            Ok(entry) => entry,
            Err(QueueError::MissingMedia { clip_id }) => {
                // PS-04: the sidecar references media that no longer exists
                // (a residual orphan). Quarantine it so the delivery head
                // advances instead of re-picking the same entry — and
                // wedging the loop — every tick. Counts as progress.
                tracing::warn!(
                    event = obs_tracing::events::QUEUE_MISSING_MEDIA,
                    clip_id = %clip_id,
                    "quarantining orphan sidecar with missing media",
                );
                self.store.quarantine_orphan(&clip_id)?;
                return Ok(true);
            }
            Err(err) => return Err(err.into()),
        };
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
                // PS-01: the bytes are already accepted by perchpub. If this
                // record fails (or we crash before it), the entry stays in
                // `inflight/` and reconcile re-queues it → an at-least-once
                // re-upload, NOT a lost clip. perchpub de-duplicates on
                // `clip_id` (see the module header).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::test_support::{
        arc_clock, fake_client, far_future_identity, fast_schedule,
    };
    use crate::queue::ClipQueueEntry;
    use crate::queue::store::QueueStore;
    use chrono::{DateTime, Utc};
    use std::time::Duration as StdDuration;
    use tempfile::TempDir;

    fn instant(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn runner(dir: &std::path::Path, store: QueueStore) -> DeliveryRunner {
        DeliveryRunner::new(
            store,
            fake_client(dir),
            arc_clock(),
            fast_schedule(),
            far_future_identity(),
        )
    }

    #[tokio::test]
    async fn try_once_quarantines_missing_media_and_advances() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();

        // An orphan: a pending sidecar whose `.mp4` media is absent.
        let id = "20260527T120000Z-001";
        let entry = ClipQueueEntry::new(id, instant("2026-05-27T12:00:00Z"), Utc::now(), 9);
        std::fs::write(
            store.pending_dir().join(format!("{id}.json")),
            serde_json::to_vec_pretty(&entry).unwrap(),
        )
        .unwrap();

        let runner = runner(dir.path(), store.clone());

        let progressed = runner.try_once().await.expect("try_once must not error on MissingMedia");
        assert!(progressed, "quarantining a MissingMedia orphan counts as progress");
        assert!(
            !store.pending_dir().join(format!("{id}.json")).exists(),
            "orphan sidecar must be quarantined",
        );

        // The head advanced: the next pick finds nothing.
        let again = runner.try_once().await.expect("try_once 2");
        assert!(!again, "delivery head must have advanced past the orphan");
    }

    #[tokio::test]
    async fn run_exits_on_cancellation() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap(); // empty queue → idle loop
        let runner = runner(dir.path(), store);

        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(runner.run(shutdown.clone()));

        // Let it spin a couple of idle ticks, then cancel.
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        shutdown.cancel();

        let joined = tokio::time::timeout(StdDuration::from_secs(2), handle).await;
        assert!(joined.is_ok(), "run() must exit promptly after cancellation");
        joined.unwrap().expect("run task join");
    }
}
