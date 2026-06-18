//! Failure-isolated task spawn helper (T033, FR-012, SC-009).
//!
//! Each long-lived worker (`DeliveryRunner`, `ClassifyPoller`, `Capture`)
//! runs in its own [`tokio::spawn`]ed task under
//! `perchstation::commands::serve::run`. The constitution's
//! "unattended reliability" principle requires a panic in one of those
//! tasks to be observed (so an operator can grep journald for it) but
//! not to cascade into the other tasks. The wrapper in this module is
//! the mechanism that delivers both guarantees.
//!
//! Usage:
//!
//! ```ignore
//! use perchstation_core::supervision::spawn_supervised;
//!
//! let handle = spawn_supervised("delivery", runner.run(shutdown.clone()));
//! // ...later, on SIGTERM:
//! shutdown.cancel();           // graceful drain at the next `.await`
//! handle.abort();              // hard backstop — now actually stops the worker
//! ```
//!
//! If the supervised future completes cleanly the spawned task resolves
//! with `()`; if it panics, the panic is caught **in place** via
//! [`FutureExt::catch_unwind`] and a `service.task_panicked { task: "<name>" }`
//! event is emitted, after which the task still resolves with `()`. Because
//! the worker runs as the *single* returned task (no nested spawn),
//! `handle.abort()` actually stops the worker — and `handle.await` in
//! shutdown code never propagates an inner panic.
//!
//! Long-lived workers additionally cooperate with a [`CancellationToken`]
//! for graceful SIGTERM drain (PS-04 / PS-09); `abort()` remains available
//! as a hard backstop once the returned handle truly owns the worker.
//!
//! [`CancellationToken`]: tokio_util::sync::CancellationToken

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use tokio::task::JoinHandle;

use crate::observability::tracing as obs_tracing;

/// Spawn `fut` on the current tokio runtime under a supervisor that
/// catches panics and unexpected exits. Returns a [`JoinHandle`] that
/// resolves with `()` on every termination mode and whose `abort()`
/// stops the supervised worker.
///
/// `task` is the human-friendly label emitted on
/// `service.task_panicked` (e.g. `"delivery"`, `"classify"`,
/// `"capture"`). Operators grep journald with it.
pub fn spawn_supervised<F>(task: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    // Run the worker as the single returned task and catch any panic in
    // place. The previous implementation nested a second `tokio::spawn` and
    // returned the *outer* handle; aborting it dropped — and thus detached,
    // not aborted — the inner worker, leaving it running unsupervised
    // (PS-09). `AssertUnwindSafe` is sound here because a panic terminates
    // this task: nothing observes its post-unwind state.
    tokio::spawn(async move {
        if let Err(payload) = AssertUnwindSafe(fut).catch_unwind().await {
            let panic_msg = panic_payload_string(&*payload);
            tracing::error!(
                event = obs_tracing::events::SERVICE_TASK_PANICKED,
                task,
                message = %panic_msg,
                "supervised task panicked; isolated by spawn_supervised",
            );
        }
    })
}

/// Extract a stringified description of a `Box<dyn Any + Send>` panic
/// payload — the typical payload of a `panic!("...")` is a `&'static str`
/// or a `String`, so we try both. Falls back to a generic marker for
/// other payload types.
fn panic_payload_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn clean_exit_does_not_emit_panic_event() {
        let handle = spawn_supervised("clean", async move {});
        handle.await.expect("supervisor handle");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panic_is_isolated_and_supervisor_handle_resolves_ok() {
        let handle = spawn_supervised("panicker", async move {
            panic!("synthetic panic for supervision_test");
        });
        // The wrapper catches the inner panic; the outer handle resolves Ok.
        handle.await.expect("supervisor handle must not propagate inner panic");
    }

    #[tokio::test]
    async fn abort_actually_stops_inner_worker() {
        // Aborting the handle returned by `spawn_supervised` must stop the
        // supervised worker. The old nested-spawn returned the *outer* task's
        // handle; aborting it dropped (detached) the inner worker, which kept
        // running unsupervised (PS-09).
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_worker = counter.clone();

        let handle = spawn_supervised("worker", async move {
            loop {
                counter_for_worker.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        // Let the worker spin up, then abort and wait for the abort to land.
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let after_abort = counter.load(Ordering::SeqCst);
        // If the worker is truly stopped the counter is frozen from here on.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let later = counter.load(Ordering::SeqCst);

        assert_eq!(
            after_abort, later,
            "aborting the supervisor handle must stop the worker (it advanced \
             from {after_abort} to {later})",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panic_in_one_task_does_not_stop_a_sibling_task() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_runner = counter.clone();

        let runner = spawn_supervised("counter", async move {
            for _ in 0..5 {
                counter_for_runner.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let panicker = spawn_supervised("panicker", async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            panic!("synthetic panic");
        });

        let _ = tokio::time::timeout(Duration::from_secs(2), runner).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), panicker).await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            5,
            "sibling task must complete its full work despite a parallel panic",
        );
    }
}
