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
//! 7. Spawn the [`DeliveryRunner`], [`ClassifyPoller`], and capture
//!    [`Capture`] tasks.
//! 8. Wait for `SIGTERM` (or `SIGINT`), emit `service.shutdown`, abort
//!    the worker tasks, and return.
//!
//! Boot reconciliation (US2 T048) and SIGTERM-drain semantics (US2) land
//! later — MVP relies on the workers being abortable and on the test
//! suite's `SIGKILL` already short-circuiting cleanup paths.

use std::sync::Arc;

use anyhow::anyhow;
use perchstation_core::capture::staging::{PurgeReport, purge as purge_staging};
use perchstation_core::config::Config;
use perchstation_core::delivery::classify::ClassifyPoller;
use perchstation_core::delivery::retry::BackoffSchedule;
use perchstation_core::delivery::runner::DeliveryRunner;
use perchstation_core::hw_traits::Clock;
use perchstation_core::identity::{IdentityError, StationIdentity};
use perchstation_core::observability::tracing as obs_tracing;
use perchstation_core::perchpub::client::PerchpubClient;
use perchstation_core::queue::store::QueueStore;
use perchstation_core::supervision::spawn_supervised;
use perchstation_hw::clock::SystemClock;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::commands::CommandError;

const CAPTURE_STAGING_SUBDIR: &str = "capture-staging";

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

    // Register every body line of the on-disk station private key in
    // the redaction registry so subsequent log emissions cannot leak it
    // (T059 / FR-001). identity.json/.crt/.ca_chain are non-secret and
    // do not need scrubbing.
    register_station_key_secrets(&config.data_dir)?;

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

    let staging_path = config.data_dir.join(CAPTURE_STAGING_SUBDIR);
    let capture_purge_outcome = purge_capture_staging_or_disable(&staging_path);

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

    let runner = DeliveryRunner::new(
        store.clone(),
        client.clone(),
        clock.clone(),
        schedule,
        identity.clone(),
    );
    let poller = ClassifyPoller::new(store.clone(), client, clock.clone());

    // Wrap each long-lived worker in `spawn_supervised` so a panic in
    // one task is logged and isolated rather than aborting the others
    // (FR-012, SC-009).
    let delivery_handle = spawn_supervised("delivery", runner.run());
    let classify_handle = spawn_supervised("classify", poller.run());

    // Build the capture loop's inbox + adapters and spawn its supervised
    // task alongside delivery / classify. Linux-only: on dev hosts that
    // are not Linux the production adapters are absent — the capture
    // task is not spawned, mirroring the pattern feature 001 used for
    // the QR camera adapter.
    let capture_shutdown = CancellationToken::new();
    let capture_handle = spawn_capture_task(
        config,
        &staging_path,
        capture_purge_outcome,
        store.clone(),
        clock.clone(),
        capture_shutdown.clone(),
    );

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

    capture_shutdown.cancel();
    delivery_handle.abort();
    classify_handle.abort();

    if let Some(handle) = capture_handle {
        // Give the capture loop a brief chance to drain before falling
        // back to abort. The supervisor exits cleanly when the
        // CancellationToken fires; abort is the belt-and-braces path.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn spawn_capture_task(
    config: &Config,
    staging_path: &std::path::Path,
    purge_report: Option<PurgeReport>,
    store: QueueStore,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    use perchstation_core::capture::{Capture, CaptureState, StagingDir};
    use perchstation_core::queue::inbox::StoreInbox;
    use perchstation_core::queue::policy::{PolicyInbox, QueuePolicy};
    use perchstation_hw::camera_recorder::LibcameraVidCamera;
    use perchstation_hw::motion_sensor::GpioMotionSensor;

    // Purge failed earlier — `capture.init_failed` was already logged in
    // `run`. Do not spawn the supervisor; delivery continues regardless.
    let purge_report = purge_report?;
    let policy = QueuePolicy::from(&config.queue);
    let inbox = Arc::new(PolicyInbox::new(StoreInbox::new(store.clone()), store, policy));

    let sensor = match GpioMotionSensor::new(
        &config.capture.sensor_gpiochip,
        config.capture.sensor_line,
        config.capture.sensor_active_high,
    ) {
        Ok(s) => s,
        Err(err) => {
            // Capture failure must not block delivery (FR-012). Log a
            // warning so an operator on real hardware sees the
            // misconfiguration; on dev hosts without /dev/gpiochip0
            // this is the expected path.
            tracing::warn!(
                event = obs_tracing::events::CAPTURE_INIT_FAILED,
                reason = "sensor_open_failed",
                error = %err,
                chip = %config.capture.sensor_gpiochip.display(),
                line = config.capture.sensor_line,
                "capture loop not started: motion sensor unavailable",
            );
            return None;
        }
    };

    let camera = LibcameraVidCamera::new(
        staging_path,
        config.capture.camera_width,
        config.capture.camera_height,
        config.capture.camera_framerate,
        config.capture.camera_bitrate_bps,
    );

    let state = Arc::new(CaptureState::new());
    let capture = Capture::new(
        Box::new(sensor),
        Box::new(camera),
        inbox,
        state,
        clock,
        config.capture.clone(),
        StagingDir::new(staging_path),
    )
    .with_purge_report(purge_report);

    Some(spawn_supervised("capture", capture.run(shutdown)))
}

