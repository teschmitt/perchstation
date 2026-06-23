//! `perchstation enroll` — drives QR decode → CSR → mTLS POST → atomic persist.
//!
//! The four steps map 1:1 to T023/T024/T025/T026:
//! - `decode_enrollment_session` (`perchstation_core::enrollment`)
//! - `csr::generate`             (`perchstation_core::enrollment::csr`)
//! - `confirm::send`             (`perchstation_core::enrollment::confirm`)
//! - `identity::save`            (`perchstation_core::identity`)
//!
//! Behavioural contract: `specs/001-clip-delivery/contracts/cli.md` §enroll.
//! Event contract: `specs/001-clip-delivery/contracts/log-events.md` §Enrollment.

use anyhow::anyhow;
use chrono::Utc;
use perchstation_core::config::Config;
use perchstation_core::enrollment::confirm::ConfirmError;
use perchstation_core::enrollment::{
    self,
    confirm::{self, ConfirmedEnrollment},
    csr,
    file_source::FileQrSource,
};
use perchstation_core::hw_traits::{QrFrameError, QrFrameSource};
use perchstation_core::identity::{self, IdentityError, SaveOptions};
use perchstation_core::observability::tracing as obs_tracing;

use crate::cli::{EnrollArgs, QrSourceArg};
use crate::commands::CommandError;

/// Entry point invoked by `main::run`. Returns `Ok` on success; otherwise
/// maps each failure mode to the typed [`CommandError`] variant whose
/// exit-code lines up with `contracts/cli.md` §Exit codes.
#[allow(clippy::too_many_lines, reason = "linear orchestration of the enrollment steps")]
pub async fn run(args: EnrollArgs, config: &Config) -> Result<(), CommandError> {
    // Validate the whole config up front (PS-03/PS-08): `perchpub_url`
    // presence plus every numeric bound, so a malformed config is rejected
    // before we touch the QR source or generate a keypair.
    config.ensure_runtime_ready().map_err(|err| CommandError::Config(anyhow!("{err}")))?;
    let perchpub_url =
        config.perchpub_url.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
            CommandError::Config(anyhow!("`perchpub_url` is required for enrollment"))
        })?;

    // Re-enroll semantics (device-cert contract §2/§8): the keypair *is* the
    // station identity — perchpub pins its SPKI — so it is generated once and
    // reused for the station's life. A plain re-enroll therefore REUSES the
    // persisted key (same station, refreshed certificate; the §8 manual-renewal
    // path); only `--force` mints a fresh keypair = a deliberately NEW station
    // (new SPKI, prior identity discarded). The CSR step below applies that
    // choice; here we just record the prior `station_id` for the audit log.
    let previous_station_id = identity::peek_existing_station_id(&config.data_dir).ok().flatten();

    // --- 1. Acquire a QR frame ---
    let mut source = build_qr_source(&args, config)?;
    let frame = source.next_frame().await.map_err(|err| classify_qr_error(&err))?;

    // --- 2. Decode the QR payload ---
    let material = enrollment::decode_enrollment_session(&frame).map_err(|err| {
        tracing::error!(
            event = obs_tracing::events::ENROLLMENT_FAILED,
            kind = "qr_decode",
            message = %err,
            "QR decode failed"
        );
        CommandError::Io(anyhow!("QR decode failed: {err}"))
    })?;
    tracing::info!(
        event = obs_tracing::events::ENROLLMENT_QR_DECODED,
        session_id = %material.session_id,
        "decoded enrollment QR"
    );

    // --- 3. Obtain the station keypair + build the CSR ---
    //
    // The fresh-key path (`csr::generate`) is reachable ONLY at initial
    // enrollment (no persisted key) or under `--force` (deliberate new
    // station). Every other re-enroll loads `station.key` and builds the CSR
    // over it (`csr::build_from_keypair`) so the issued leaf keeps the same
    // SPKI (§2/§8). A present-but-unreadable key without `--force` is an error,
    // never a silent re-mint — that would orphan the station's identity.
    let (csr, reused_key) = if args.force {
        (build_fresh_csr()?, false)
    } else {
        match identity::load_keypair(&config.data_dir) {
            Ok(existing_key) => {
                let built = csr::build_from_keypair(existing_key).map_err(|err| {
                    tracing::error!(
                        event = obs_tracing::events::ENROLLMENT_FAILED,
                        kind = "csr",
                        message = %err,
                        "CSR build over the persisted keypair failed"
                    );
                    CommandError::Io(anyhow!("CSR build failed: {err}"))
                })?;
                (built, true)
            }
            Err(IdentityError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                (build_fresh_csr()?, false)
            }
            Err(err) => {
                tracing::error!(
                    event = obs_tracing::events::ENROLLMENT_FAILED,
                    kind = "key_load",
                    message = %err,
                    "existing station.key could not be loaded; pass --force to enroll as a NEW station"
                );
                return Err(CommandError::Io(anyhow!(
                    "existing station.key is unreadable: {err}; pass --force to enroll as a new station"
                )));
            }
        }
    };
    tracing::info!(
        event = obs_tracing::events::ENROLLMENT_CSR_GENERATED,
        key_origin = if reused_key { "reused" } else { "generated" },
        "{}",
        if reused_key {
            "reusing the persisted keypair (same SPKI, same station); refreshing the certificate"
        } else {
            "built a fresh Ed25519 keypair and CSR (new station identity / SPKI)"
        }
    );

    // --- 4. POST /enrollment/confirm ---
    let confirmed = match confirm::send(
        perchpub_url,
        &material.ca_chain_pem,
        material.session_id,
        &material.auth_token,
        &csr.csr_pem,
        &csr.keypair,
        Utc::now(),
    )
    .await
    {
        Ok(c) => {
            tracing::info!(
                event = obs_tracing::events::ENROLLMENT_SENT,
                session_id = %material.session_id,
                perchpub_url = %perchpub_url,
                "perchpub accepted enrollment"
            );
            c
        }
        Err(err) => return Err(map_confirm_error(&err)),
    };

    // --- 5. Atomically persist credentials ---
    // Overwrite whenever we are replacing an existing set — a reused key (same
    // station, refreshed cert), a `--force` new-station mint, or any prior
    // credentials on disk.
    let overwrite = reused_key || args.force || previous_station_id.is_some();
    let identity = persist_identity(config, &confirmed, &csr, overwrite)?;

    // Audit the identity transition. A reused key is the same station (no loud
    // event); a fresh key over a prior identity orphans it — loud, operator-
    // visible audit naming both the old and the new station.
    if !reused_key && let Some(prev) = previous_station_id {
        tracing::warn!(
            event = obs_tracing::events::ENROLLMENT_OVERWRITTEN,
            previous_station_id = %prev,
            station_id = %identity.station_id,
            "minted a NEW keypair (new SPKI); the previous station identity was discarded"
        );
    }

    tracing::info!(
        event = obs_tracing::events::ENROLLMENT_PERSISTED,
        station_id = %identity.station_id,
        cert_not_after = %identity.cert_not_after.to_rfc3339(),
        key_origin = if reused_key { "reused" } else { "generated" },
        "enrollment complete"
    );

    println!(
        "perchstation enrolled as {} (cert expires {})",
        identity.station_id,
        identity.cert_not_after.to_rfc3339(),
    );

    Ok(())
}

