//! On-device Ed25519 keypair + PKCS#10 CSR generation for enrollment.
//!
//! The keypair is built in memory by `rcgen` and never touches the disk
//! through this module — the caller (the enrollment command) keeps the
//! `KeyPair` alive only long enough to send the CSR, validate the issued
//! cert against it, and hand the PEM-serialised private key to
//! `identity::save`. The private bytes leave RAM only once, into the
//! `station.key` file written with mode `0600`.
//!
//! The CSR's subject is a placeholder (`station-enrollment`) — perchpub
//! rewrites the subject server-side when it mints the leaf cert, per
//! `contracts/perchpub-api.md` §1.

use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use thiserror::Error;

/// In-memory result of building a fresh enrollment keypair + CSR.
///
/// Hand both off to [`crate::enrollment::confirm`]: `csr_pem` goes on the
/// wire, `keypair` stays in memory until the issued cert is validated and
/// then is serialised to `station.key` by [`crate::identity::save`].
pub struct EnrollmentCsr {
    pub keypair: KeyPair,
    pub csr_pem: String,
}

#[derive(Debug, Error)]
pub enum CsrError {
    #[error("ed25519 keypair generation failed: {0}")]
    Keygen(String),
    #[error("CSR construction failed: {0}")]
    Build(String),
    #[error("CSR PEM serialisation failed: {0}")]
    Serialise(String),
}

/// Generate a fresh Ed25519 keypair and a PKCS#10 CSR signed by it.
pub fn generate() -> Result<EnrollmentCsr, CsrError> {
    let keypair =
        KeyPair::generate_for(&PKCS_ED25519).map_err(|e| CsrError::Keygen(e.to_string()))?;
    let params = CertificateParams::new(vec!["station-enrollment".into()])
        .map_err(|e| CsrError::Build(e.to_string()))?;
    let csr = params.serialize_request(&keypair).map_err(|e| CsrError::Build(e.to_string()))?;
    let csr_pem = csr.pem().map_err(|e| CsrError::Serialise(e.to_string()))?;
    Ok(EnrollmentCsr { keypair, csr_pem })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_returns_pem_csr_and_keypair() {
        let out = generate().expect("csr generation");
        assert!(
            out.csr_pem.contains("BEGIN CERTIFICATE REQUEST"),
            "csr is not PEM: {}",
            out.csr_pem
        );
        assert!(out.csr_pem.contains("END CERTIFICATE REQUEST"), "csr missing END marker");
        // The keypair's serialised PEM should be a PKCS#8 Ed25519 private key.
        let key_pem = out.keypair.serialize_pem();
        assert!(key_pem.contains("BEGIN PRIVATE KEY"), "keypair not PEM: {key_pem}");
    }

    #[test]
    fn each_invocation_yields_a_fresh_keypair() {
        let a = generate().expect("a");
        let b = generate().expect("b");
        assert_ne!(a.keypair.serialize_pem(), b.keypair.serialize_pem());
        assert_ne!(a.csr_pem, b.csr_pem);
    }

    #[test]
    fn csr_is_parseable_as_pkcs10_back_through_rcgen() {
        // Round-trip: rcgen can re-ingest the PEM via
        // CertificateSigningRequestParams::from_pem. This is the exact path
        // the fake perchpub uses when signing the CSR, so green here means
        // the wire payload is well-formed.
        let out = generate().expect("csr");
        let _ = rcgen::CertificateSigningRequestParams::from_pem(&out.csr_pem)
            .expect("CSR round-trips through rcgen");
    }
}
