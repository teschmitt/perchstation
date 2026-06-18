//! Delivery subsystem: the long-running upload loop and the classify-task
//! poller.
//!
//! Layout follows `specs/001-clip-delivery/plan.md` §Project Structure:
//!
//! - [`runner`] — picks the oldest `pending/` clip, uploads it via the
//!   mTLS client, and transitions the entry into `delivered/`.
//! - [`classify`] — scans `delivered/` and polls perchpub for the
//!   post-upload classify-task status.
//!
//! Retry policy (US2 T045) and full error classification (US2 T046, T052)
//! layer on top of the happy-path loops here.

pub mod classify;
pub mod retry;
pub mod runner;

#[cfg(test)]
pub(crate) mod test_support;

use std::time::Duration;

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// Sleep for `dur` unless `shutdown` fires first. Returns `true` when
/// cancellation was observed (the caller should break its loop). Shared by
/// the delivery runner and the classify poller so both cooperate with a
/// SIGTERM-driven shutdown at every `.await` rather than relying on a
/// detaching `abort()` (PS-04 / PS-09).
pub(crate) async fn cancellable_sleep(shutdown: &CancellationToken, dur: Duration) -> bool {
    tokio::select! {
        () = shutdown.cancelled() => true,
        () = sleep(dur) => false,
    }
}
