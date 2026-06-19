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

    // FR-003 — refuse to clobber an existing identity. The log event
    // fires *before* we touch the source, so the operator sees the
    // refusal even if the QR was unreachable.
    if !args.force {
        match identity::peek_existing_station_id(&config.data_dir) {
            Ok(Some(existing_station_id)) => {
                tracing::error!(
                    event = obs_tracing::events::ENROLLMENT_REFUSED_OVERWRITE,
                    existing_station_id = %existing_station_id,
                    data_dir = %config.data_dir.display(),
                    "credentials already exist; re-run with --force to replace"
                );
                return Err(CommandError::Unrecoverable(anyhow!(
                    "credentials already exist for station {existing_station_id}; pass --force to replace"
                )));
            }
            Ok(None) => {}
            Err(err) => {
                return Err(CommandError::Io(anyhow!(
                    "could not inspect existing credentials: {err}"
                )));
            }
        }
    }
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

    // --- 3. Generate Ed25519 keypair + CSR ---
    let csr = csr::generate().map_err(|err| {
        tracing::error!(
            event = obs_tracing::events::ENROLLMENT_FAILED,
            kind = "csr",
            message = %err,
            "CSR generation failed"
        );
        CommandError::Io(anyhow!("CSR generation failed: {err}"))
    })?;
    tracing::info!(
        event = obs_tracing::events::ENROLLMENT_CSR_GENERATED,
        "built fresh Ed25519 keypair and CSR"
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
    let identity = persist_identity(config, &confirmed, &csr, args.force)?;

    if let Some(prev) = previous_station_id {
        tracing::warn!(
            event = obs_tracing::events::ENROLLMENT_OVERWRITTEN,
            previous_station_id = %prev,
            station_id = %identity.station_id,
            "--force replaced an existing identity"
        );
    }

    tracing::info!(
        event = obs_tracing::events::ENROLLMENT_PERSISTED,
        station_id = %identity.station_id,
        cert_not_after = %identity.cert_not_after.to_rfc3339(),
        "enrollment complete"
    );

    println!(
        "perchstation enrolled as {} (cert expires {})",
        identity.station_id,
        identity.cert_not_after.to_rfc3339(),
    );

    Ok(())
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
    force: bool,
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
            overwrite: force,
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
