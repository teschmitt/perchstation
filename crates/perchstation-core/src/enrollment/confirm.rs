//! `POST /api/v1/enrollment/confirm/{session_id}` — the pre-enrollment
//! HTTPS exchange.
//!
//! This client is *distinct* from the post-enrollment mTLS client in
//! [`crate::perchpub::client`]:
//!
//! - Uses plain TLS (no client certificate is presented).
//! - Validates the public `:443` edge against the **system/public trust
//!   store** (§7): that edge is served with a publicly trusted (Let's Encrypt)
//!   certificate, so system trust — not the QR's device CA — is what anchors
//!   it. Certificate verification is never disabled (SEC-4).
//! - The QR's `ca_chain_pem` is the **device CA**, not the edge CA. It is
//!   added only as an *additive* trust anchor (so a privately-rooted dev/edge
//!   deployment still validates) and is used by [`validate_chain`] to verify
//!   the device-issued leaf — it does *not* anchor the `:443` edge.
//!
//! Retry schedule (`contracts/perchpub-api.md` §1), capped at the 3-POST
//! session budget (LIF-1):
//! transient 5xx (≠ 502) / network → 5 s, 30 s (3 attempts total, then give up).
//! 502 → terminal, surfaced as [`ConfirmError::ServerRejected`] (perchpub
//!   unreachable behind Traefik; re-provision the session).
//! 422 → terminal, surfaced as [`ConfirmError::SessionInvalid`].
//! Other 4xx (incl. 403) → terminal, surfaced as [`ConfirmError::ServerRejected`].

use std::io::BufReader;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use rcgen::KeyPair;
use reqwest::StatusCode;
use thiserror::Error;
use uuid::Uuid;

use crate::perchpub::types::{EnrollmentRequest, EnrollmentResponse, HTTPValidationError};
use crate::tls::{TlsBuilderError, rustls_builder_for_upload};

/// Successful confirm response, validated end-to-end against the station's
/// in-memory keypair. The cert and CA chain returned here are what
/// [`crate::identity::save`] will persist on disk.
#[derive(Debug, Clone)]
pub struct ConfirmedEnrollment {
    pub station_id: Uuid,
    pub certificate_pem: String,
    pub ca_chain_pem: String,
}

#[derive(Debug, Error)]
pub enum ConfirmError {
    #[error("could not build TLS client: {0}")]
    TlsConfig(String),
    #[error("pinned CA chain contains no usable certificates")]
    CaChainEmpty,
    #[error("network/server failure after {attempts} attempt(s) against `{url}`: {message}")]
    TransientExhausted { attempts: u32, url: String, message: String },
    #[error("perchpub returned 422; enrollment session invalid (detail: {detail:?})")]
    SessionInvalid { status: u16, detail: Option<HTTPValidationError> },
    #[error("perchpub returned {status}: {message}")]
    ServerRejected { status: u16, message: String },
    #[error("perchpub refused enrollment: {reason}")]
    Refused { reason: String },
    #[error("perchpub returned a success response missing required field `{field}`")]
    MissingField { field: &'static str },
    #[error("perchpub returned a certificate that is not parseable PEM: {0}")]
    CertPem(String),
    #[error(
        "perchpub returned a certificate whose subject public key does not match the station's private key"
    )]
    KeyMismatch,
    #[error("perchpub returned a certificate that does not chain to the pinned CA: {0}")]
    ChainMismatch(String),
    #[error("perchpub attempted an unsupported redirect (HTTP {status}, location: {location:?})")]
    UnexpectedRedirect { status: u16, location: Option<String> },
    #[error("perchpub returned a certificate that is not yet valid (not_before {not_before})")]
    CertNotYetValid { not_before: DateTime<Utc> },
    #[error("perchpub returned a certificate that has expired (not_after {not_after})")]
    CertExpired { not_after: DateTime<Utc> },
}

/// Retry schedule for transient 5xx / network errors. Each entry is the
/// sleep between attempts; `&[5s, 30s]` means: attempt → 5s → attempt → 30s →
/// final attempt (**3 attempts total**). Capped at three because perchpub's
/// enrollment session counts every POST — even a failed one — and allows only
/// three before the session must be re-provisioned (LIF-1, perchpub-api.md §1).
pub const TRANSIENT_BACKOFF: &[Duration] = &[Duration::from_secs(5), Duration::from_secs(30)];

