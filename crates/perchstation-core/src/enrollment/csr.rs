//! On-device Ed25519 keypair + PKCS#10 CSR generation for enrollment.
//!
//! The keypair is built in memory by `rcgen` and never touches the disk
//! through this module — the caller (the enrollment command) keeps the
//! `KeyPair` alive only long enough to send the CSR, validate the issued
//! cert against it, and hand the PEM-serialised private key to
//! `identity::save`. The private bytes leave RAM only once, into the
//! `station.key` file written with mode `0600`.
//!
//! **Conformant subject (device-cert contract §3).** The CSR's identity is a
//! stable, unique, DNS-valid `station-<id>` derived from the keypair (see
//! [`station_identity`]). That identity is set as **both** the first
//! `dNSName` `SubjectAltName` **and** the Subject `CommonName` — they are equal
//! by construction — so the request never carries rcgen's default
//! `"rcgen self signed cert"` `CommonName`. perchpub authorises step-ca off the
//! first DNS SAN (CN only as a fallback); a CN that disagreed with the SAN, or
//! sat at the library default, is the §10 bug that step-ca rejects with `403`
//! → `502 "certificate issuance failed"`. perchpub does **not** rewrite the
//! subject server-side; the station owns producing a conformant one.
//!
//! **Key algorithm (KEY-1).** The station uses **Ed25519**, an intentional,
//! operator-verified divergence from the enrollment/upload spec's EC P-256
//! convention: perchpub's CA signs Ed25519 CSRs and its Traefik front accepts
//! Ed25519 client certificates. This resolves `research.md`'s "first flag to
//! flip" caveat — Ed25519 is confirmed compatible, so the deliberate choice
//! stands. Switching to `PKCS_ECDSA_P256_SHA256` would be the fallback only if
//! that compatibility ever regresses.

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ED25519, SanType};
use thiserror::Error;

use crate::observability::tracing as obs_tracing;

/// In-memory result of building an enrollment keypair + CSR.
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

/// The station's DNS identity, derived deterministically from its keypair.
///
/// `station-<hex>`, where `<hex>` is the first 8 bytes of
/// `SHA256(SubjectPublicKeyInfo)` rendered lowercase. This identity is:
/// - **stable** for the life of the keypair (§2 guarantees the key is reused,
///   so the name does not change across reloads or renewals),
/// - **unique** per station (a different key ⇒ a different SPKI ⇒ a different
///   hash, so no two stations collide), and
/// - **DNS-valid** per RFC 1123 (the `station-` prefix plus lowercase hex).
///
/// Deriving the name from the SPKI ties the human-facing identity to the exact
/// value perchpub pins (`SHA256(SubjectPublicKeyInfo)`) and needs no extra
/// persisted state — the same on-disk key always regenerates the same name.
fn station_identity(keypair: &KeyPair) -> String {
    use std::fmt::Write as _;
    let spki = keypair.public_key_der();
    let digest = ring::digest::digest(&ring::digest::SHA256, &spki);
    let mut identity = String::from("station-");
    for byte in &digest.as_ref()[..8] {
        write!(identity, "{byte:02x}").expect("write! to a String is infallible");
    }
    identity
}

/// Generate a fresh Ed25519 keypair and build a conformant PKCS#10 CSR over
/// it. The initial-enrollment / `--force` (new-station) path; every other
/// caller (renewal, plain re-enroll) reuses a persisted key via
/// [`build_from_keypair`] so the station identity (SPKI) is preserved (§2/§8).
pub fn generate() -> Result<EnrollmentCsr, CsrError> {
    let keypair =
        KeyPair::generate_for(&PKCS_ED25519).map_err(|e| CsrError::Keygen(e.to_string()))?;
    build_from_keypair(keypair)
}

/// Build a conformant PKCS#10 CSR over an **existing** keypair (§3, §11).
///
/// The keypair is moved in and returned in the [`EnrollmentCsr`] so the caller
/// keeps it alive for cert validation + persistence. The subject is set per the
/// contract: the derived [`station_identity`] becomes the sole `dNSName` SAN
/// **and** the `CommonName` (equal by construction), overwriting rcgen's default
/// `DistinguishedName`. Reusing a loaded key here keeps the same SPKI, so a
/// renewal / re-enroll re-presents the same station to perchpub.
pub fn build_from_keypair(keypair: KeyPair) -> Result<EnrollmentCsr, CsrError> {
    let identity = station_identity(&keypair);

    let mut params = CertificateParams::new(vec![identity.clone()])
        .map_err(|e| CsrError::Build(e.to_string()))?;
    // Defensive: pin exactly the SAN we intend (§3.1) — some rcgen versions
    // seed extras, and the first dNSName is the authoritative identity.
    let dns_name = SanType::DnsName(
        identity
            .as_str()
            .try_into()
            .map_err(|e| CsrError::Build(format!("identity is not a valid dNSName: {e}")))?,
    );
    params.subject_alt_names = vec![dns_name];
    // Overwrite rcgen's default DN so the CommonName is the identity, not the
    // placeholder `"rcgen self signed cert"` (§3.2 / §10).
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, identity.as_str());
    params.distinguished_name = dn;

    let csr = params.serialize_request(&keypair).map_err(|e| CsrError::Build(e.to_string()))?;
    let csr_pem = csr.pem().map_err(|e| CsrError::Serialise(e.to_string()))?;

    // Register each PEM body line in the redaction registry so any
    // accidental log emission of the CSR or private key is scrubbed
    // (T059 / FR-001). The header/footer lines (`-----BEGIN ...-----`)
    // are public structure; only the base64 body bytes are sensitive.
    for body_line in pem_body_lines(&csr_pem) {
        obs_tracing::register_secret(body_line);
    }
    let key_pem = keypair.serialize_pem();
    for body_line in pem_body_lines(&key_pem) {
        obs_tracing::register_secret(body_line);
    }

    Ok(EnrollmentCsr { keypair, csr_pem })
}