#[cfg(not(target_os = "linux"))]
fn spawn_capture_task(
    _config: &Config,
    _staging_path: &std::path::Path,
    _purge_report: Option<PurgeReport>,
    _store: QueueStore,
    _clock: Arc<dyn Clock>,
    _shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    // Production hardware adapters are Linux-only; on non-Linux hosts
    // (dev machines that are not Pi-like) the capture loop is simply
    // not spawned. Integration tests exercise the supervisor via the
    // in-memory fakes.
    tracing::info!(
        event = obs_tracing::events::CAPTURE_SKIPPED,
        reason = "non_linux_host",
        "capture loop not started: target_os != linux",
    );
    None
}

/// FR-017: purge `<data_dir>/capture-staging/` before `service.ready` so
/// systemd never observes `READY=1` while the capture-side staging
/// directory is in an unknown state. Returns `Some(report)` on success
/// for the supervisor to echo on `capture.ready`; on failure logs
/// `capture.init_failed` and returns `None` so the caller skips
/// spawning the capture supervisor (delivery continues regardless,
/// FR-012).
fn purge_capture_staging_or_disable(staging_path: &std::path::Path) -> Option<PurgeReport> {
    match purge_staging(staging_path) {
        Ok(report) => Some(report),
        Err(err) => {
            tracing::warn!(
                event = obs_tracing::events::CAPTURE_INIT_FAILED,
                reason = "staging_purge_failed",
                error = %err,
                staging_dir = %staging_path.display(),
                "capture loop not started: startup staging purge failed",
            );
            None
        }
    }
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

/// Read `<data_dir>/credentials/station.key` and register every non-empty
/// PEM body line in the process-wide redaction registry. Called once at
/// the start of `serve` so any log line emitted afterwards (under any
/// `--log-level`) is scrubbed of the key body before reaching stderr.
fn register_station_key_secrets(data_dir: &std::path::Path) -> Result<(), CommandError> {
    use perchstation_core::identity::{CREDENTIALS_DIR, STATION_KEY_FILE};
    use perchstation_core::observability::tracing as obs_tracing;

    let key_path = data_dir.join(CREDENTIALS_DIR).join(STATION_KEY_FILE);
    let key_pem = std::fs::read_to_string(&key_path)
        .map_err(|err| CommandError::Io(anyhow!("read `{}`: {err}", key_path.display())))?;
    for line in key_pem.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with("-----") {
            obs_tracing::register_secret(trimmed.to_string());
        }
    }
    Ok(())
}