/// Send the enrollment confirm request. Drives the retry loop, parses the
/// response, and validates the returned cert against the local keypair and
/// the pinned CA chain.
///
/// `perchpub_base_url` is the value from `config.toml::perchpub_url`.
/// `ca_chain_pem` is the CA pin from the QR payload (must contain at least
/// one PEM cert; multiple are tolerated). `now` is the wall-clock instant
/// the returned leaf's validity window is checked against — the caller
/// supplies it (rather than this module calling `Utc::now`) so validation
/// stays a pure function of time.
pub async fn send(
    perchpub_base_url: &str,
    ca_chain_pem: &str,
    session_id: Uuid,
    auth_token: &str,
    csr_pem: &str,
    keypair: &KeyPair,
    now: DateTime<Utc>,
) -> Result<ConfirmedEnrollment, ConfirmError> {
    send_with_backoff(
        perchpub_base_url,
        ca_chain_pem,
        session_id,
        auth_token,
        csr_pem,
        keypair,
        now,
        TRANSIENT_BACKOFF,
    )
    .await
}

/// Variant of [`send`] with an injectable backoff schedule. The production
/// call site uses [`TRANSIENT_BACKOFF`]; tests substitute a `&[]` (no
/// retries) or a shortened schedule to keep wall-clock under control.
#[allow(
    clippy::too_many_arguments,
    reason = "one-shot enrollment exchange; each parameter is a distinct wire input and bundling them would only obscure the call"
)]
pub async fn send_with_backoff(
    perchpub_base_url: &str,
    ca_chain_pem: &str,
    session_id: Uuid,
    auth_token: &str,
    csr_pem: &str,
    keypair: &KeyPair,
    now: DateTime<Utc>,
    backoff: &[Duration],
) -> Result<ConfirmedEnrollment, ConfirmError> {
    let client = build_client(ca_chain_pem)?;
    let url = format!(
        "{}/api/v1/enrollment/confirm/{}",
        perchpub_base_url.trim_end_matches('/'),
        session_id
    );
    let body =
        EnrollmentRequest { auth_token: auth_token.to_string(), csr_pem: csr_pem.to_string() };

    let max_attempts = u32::try_from(backoff.len()).unwrap_or(u32::MAX).saturating_add(1);
    let mut last_transient: Option<String> = None;

    for attempt in 1..=max_attempts {
        match attempt_once(&client, &url, &body).await {
            Ok(response) => {
                return validate_response(response, ca_chain_pem, keypair, now);
            }
            Err(AttemptError::Transient(msg)) => {
                last_transient = Some(msg);
                if attempt < max_attempts {
                    let sleep = backoff[(attempt - 1) as usize];
                    tokio::time::sleep(sleep).await;
                }
            }
            Err(AttemptError::Terminal(err)) => return Err(err),
        }
    }

    Err(ConfirmError::TransientExhausted {
        attempts: max_attempts,
        url,
        message: last_transient.unwrap_or_else(|| "no detail captured".into()),
    })
}

#[derive(Debug)]
enum AttemptError {
    Transient(String),
    Terminal(ConfirmError),
}

/// Classify a response status that fell through the 200 / 422 / 4xx / 5xx
/// arms of [`attempt_once`]. The client disables redirect-following
/// ([`build_client`] sets [`reqwest::redirect::Policy::none`]), so a 3xx
/// reaching here is the server trying to bounce our `auth_token` + CSR to
/// another host — terminal, and never retried (re-issuing the POST to the
/// `Location` would leak the one-time enrollment token). A stray 1xx is
/// genuinely unexpected and kept transient.
fn classify_non_terminal_status(status: StatusCode, location: Option<String>) -> AttemptError {
    if status.is_redirection() {
        AttemptError::Terminal(ConfirmError::UnexpectedRedirect {
            status: status.as_u16(),
            location,
        })
    } else {
        AttemptError::Transient(format!("unexpected HTTP {status}"))
    }
}