/// Generate a fresh Ed25519 keypair and build a conformant CSR over it — the
/// initial-enrollment / `--force` (new-station) path. Logs and maps the failure
/// to the I/O exit code.
fn build_fresh_csr() -> Result<csr::EnrollmentCsr, CommandError> {
    csr::generate().map_err(|err| {
        tracing::error!(
            event = obs_tracing::events::ENROLLMENT_FAILED,
            kind = "csr",
            message = %err,
            "CSR generation failed"
        );
        CommandError::Io(anyhow!("CSR generation failed: {err}"))
    })
}

fn build_qr_source(
    args: &EnrollArgs,
    config: &Config,
) -> Result<Box<dyn QrFrameSource>, CommandError> {
    match args.qr_source {
        QrSourceArg::File => {
            let path = args.qr_file.clone().ok_or_else(|| {
                CommandError::Config(anyhow!("--qr-source=file requires --qr-file"))
            })?;
            Ok(Box::new(FileQrSource::new(path)))
        }
        QrSourceArg::Camera => build_camera_source(config),
    }
}

#[cfg(target_os = "linux")]
fn build_camera_source(config: &Config) -> Result<Box<dyn QrFrameSource>, CommandError> {
    // The camera-binary name lives in the hardware-specific `[capture]` knobs
    // that core carries opaquely (PS-29/PS-30); decode them here at the wiring
    // layer to find which still-capture binary to shell out to.
    let hw = perchstation_hw::capture_config::CaptureHwConfig::from_table(&config.capture.hardware)
        .map_err(|err| CommandError::Config(anyhow!("invalid [capture] hardware config: {err}")))?;
    Ok(Box::new(
        perchstation_hw::camera_qr::CameraQrSource::new()
            .with_binary(hw.camera_still_command.clone()),
    ))
}