/// Extract every non-empty body line from a PEM-encoded blob, skipping
/// the `-----BEGIN/-----END` boundary lines. Each line is a single
/// 64-character base64 chunk (or shorter on the last line); registering
/// each line individually lets the redaction layer scrub log lines that
/// happened to emit a single wrapped row of the PEM.
fn pem_body_lines(pem: &str) -> Vec<String> {
    pem.lines()
        .filter(|line| !line.starts_with("-----"))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
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

    /// §12.1 structural conformance — parse the produced CSR with an
    /// independent X.509 parser and assert every acceptance-list item. This is
    /// the regression guard for F1: it fails against the old builder (which
    /// left the `CommonName` at the rcgen default and put a shared identity in
    /// the SAN).
    #[test]
    fn csr_meets_section_12_1_structural_conformance() {
        use x509_parser::certification_request::X509CertificationRequest;
        use x509_parser::extensions::{GeneralName, ParsedExtension};
        use x509_parser::prelude::FromDer;

        // Public-key algorithm OIDs accepted by the contract (§12.1).
        const ED25519: &str = "1.3.101.112";
        const EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1"; // id-ecPublicKey (P-256 params)

        let out = generate().expect("csr");

        // Parses as a PKCS#10 request.
        let (_, pem) =
            x509_parser::pem::parse_x509_pem(out.csr_pem.as_bytes()).expect("PEM frames the CSR");
        let (_, csr) =
            X509CertificationRequest::from_der(&pem.contents).expect("parses as a PKCS#10 request");

        // Proof of possession: the CSR is self-signed by the station key.
        csr.verify_signature().expect("CSR self-signature (proof of possession) verifies");

        // Public-key algorithm ∈ { Ed25519, EC P-256 }.
        let alg = csr.certification_request_info.subject_pki.algorithm.algorithm.to_id_string();
        assert!(
            alg == ED25519 || alg == EC_PUBLIC_KEY,
            "unexpected public-key algorithm OID {alg}"
        );

        // First dNSName SAN — the station's authoritative identity (§3.1).
        let first_dns = csr
            .requested_extensions()
            .expect("CSR carries requested extensions")
            .find_map(|ext| match ext {
                ParsedExtension::SubjectAlternativeName(san) => {
                    san.general_names.iter().find_map(|gn| match gn {
                        GeneralName::DNSName(name) => Some((*name).to_owned()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .expect("CSR contains at least one dNSName SAN");

        // The identity is RFC 1123-valid: lowercase letters, digits, hyphens,
        // dots; no spaces or uppercase.
        assert!(
            !first_dns.is_empty()
                && first_dns.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'
                }),
            "first dNSName `{first_dns}` is not RFC 1123-valid",
        );

        // CommonName: present, not the rcgen default, equal to the first SAN.
        let cn = csr
            .certification_request_info
            .subject
            .iter_common_name()
            .next()
            .expect("CSR subject has a CommonName")
            .as_str()
            .expect("CommonName is a UTF-8 string")
            .to_owned();
        assert_ne!(cn, "rcgen self signed cert", "CommonName left at the rcgen default");
        assert_eq!(cn, first_dns, "CommonName must equal the first dNSName SAN (§3.2)");
    }

    #[test]
    fn two_keypairs_yield_distinct_identities() {
        // §3.1 uniqueness: the identity is derived from the key, so two
        // stations (two keypairs) get different first dNSName values rather
        // than the shared `station-enrollment` constant of the old builder.
        let a = generate().expect("a");
        let b = generate().expect("b");
        assert_ne!(
            station_identity(&a.keypair),
            station_identity(&b.keypair),
            "two distinct keypairs produced the same station identity",
        );
    }

    #[test]
    fn identity_is_stable_across_reloads() {
        // §2/§8: the identity is a pure function of the key, so rebuilding the
        // CSR from the same (reloaded) key reproduces the same CN/SAN — the
        // station's identity does not drift for the life of its keypair.
        let original = generate().expect("csr");
        let reloaded = KeyPair::from_pem(&original.keypair.serialize_pem()).expect("reload key");
        assert_eq!(
            station_identity(&original.keypair),
            station_identity(&reloaded),
            "identity changed after reloading the same keypair",
        );
        // A CSR rebuilt from the reloaded key still serialises (and carries
        // that identity, asserted structurally by the conformance test above).
        let rebuilt = build_from_keypair(reloaded).expect("rebuild csr from reloaded key");
        assert!(rebuilt.csr_pem.contains("BEGIN CERTIFICATE REQUEST"));
    }
}
