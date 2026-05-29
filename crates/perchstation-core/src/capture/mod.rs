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
    /// The startup staging-purge (FR-017) is the wiring layer's
    /// responsibility — `serve` runs it *before* `service.ready` so
    /// systemd never observes a `READY=1` until the staging directory
    /// is clean. The resulting [`staging::PurgeReport`] is fed in via
    /// [`Capture::with_purge_report`]; this method just emits
    /// `capture.ready` echoing the report and then enters the
    /// supervisor's `select!` loop.
    ///
    /// The staging directory is `create_dir_all`'d here as a defensive
    /// no-op so that fake-camera-driven integration tests (which call
    /// `Capture::run` directly without going through `serve`) can write
    /// fixture files into it before the first trigger arrives.
    pub async fn run(self, shutdown: CancellationToken) {
        if let Err(err) = std::fs::create_dir_all(self.staging_path()) {
            tracing::warn!(
                event = obs_tracing::events::CAPTURE_INIT_FAILED,
                reason = "staging_dir_create_failed",
                error = %err,
                staging_dir = %self.staging_path().display(),
                "capture supervisor refusing to start: staging dir create failed",
            );
            return;
        }
        tracing::info!(
            event = obs_tracing::events::CAPTURE_READY,
            staging_purged_files = self.purge_report().removed_files,
            "capture supervisor ready",
        );
        self.run_loop(shutdown).await;
    }
}
