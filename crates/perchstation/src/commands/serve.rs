//! `perchstation serve` — the long-running delivery + classify-task daemon.
//!
//! Steps (T038/T039):
//!
//! 1. Validate `perchpub_url` is set; otherwise exit 70.
//! 2. Load `<data_dir>/credentials/`; absence → exit 76 ("not enrolled").
//! 3. Build the mTLS [`PerchpubClient`] from the on-disk credentials.
//! 4. Open the [`QueueStore`] (creates `pending/`/`inflight/`/`delivered/`
//!    on first use).
//! 5. Count `pending_at_start` from `<data_dir>/queue/pending/`.
//! 6. Emit `service.ready` and call `sd_notify(READY=1)` immediately
//!    afterwards so systemd `Type=notify` observes a truthful resume
//!    timestamp (SC-003).
//! 7. Spawn the [`DeliveryRunner`] and [`ClassifyPoller`] tasks.
//! 8. Wait for `SIGTERM` (or `SIGINT`), emit `service.shutdown`, abort
//!    the worker tasks, and return.
//!
//! Boot reconciliation (US2 T048) and SIGTERM-drain semantics (US2) land
//! later — MVP relies on the workers being abortable and on the test
//! suite's `SIGKILL` already short-circuiting cleanup paths.

use std::sync::Arc;

use anyhow::anyhow;
use perchstation_core::config::Config;
use perchstation_core::delivery::classify::ClassifyPoller;
use perchstation_core::delivery::retry::BackoffSchedule;
use perchstation_core::delivery::runner::DeliveryRunner;
use perchstation_core::hw_traits::Clock;
use perchstation_core::identity::{IdentityError, StationIdentity};
use perchstation_core::observability::tracing as obs_tracing;
use perchstation_core::perchpub::client::PerchpubClient;
use perchstation_core::queue::store::QueueStore;
use perchstation_hw::clock::SystemClock;
use tokio::signal::unix::{SignalKind, signal};

use crate::commands::CommandError;

pub async fn run(config: &Config) -> Result<(), CommandError> {
    let perchpub_url = match config.perchpub_url.as_deref() {
        Some(url) if !url.is_empty() => url,
        _ => {
            return Err(CommandError::Config(anyhow!(
                "`perchpub_url` is required for `perchstation serve`"
            )));
        }
    };

    let identity = match StationIdentity::load(&config.data_dir) {
        Ok(id) => id,
        Err(IdentityError::Io { source, path })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Err(CommandError::Unrecoverable(anyhow!(
                "station is not enrolled (missing `{}`); run `perchstation enroll` first",
                path.display()
            )));
        }
        Err(err) => return Err(CommandError::Io(anyhow!("identity load failed: {err}"))),
    };

    let client = PerchpubClient::new(&config.data_dir, perchpub_url)
        .map_err(|err| CommandError::Io(anyhow!("perchpub client init: {err}")))?;

    let store = QueueStore::open(&config.data_dir)
        .map_err(|err| CommandError::Io(anyhow!("queue init: {err}")))?;

    // Boot reconciliation: move any leftover `inflight/` entries back to
    // `pending/` so a previous crash mid-upload resumes cleanly (T048).
    let recovered = store
        .reconcile_inflight()
        .map_err(|err| CommandError::Io(anyhow!("boot reconciliation: {err}")))?;
    for entry in &recovered {
        tracing::warn!(
            event = obs_tracing::events::QUEUE_RECOVERED_INFLIGHT,
            clip_id = %entry.clip_id,
            "re-queued inflight entry after crash",
        );
    }

    let pending_at_start =
        count_pending(&store).map_err(|err| CommandError::Io(anyhow!("count pending: {err}")))?;

    tracing::info!(
        event = obs_tracing::events::SERVICE_READY,
        station_id = %identity.station_id,
        pending_at_start = pending_at_start,
        "perchstation serve ready",
    );
    // Best-effort notify — systemd may not be present (dev hosts, tests).
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let schedule = BackoffSchedule::from_config(&config.retry);

    let runner = DeliveryRunner::new(store.clone(), client.clone(), clock.clone(), schedule);
    let poller = ClassifyPoller::new(store, client, clock);

    let delivery_handle = tokio::spawn(runner.run());
    let classify_handle = tokio::spawn(poller.run());

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|err| CommandError::Io(anyhow!("install SIGTERM handler: {err}")))?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|err| CommandError::Io(anyhow!("install SIGINT handler: {err}")))?;

    let reason = tokio::select! {
        _ = sigterm.recv() => "sigterm",
        _ = sigint.recv() => "sigint",
    };

    tracing::info!(
        event = obs_tracing::events::SERVICE_SHUTDOWN,
        reason = reason,
        "perchstation serve shutting down",
    );

    delivery_handle.abort();
    classify_handle.abort();

    Ok(())
}

fn count_pending(store: &QueueStore) -> std::io::Result<u32> {
    let pending = store.pending_dir();
    let mut count = 0_u32;
    for entry in std::fs::read_dir(&pending)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|e| e == "json") {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}