/// Classify a 5xx from the enrollment confirm endpoint (LIF-1). Per the
/// session budget (`contracts/perchpub-api.md` §1) a 502 means the perchpub
/// app is unreachable behind its Traefik front: surface it as terminal so the
/// operator re-provisions, rather than spending one of the three allowed
/// session POSTs on a doomed retry. Every other 5xx is a genuine transient
/// blip and stays retryable within the budget.
fn classify_server_error(status: StatusCode, body: String) -> AttemptError {
    if status == StatusCode::BAD_GATEWAY {
        AttemptError::Terminal(ConfirmError::ServerRejected {
            status: status.as_u16(),
            message: body,
        })
    } else {
        AttemptError::Transient(format!("HTTP {status}: {body}"))
    }
}

async fn attempt_once(
    client: &reqwest::Client,
    url: &str,
    body: &EnrollmentRequest,
) -> Result<EnrollmentResponse, AttemptError> {
    let response = match client.post(url).json(body).send().await {
        Ok(r) => r,
        Err(err) => return Err(AttemptError::Transient(format!("send error: {err}"))),
    };

    let status = response.status();
    if status == StatusCode::OK {
        let parsed: EnrollmentResponse = response
            .json()
            .await
            .map_err(|err| AttemptError::Transient(format!("decode 200 body: {err}")))?;
        return Ok(parsed);
    }

    if status == StatusCode::UNPROCESSABLE_ENTITY {
        let body_text = response.text().await.unwrap_or_default();
        let detail: Option<HTTPValidationError> = serde_json::from_str(&body_text).ok();
        return Err(AttemptError::Terminal(ConfirmError::SessionInvalid {
            status: status.as_u16(),
            detail,
        }));
    }

    if status.is_client_error() {
        let message = response.text().await.unwrap_or_default();
        return Err(AttemptError::Terminal(ConfirmError::ServerRejected {
            status: status.as_u16(),
            message,
        }));
    }

    if status.is_server_error() {
        let message = response.text().await.unwrap_or_default();
        // LIF-1: 502 is terminal (perchpub unreachable behind Traefik); other
        // 5xx remain transient within the 3-POST session budget.
        return Err(classify_server_error(status, message));
    }

    // 1xx / 3xx. Redirect-following is disabled at the client
    // (build_client), so a 3xx here means the server tried to bounce us to
    // another host — terminal, never retried. Capture Location for the
    // error before dropping the response body.
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    Err(classify_non_terminal_status(status, location))
}

fn build_client(ca_chain_pem: &str) -> Result<reqwest::Client, ConfirmError> {
    // F2/§7: validate the `:443` enrollment edge against the system/public
    // trust store — that edge is a publicly trusted (Let's Encrypt) cert, not
    // a device-CA-issued one. The QR's device CA is still required (the empty
    // check below), but only as an *additive* anchor and for verifying the
    // device-issued leaf in `validate_chain` — not to anchor the public edge,
    // so the QR no longer needs to smuggle the edge's public intermediate.
    //
    // Hardened rustls base (PS-31): rustls backend, TLS >= 1.2, HTTPS-only, no
    // redirect following — a 307/308 preserves method + body, so following it
    // would re-POST the bearer auth_token + CSR to a server-named host.
    // Certificate verification is never disabled (SEC-4). The confirm client
    // presents no identity (plain TLS) and uses a 30-second timeout.
    if ca_chain_pem.trim().is_empty() {
        return Err(ConfirmError::CaChainEmpty);
    }
    let builder =
        rustls_builder_for_upload(Some(ca_chain_pem.as_bytes())).map_err(|err| match err {
            TlsBuilderError::Parse(message) => ConfirmError::TlsConfig(message),
        })?;
    builder
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| ConfirmError::TlsConfig(err.to_string()))
}

fn validate_response(
    response: EnrollmentResponse,
    ca_chain_pem: &str,
    keypair: &KeyPair,
    now: DateTime<Utc>,
) -> Result<ConfirmedEnrollment, ConfirmError> {
    if !response.success {
        return Err(ConfirmError::Refused { reason: response.reason });
    }
    let certificate_pem =
        response.certificate_pem.ok_or(ConfirmError::MissingField { field: "certificate_pem" })?;
    let ca_chain_response =
        response.ca_chain_pem.ok_or(ConfirmError::MissingField { field: "ca_chain_pem" })?;
    let station_id =
        response.station_id.ok_or(ConfirmError::MissingField { field: "station_id" })?;

    validate_cert_against_key(&certificate_pem, keypair)?;
    validate_chain(&certificate_pem, ca_chain_pem, &ca_chain_response, now)?;

    Ok(ConfirmedEnrollment { station_id, certificate_pem, ca_chain_pem: ca_chain_response })
}

