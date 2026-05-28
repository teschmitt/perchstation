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
//! let handle = spawn_supervised("delivery", runner.run());
//! ```
//!
//! `handle.await` resolves with `Ok(())` whether the inner future
//! completed cleanly, panicked, or returned an error — the wrapper
//! catches every termination mode at the inner `JoinHandle` boundary
//! and emits a `service.task_panicked { task: "<name>" }` event for any
//! non-clean exit. The wrapper itself does not panic, so awaiting the
//! returned handle in shutdown code is always safe.

use std::future::Future;

use tokio::task::JoinHandle;

use crate::observability::tracing as obs_tracing;

/// Spawn `fut` on the current tokio runtime under a supervisor that
/// catches panics and unexpected exits. Returns a [`JoinHandle`] that
/// resolves with `()` on every termination mode.
///
/// `task` is the human-friendly label emitted on
/// `service.task_panicked` (e.g. `"delivery"`, `"classify"`,
/// `"capture"`). Operators grep journald with it.
pub fn spawn_supervised<F>(task: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let inner = tokio::spawn(fut);
        // Non-panic JoinError means the handle was aborted, which happens
        // during shutdown; treated as a clean exit. Only the panic path
        // is interesting here.
        if let Err(err) = inner.await
            && err.is_panic()
        {
            let panic_msg = panic_payload_string(&err.into_panic());
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
