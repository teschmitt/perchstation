//! Capture subsystem: motion-triggered recorder.
//!
//! Public surface:
//! - [`Capture`] — the supervised tokio task (constructor, plus the
//!   [`Capture::run`] entry point).
//! - [`CaptureConfig`] — operator-tunable knobs (re-export from
//!   [`crate::config`]).
//! - [`CaptureState`] — the in-process projection rendered by
//!   `perchstation status`.
//! - [`staging::StagingDir`] — newtype around the staging directory path.
//!
//! The internal submodules are documented at their definition sites; see
//! `specs/002-capture-subsystem/` for the spec, data-model, and contracts.

pub mod cooldown;
pub mod liveness;
pub mod recording;
pub mod runner;
pub mod staging;
pub mod state;

pub use crate::config::CaptureConfig;
pub use runner::Capture;
pub use staging::StagingDir;
pub use state::CaptureState;

use tokio_util::sync::CancellationToken;

use crate::observability::tracing as obs_tracing;

impl Capture {
    /// Run the capture supervisor until `shutdown` fires.
    ///
    /// Steps performed before the supervisor's `select!` loop begins:
    /// 1. Purge `<data_dir>/capture-staging/` so a previous run's
    ///    partial recording cannot leak across reboots (FR-017).
    /// 2. Emit `capture.ready` with the count of staging files cleaned
    ///    up by the purge.
    /// 3. Enter the supervisor loop.
    pub async fn run(self, shutdown: CancellationToken) {
        let staging_path = self.staging_path().to_path_buf();
        let report = match staging::purge(&staging_path) {
            Ok(report) => report,
            Err(err) => {
                tracing::error!(
                    event = obs_tracing::events::CAPTURE_SHUTDOWN,
                    reason = "staging_purge_failed",
                    error = %err,
                    "capture supervisor refusing to start: staging purge failed",
                );
                return;
            }
        };

        tracing::info!(
            event = obs_tracing::events::CAPTURE_READY,
            staging_purged_files = report.removed_files,
            "capture supervisor ready",
        );

        self.run_loop(shutdown).await;
    }
}
