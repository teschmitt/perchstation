//! Shared rustls `reqwest::ClientBuilder` construction (PS-31).
//!
//! Both the pre-enrollment confirm client ([`crate::enrollment::confirm`]) and
//! the post-enrollment mTLS client ([`crate::perchpub::client`]) pin to a
//! caller-supplied CA chain and harden TLS identically: the rustls backend,
//! the platform trust store disabled, TLS >= 1.2, HTTPS-only, and no redirect
//! following. This builds that shared base once; each caller layers on its own
//! identity (mTLS only), request timeout, and `build()`, and maps
//! [`TlsBuilderError`] into its own error type.

use std::io::BufReader;

use reqwest::{Certificate, ClientBuilder};

/// Failure building the shared TLS base. Deliberately small so each caller can
/// map it into its own error enum — `EmptyRoots` in particular has distinct
/// meaning per caller (`ConfirmError::CaChainEmpty` vs the mTLS client's
/// `TlsConfig`).
#[derive(Debug)]
pub(crate) enum TlsBuilderError {
    /// The CA PEM parsed to zero certificates — nothing to pin against.
    EmptyRoots,
    /// A certificate failed to parse or convert to a reqwest root.
    Parse(String),
}

/// Parse `ca_pem` into pinned roots and return a hardened, redirect-disabled
/// [`ClientBuilder`] trusting only those roots. The caller adds its identity
/// (mTLS only), request timeout, and calls `build()`.
pub(crate) fn rustls_builder_with_roots(ca_pem: &[u8]) -> Result<ClientBuilder, TlsBuilderError> {
    let mut roots: Vec<Certificate> = Vec::new();
    for cert in rustls_pemfile::certs(&mut BufReader::new(ca_pem)) {
        let cert = cert.map_err(|err| TlsBuilderError::Parse(format!("parse CA cert: {err}")))?;
        let reqwest_cert = Certificate::from_der(cert.as_ref())
            .map_err(|err| TlsBuilderError::Parse(format!("convert CA cert: {err}")))?;
        roots.push(reqwest_cert);
    }
    if roots.is_empty() {
        return Err(TlsBuilderError::EmptyRoots);
    }

    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .https_only(true)
        // Never follow redirects: a 3xx must surface to the caller rather than
        // a transparent reconnect to a server-named Location (which would
        // re-send the mTLS identity / the enrollment auth_token + CSR).
        .redirect(reqwest::redirect::Policy::none());
    for root in roots {
        builder = builder.add_root_certificate(root);
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pem_yields_empty_roots_error() {
        assert!(matches!(rustls_builder_with_roots(b""), Err(TlsBuilderError::EmptyRoots)));
    }

    #[test]
    fn garbage_pem_yields_empty_roots_error() {
        // `rustls_pemfile` skips lines it does not recognise as PEM, so a
        // non-PEM blob parses to zero roots rather than a parse error.
        assert!(matches!(
            rustls_builder_with_roots(b"not a pem at all"),
            Err(TlsBuilderError::EmptyRoots)
        ));
    }
}