#[cfg(not(target_os = "linux"))]
fn build_camera_source(_config: &Config) -> Result<Box<dyn QrFrameSource>, CommandError> {
    Err(CommandError::Config(anyhow!(
        "--qr-source=camera is only supported on Linux (Pi). Use --qr-source=file on dev hosts.",
    )))
}

fn classify_qr_error(err: &QrFrameError) -> CommandError {
    tracing::error!(
        event = obs_tracing::events::ENROLLMENT_FAILED,
        kind = "qr_source",
        message = %err,
        "could not acquire QR frame"
    );
    CommandError::Io(anyhow!("could not acquire QR frame: {err}"))
}

fn map_confirm_error(err: &ConfirmError) -> CommandError {
    match err {
        ConfirmError::SessionInvalid { status, .. } => {
            tracing::error!(
                event = obs_tracing::events::ENROLLMENT_SESSION_INVALID,
                status = *status,
                message = %err,
                "enrollment session rejected (422)"
            );
            CommandError::Unrecoverable(anyhow!("{err}"))
        }
        ConfirmError::Refused { reason } => {
            tracing::error!(
                event = obs_tracing::events::ENROLLMENT_FAILED,
                kind = "refused",
                message = %reason,
                "perchpub refused enrollment"
            );
            CommandError::Unrecoverable(anyhow!("{err}"))
        }
        ConfirmError::ServerRejected { status, .. } => {
            tracing::error!(
                event = obs_tracing::events::ENROLLMENT_FAILED,
                kind = "server_rejected",
                status = *status,
                message = %err,
                "perchpub returned a non-retryable status"
            );
            CommandError::Unrecoverable(anyhow!("{err}"))
        }
        ConfirmError::TransientExhausted { attempts, .. } => {
            tracing::error!(
                event = obs_tracing::events::ENROLLMENT_FAILED,
                kind = "transient_exhausted",
                attempts = *attempts,
                message = %err,
                "enrollment failed after retry budget exhausted"
            );
            CommandError::Transient(anyhow!("{err}"))
        }
        ConfirmError::KeyMismatch
        | ConfirmError::ChainMismatch(_)
        | ConfirmError::CertPem(_)
        | ConfirmError::CertExpired { .. }
        | ConfirmError::CertNotYetValid { .. }
        | ConfirmError::UnexpectedRedirect { .. }
        | ConfirmError::MissingField { .. }
        | ConfirmError::CaChainEmpty
        | ConfirmError::TlsConfig(_) => {
            tracing::error!(
                event = obs_tracing::events::ENROLLMENT_FAILED,
                kind = "validation",
                message = %err,
                "enrollment validation failed"
            );
            CommandError::Io(anyhow!("{err}"))
        }
    }
}

fn persist_identity(
    config: &Config,
    confirmed: &ConfirmedEnrollment,
    csr: &csr::EnrollmentCsr,
    overwrite: bool,
) -> Result<perchstation_core::identity::StationIdentity, CommandError> {
    identity::save(
        &config.data_dir,
        &SaveOptions {
            station_id: confirmed.station_id,
            enrolled_at: Utc::now(),
            perchpub_url: config.perchpub_url.as_deref().unwrap_or(""),
            station_key_pem: &csr.keypair.serialize_pem(),
            station_cert_pem: &confirmed.certificate_pem,
            ca_chain_pem: &confirmed.ca_chain_pem,
            overwrite,
        },
    )
    .map_err(|err| match err {
        IdentityError::AlreadyExists { existing_station_id, .. } => {
            // Defence-in-depth — the pre-check above should have caught this.
            tracing::error!(
                event = obs_tracing::events::ENROLLMENT_REFUSED_OVERWRITE,
                existing_station_id = %existing_station_id,
                "save refused to clobber existing credentials"
            );
            CommandError::Unrecoverable(anyhow!(
                "credentials already exist for {existing_station_id}; pass --force to replace"
            ))
        }
        other => {
            tracing::error!(
                event = obs_tracing::events::ENROLLMENT_FAILED,
                kind = "persist",
                message = %other,
                "could not persist credentials"
            );
            CommandError::Io(anyhow!("identity persist failed: {other}"))
        }
    })
}