fn validate_cert_against_key(cert_pem: &str, keypair: &KeyPair) -> Result<(), ConfirmError> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|err| ConfirmError::CertPem(err.to_string()))?;
    let cert = pem.parse_x509().map_err(|err| ConfirmError::CertPem(err.to_string()))?;

    // SPKI from the cert (DER-encoded SubjectPublicKeyInfo).
    let cert_spki_raw = cert.public_key().raw;
    // SPKI from the held keypair.
    let key_spki_raw = keypair.public_key_der();
    if cert_spki_raw != key_spki_raw.as_slice() {
        return Err(ConfirmError::KeyMismatch);
    }
    Ok(())
}

fn validate_chain(
    cert_pem: &str,
    pinned_ca_pem: &str,
    response_ca_pem: &str,
    now: DateTime<Utc>,
) -> Result<(), ConfirmError> {
    // Refuse if the server tried to pivot us to a different CA than the
    // one we pinned from the QR. Comparing the parsed cert sets is robust
    // against trailing whitespace / line-ending differences.
    let pinned_ders = parse_cert_ders(pinned_ca_pem)
        .map_err(|err| ConfirmError::ChainMismatch(format!("parse pinned CA: {err}")))?;
    let response_ders = parse_cert_ders(response_ca_pem)
        .map_err(|err| ConfirmError::ChainMismatch(format!("parse response CA: {err}")))?;
    if pinned_ders.is_empty() {
        return Err(ConfirmError::CaChainEmpty);
    }
    for response_der in &response_ders {
        if !pinned_ders.iter().any(|pinned| pinned == response_der) {
            return Err(ConfirmError::ChainMismatch(
                "response ca_chain contains a cert not in the pinned QR chain".into(),
            ));
        }
    }

    // Verify the leaf's signature using any cert in the pinned chain as
    // the issuer. Iterate so a multi-cert CA chain is handled correctly.
    let (_, leaf_pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|err| ConfirmError::ChainMismatch(format!("parse leaf: {err}")))?;
    let leaf = leaf_pem
        .parse_x509()
        .map_err(|err| ConfirmError::ChainMismatch(format!("parse leaf x509: {err}")))?;

    // Reject a leaf whose validity window does not contain `now`. A
    // correctly-signed but expired / not-yet-valid leaf would otherwise be
    // persisted by identity::save and then brick every upload preflight
    // (cert_is_expired) — enrollment would "succeed" with a dead station.
    let validity = &leaf.tbs_certificate.validity;
    let not_before = asn1_to_utc(validity.not_before.timestamp()).ok_or_else(|| {
        ConfirmError::ChainMismatch("leaf not_before is not a representable UTC time".into())
    })?;
    let not_after = asn1_to_utc(validity.not_after.timestamp()).ok_or_else(|| {
        ConfirmError::ChainMismatch("leaf not_after is not a representable UTC time".into())
    })?;
    if now < not_before {
        return Err(ConfirmError::CertNotYetValid { not_before });
    }
    if now > not_after {
        return Err(ConfirmError::CertExpired { not_after });
    }

    let mut last_err: Option<String> = None;
    for ca_der in &pinned_ders {
        let ca = match x509_parser::parse_x509_certificate(ca_der) {
            Ok((_, ca)) => ca,
            Err(err) => {
                last_err = Some(format!("parse pinned CA: {err}"));
                continue;
            }
        };
        // Only a real CA may vouch for the leaf: require BasicConstraints
        // cA=TRUE plus a keyUsage permitting certificate signing. A pinned
        // leaf / non-CA cert must be skipped, not allowed to verify a cert.
        let usable_ca = ca.is_ca()
            && ca.key_usage().ok().flatten().is_some_and(|usage| usage.value.key_cert_sign());
        if !usable_ca {
            last_err = Some("pinned cert is not a usable CA (needs cA=TRUE + keyCertSign)".into());
            continue;
        }
        match leaf.verify_signature(Some(ca.public_key())) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = Some(format!("verify against pinned CA: {err}")),
        }
    }
    Err(ConfirmError::ChainMismatch(
        last_err.unwrap_or_else(|| "no pinned CA verified the leaf signature".into()),
    ))
}

