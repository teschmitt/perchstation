//! `POST /api/v1/enrollment/confirm/{session_id}` — the pre-enrollment
//! HTTPS exchange.
//!
//! This client is *distinct* from the post-enrollment mTLS client in
//! [`crate::perchpub::client`]:
//!
//! - Uses plain TLS (no client certificate is presented).
//! - Pins to the CA chain delivered through the QR payload, not
//!   `credentials/ca_chain.pem` (which doesn't exist yet).
//! - Disables the platform trust store entirely — the QR-bound CA is the
//!   only acceptable trust anchor.
//!
//! Retry schedule (`contracts/perchpub-api.md` §1):
//! transient 5xx / network → 5 s, 30 s, 120 s (then give up).
//! 422 → terminal, surfaced as [`ConfirmError::SessionInvalid`].
//! Other 4xx → terminal, surfaced as [`ConfirmError::ServerRejected`].

use std::io::BufReader;
use std::time::Duration;

use rcgen::KeyPair;
use reqwest::{Certificate, StatusCode};
use thiserror::Error;
use uuid::Uuid;

use crate::perchpub::types::{EnrollmentRequest, EnrollmentResponse, HTTPValidationError};

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
}

/// Retry schedule for transient 5xx / network errors. Each entry is the
/// sleep between attempts; `&[5s, 30s, 120s]` means: attempt → 5s → attempt
/// → 30s → attempt → 120s → final attempt (4 attempts total).
pub const TRANSIENT_BACKOFF: &[Duration] =
    &[Duration::from_secs(5), Duration::from_secs(30), Duration::from_mins(2)];

/// Send the enrollment confirm request. Drives the retry loop, parses the
/// response, and validates the returned cert against the local keypair and
/// the pinned CA chain.
///
/// `perchpub_base_url` is the value from `config.toml::perchpub_url`.
/// `ca_chain_pem` is the CA pin from the QR payload (must contain at least
/// one PEM cert; multiple are tolerated).
pub async fn send(
    perchpub_base_url: &str,
    ca_chain_pem: &str,
    session_id: Uuid,
    auth_token: &str,
    csr_pem: &str,
    keypair: &KeyPair,
) -> Result<ConfirmedEnrollment, ConfirmError> {
    send_with_backoff(
        perchpub_base_url,
        ca_chain_pem,
        session_id,
        auth_token,
        csr_pem,
        keypair,
        TRANSIENT_BACKOFF,
    )
    .await
}

/// Variant of [`send`] with an injectable backoff schedule. The production
/// call site uses [`TRANSIENT_BACKOFF`]; tests substitute a `&[]` (no
/// retries) or a shortened schedule to keep wall-clock under control.
pub async fn send_with_backoff(
    perchpub_base_url: &str,
    ca_chain_pem: &str,
    session_id: Uuid,
    auth_token: &str,
    csr_pem: &str,
    keypair: &KeyPair,
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
                return validate_response(response, ca_chain_pem, keypair);
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

enum AttemptError {
    Transient(String),
    Terminal(ConfirmError),
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
        return Err(AttemptError::Transient(format!("HTTP {status}: {message}")));
    }

    // 1xx / 3xx — treat as transient. reqwest follows redirects by
    // default, so a 3xx here is unusual and worth retrying.
    Err(AttemptError::Transient(format!("unexpected HTTP {status}")))
}

fn build_client(ca_chain_pem: &str) -> Result<reqwest::Client, ConfirmError> {
    let mut roots = Vec::new();
    for cert in rustls_pemfile::certs(&mut BufReader::new(ca_chain_pem.as_bytes())) {
        let cert = cert.map_err(|err| ConfirmError::TlsConfig(format!("parse CA cert: {err}")))?;
        let reqwest_cert = Certificate::from_der(cert.as_ref())
            .map_err(|err| ConfirmError::TlsConfig(format!("convert CA cert: {err}")))?;
        roots.push(reqwest_cert);
    }
    if roots.is_empty() {
        return Err(ConfirmError::CaChainEmpty);
    }

    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .https_only(true)
        .timeout(Duration::from_secs(30));
    for cert in roots {
        builder = builder.add_root_certificate(cert);
    }
    builder.build().map_err(|err| ConfirmError::TlsConfig(err.to_string()))
}

fn validate_response(
    response: EnrollmentResponse,
    ca_chain_pem: &str,
    keypair: &KeyPair,
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
    validate_chain(&certificate_pem, ca_chain_pem, &ca_chain_response)?;

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

    let mut last_err: Option<String> = None;
    for ca_der in &pinned_ders {
        let ca = match x509_parser::parse_x509_certificate(ca_der) {
            Ok((_, ca)) => ca,
            Err(err) => {
                last_err = Some(format!("parse pinned CA: {err}"));
                continue;
            }
        };
        match leaf.verify_signature(Some(ca.public_key())) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = Some(format!("verify against pinned CA: {err}")),
        }
    }
    Err(ConfirmError::ChainMismatch(
        last_err.unwrap_or_else(|| "no pinned CA verified the leaf signature".into()),
    ))
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
        let result = validate_response(response, &ca_pem, &csr.keypair).expect("ok");
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
        let err = validate_response(response, &ca_pem, &csr_a.keypair).expect_err("mismatch");
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
        let err =
            validate_response(response, &pinned_pem, &csr.keypair).expect_err("chain mismatch");
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
        let err = validate_response(response, &ca_pem, &csr.keypair).expect_err("refused");
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
        let err = validate_response(response, &ca_pem, &csr.keypair).expect_err("missing cert");
        assert!(matches!(err, ConfirmError::MissingField { field: "certificate_pem" }));
    }

    #[test]
    fn build_client_rejects_empty_ca_chain() {
        let err = build_client("").expect_err("empty CA chain");
        assert!(matches!(err, ConfirmError::CaChainEmpty));
    }
}