/// Convert an X.509 validity timestamp (whole seconds since the Unix
/// epoch) into a UTC instant, returning `None` if it is not representable
/// (handled by the caller as a rejection, never a panic).
fn asn1_to_utc(timestamp: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(timestamp, 0).single()
}

fn parse_cert_ders(pem: &str) -> Result<Vec<Vec<u8>>, String> {
    rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
        .map(|res| res.map(|der| der.as_ref().to_vec()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use rcgen::{
        BasicConstraints, CertificateParams, CertificateSigningRequestParams, IsCa, KeyPair,
        KeyUsagePurpose, PKCS_ED25519,
    };

    fn build_ca() -> (String, rcgen::Certificate, KeyPair) {
        let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let mut params = CertificateParams::new(vec!["test-ca".into()]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(2));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2099, 1, 1);
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), cert, key)
    }

    fn sign_csr(csr_pem: &str, ca: &rcgen::Certificate, ca_key: &KeyPair) -> String {
        let params = CertificateSigningRequestParams::from_pem(csr_pem).unwrap();
        let cert = params.signed_by(ca, ca_key).unwrap();
        cert.pem()
    }

    /// Sign a CSR with an explicit validity window (year, month, day) so a
    /// test can mint expired / not-yet-valid leaves deterministically.
    fn sign_csr_with_validity(
        csr_pem: &str,
        ca: &rcgen::Certificate,
        ca_key: &KeyPair,
        not_before: (i32, u8, u8),
        not_after: (i32, u8, u8),
    ) -> String {
        let mut params = CertificateSigningRequestParams::from_pem(csr_pem).unwrap();
        params.params.not_before = rcgen::date_time_ymd(not_before.0, not_before.1, not_before.2);
        params.params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        let cert = params.signed_by(ca, ca_key).unwrap();
        cert.pem()
    }

    /// A self-signed cert that is NOT a CA (`cA=FALSE`) even though it
    /// claims `keyCertSign`. Used as a bogus issuer the gate must reject.
    fn build_non_ca_issuer() -> (String, rcgen::Certificate, KeyPair) {
        let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let mut params = CertificateParams::new(vec!["not-a-ca".into()]).unwrap();
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2099, 1, 1);
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), cert, key)
    }

    /// A fixed `now` inside the test CAs' validity window (`2026..2099`)
    /// and the rcgen default leaf window (`1975..4096`).
    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).single().unwrap()
    }

    /// Install the ring crypto provider the rustls backend needs to `build()`
    /// a reqwest client. Process-global and idempotent.
    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn validate_response_accepts_well_formed_chain() {
        let (ca_pem, ca_cert, ca_key) = build_ca();
        let csr = super::super::csr::generate().expect("csr");
        let leaf_pem = sign_csr(&csr.csr_pem, &ca_cert, &ca_key);
        let station_id = Uuid::new_v4();
        let response = EnrollmentResponse {
            success: true,
            reason: String::new(),
            certificate_pem: Some(leaf_pem.clone()),
            ca_chain_pem: Some(ca_pem.clone()),
            station_id: Some(station_id),
        };
        let result = validate_response(response, &ca_pem, &csr.keypair, fixed_now()).expect("ok");
        assert_eq!(result.station_id, station_id);
        assert!(result.certificate_pem.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn validate_response_rejects_key_mismatch() {
        let (ca_pem, ca_cert, ca_key) = build_ca();
        let csr_a = super::super::csr::generate().expect("a");
        let csr_b = super::super::csr::generate().expect("b");
        // Sign B's CSR but expect to validate against A's keypair → mismatch.
        let leaf_for_b = sign_csr(&csr_b.csr_pem, &ca_cert, &ca_key);
        let response = EnrollmentResponse {
            success: true,
            reason: String::new(),
            certificate_pem: Some(leaf_for_b),
            ca_chain_pem: Some(ca_pem.clone()),
            station_id: Some(Uuid::new_v4()),
        };
        let err = validate_response(response, &ca_pem, &csr_a.keypair, fixed_now())
            .expect_err("mismatch");
        assert!(matches!(err, ConfirmError::KeyMismatch));
    }

    #[test]
    fn validate_response_rejects_chain_mismatch() {
        let (pinned_pem, _, _) = build_ca();
        let (other_pem, other_ca, other_key) = build_ca(); // different CA entirely
        let csr = super::super::csr::generate().expect("csr");
        let leaf_pem = sign_csr(&csr.csr_pem, &other_ca, &other_key);
        let response = EnrollmentResponse {
            success: true,
            reason: String::new(),
            certificate_pem: Some(leaf_pem),
            ca_chain_pem: Some(other_pem),
            station_id: Some(Uuid::new_v4()),
        };
        let err = validate_response(response, &pinned_pem, &csr.keypair, fixed_now())
            .expect_err("chain mismatch");
        assert!(matches!(err, ConfirmError::ChainMismatch(_)));
    }

    #[test]
    fn validate_response_rejects_success_false() {
        let (ca_pem, _, _) = build_ca();
        let csr = super::super::csr::generate().expect("csr");
        let response = EnrollmentResponse {
            success: false,
            reason: "session-replayed".into(),
            certificate_pem: None,
            ca_chain_pem: None,
            station_id: None,
        };
        let err =
            validate_response(response, &ca_pem, &csr.keypair, fixed_now()).expect_err("refused");
        match err {
            ConfirmError::Refused { reason } => assert_eq!(reason, "session-replayed"),
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn validate_response_rejects_missing_required_fields() {
        let (ca_pem, _, _) = build_ca();
        let csr = super::super::csr::generate().expect("csr");
        let response = EnrollmentResponse {
            success: true,
            reason: String::new(),
            certificate_pem: None,
            ca_chain_pem: Some(ca_pem.clone()),
            station_id: Some(Uuid::new_v4()),
        };
        let err = validate_response(response, &ca_pem, &csr.keypair, fixed_now())
            .expect_err("missing cert");
        assert!(matches!(err, ConfirmError::MissingField { field: "certificate_pem" }));
    }

    #[test]
    fn validate_response_rejects_expired_leaf() {
        let (ca_pem, ca_cert, ca_key) = build_ca();
        let csr = super::super::csr::generate().expect("csr");
        let leaf_pem =
            sign_csr_with_validity(&csr.csr_pem, &ca_cert, &ca_key, (2020, 1, 1), (2021, 1, 1));
        let response = EnrollmentResponse {
            success: true,
            reason: String::new(),
            certificate_pem: Some(leaf_pem),
            ca_chain_pem: Some(ca_pem.clone()),
            station_id: Some(Uuid::new_v4()),
        };
        let err = validate_response(response, &ca_pem, &csr.keypair, fixed_now())
            .expect_err("expired leaf");
        assert!(matches!(err, ConfirmError::CertExpired { .. }));
    }

    #[test]
    fn validate_response_rejects_not_yet_valid_leaf() {
        let (ca_pem, ca_cert, ca_key) = build_ca();
        let csr = super::super::csr::generate().expect("csr");
        let leaf_pem =
            sign_csr_with_validity(&csr.csr_pem, &ca_cert, &ca_key, (2090, 1, 1), (2099, 1, 1));
        let response = EnrollmentResponse {
            success: true,
            reason: String::new(),
            certificate_pem: Some(leaf_pem),
            ca_chain_pem: Some(ca_pem.clone()),
            station_id: Some(Uuid::new_v4()),
        };
        let err = validate_response(response, &ca_pem, &csr.keypair, fixed_now())
            .expect_err("not yet valid leaf");
        assert!(matches!(err, ConfirmError::CertNotYetValid { .. }));
    }

    #[test]
    fn validate_response_rejects_leaf_signed_by_non_ca() {
        // Pin a non-CA cert and have it sign the leaf. The signature is
        // cryptographically valid, but the issuer is not a CA, so the chain
        // must be refused rather than trusting an end-entity to vouch for
        // another cert.
        let (issuer_pem, issuer_cert, issuer_key) = build_non_ca_issuer();
        let csr = super::super::csr::generate().expect("csr");
        let leaf_pem = sign_csr(&csr.csr_pem, &issuer_cert, &issuer_key);
        let response = EnrollmentResponse {
            success: true,
            reason: String::new(),
            certificate_pem: Some(leaf_pem),
            ca_chain_pem: Some(issuer_pem.clone()),
            station_id: Some(Uuid::new_v4()),
        };
        let err = validate_response(response, &issuer_pem, &csr.keypair, fixed_now())
            .expect_err("non-CA issuer");
        assert!(matches!(err, ConfirmError::ChainMismatch(_)));
    }

    #[test]
    fn build_client_rejects_empty_ca_chain() {
        let err = build_client("").expect_err("empty CA chain");
        assert!(matches!(err, ConfirmError::CaChainEmpty));
    }

    #[test]
    fn build_client_accepts_valid_ca() {
        install_crypto_provider();
        let (ca_pem, _, _) = build_ca();
        build_client(&ca_pem).expect("a valid CA chain builds a client");
    }

    #[test]
    fn build_client_validates_public_edge_with_device_ca_additive() {
        // F2/§7: the confirm client validates the public `:443` edge against
        // system trust, with the QR's device CA added only as an extra anchor
        // — so a build over a *device-CA-only* chain (no public/edge
        // intermediate) succeeds. The old builder pinned only that CA and
        // disabled public roots; this asserts that model is gone. The live
        // public-edge handshake is exercised end-to-end by
        // tests/integration/enrollment_happy.rs, where the station validates
        // fakepub's server cert via the additive device CA.
        install_crypto_provider();
        let (device_ca_pem, _, _) = build_ca();
        build_client(&device_ca_pem)
            .expect("device-CA-only chain builds a system-trust confirm client");
    }

    #[test]
    fn redirect_status_is_terminal_with_location() {
        let err = classify_non_terminal_status(
            StatusCode::TEMPORARY_REDIRECT,
            Some("https://attacker.example.org/".into()),
        );
        match err {
            AttemptError::Terminal(ConfirmError::UnexpectedRedirect { status, location }) => {
                assert_eq!(status, 307);
                assert_eq!(location.as_deref(), Some("https://attacker.example.org/"));
            }
            other => panic!("expected Terminal UnexpectedRedirect, got a different arm: {other:?}"),
        }
    }

    #[test]
    fn permanent_redirect_is_terminal() {
        assert!(matches!(
            classify_non_terminal_status(StatusCode::PERMANENT_REDIRECT, None),
            AttemptError::Terminal(ConfirmError::UnexpectedRedirect { .. })
        ));
    }

    #[test]
    fn informational_status_stays_transient() {
        assert!(matches!(
            classify_non_terminal_status(StatusCode::CONTINUE, None),
            AttemptError::Transient(_)
        ));
    }

    #[test]
    fn bad_gateway_is_terminal() {
        // LIF-1: a 502 means perchpub is unreachable behind its Traefik front.
        // Surface it (operator must re-provision) instead of spending one of
        // the three allowed session POSTs on a doomed retry.
        match classify_server_error(StatusCode::BAD_GATEWAY, "bad gateway".into()) {
            AttemptError::Terminal(ConfirmError::ServerRejected { status, .. }) => {
                assert_eq!(status, 502);
            }
            other => panic!("expected Terminal ServerRejected(502), got {other:?}"),
        }
    }

    #[test]
    fn other_5xx_stays_transient() {
        // The session budget only forbids retrying 403/502; a 500/503/504 is a
        // genuine transient blip and may still be retried within the budget.
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(
                matches!(classify_server_error(status, "x".into()), AttemptError::Transient(_)),
                "{status} must stay transient on the enrollment path",
            );
        }
    }

    #[test]
    fn confirm_session_budget_is_at_most_three_posts() {
        // LIF-1: perchpub's enrollment session counts every POST (even
        // failures) and allows only three. attempts = backoff.len() + 1.
        let max_attempts = TRANSIENT_BACKOFF.len() + 1;
        assert!(max_attempts <= 3, "confirm must not exceed 3 POSTs (got {max_attempts})");
    }
}
