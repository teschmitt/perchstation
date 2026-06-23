//! Shared rustls `reqwest::ClientBuilder` construction (PS-31).
//!
//! Both the pre-enrollment confirm client ([`crate::enrollment::confirm`]) and
//! the post-enrollment mTLS client ([`crate::perchpub::client`]) validate the
//! perchpub edge against the **public/system trust store** and *additionally*
//! trust the operator's enrollment CA chain when supplied (§7), hardening TLS
//! identically: the rustls backend, TLS >= 1.2, HTTPS-only, and no redirect
//! following. This builds that shared base once; each caller layers on its own
//! identity (mTLS only), request timeout, and `build()`, and maps
//! [`TlsBuilderError`] into its own error type.

use std::io::BufReader;

use reqwest::{Certificate, ClientBuilder};

/// Failure building the shared TLS base. The single variant is a parse/convert
/// failure of an additive CA certificate; each caller maps it into its own
/// error type (`ConfirmError::TlsConfig` / the mTLS client's `TlsConfig`).
#[derive(Debug)]
pub(crate) enum TlsBuilderError {
    /// A certificate failed to parse or convert to a reqwest root.
    Parse(String),
}

/// Hardened, redirect-disabled [`ClientBuilder`] for the post-enrollment
/// upload client (UPL-8). Validates the perchpub *server* certificate against
/// the platform/webpki **public** root store — the perchpub edge terminates
/// TLS with a publicly-rooted (e.g. Let's Encrypt) cert — and **additionally**
/// trusts `extra_ca_pem` (the operator's enrollment CA chain) when supplied,
/// so a privately-rooted perchpub deployment validates too. The caller layers
/// on the station leaf as its mTLS *client* identity and a request timeout.
///
/// Public roots stay enabled and the supplied CA is *added*, never substituted
/// for them. The enrollment CA is already a fully operator-trusted anchor, so
/// adding it expands no real
/// trust; the outbound-authority allowlist (SC-007, enforced in
/// `perchpub::client`) remains the host-pinning defence, and SEC-4 is
/// preserved — certificate verification is never disabled. A `None` (or empty)
/// `extra_ca_pem` yields a public-roots-only client.
pub(crate) fn rustls_builder_for_upload(
    extra_ca_pem: Option<&[u8]>,
) -> Result<ClientBuilder, TlsBuilderError> {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        // Public webpki roots (the reqwest `rustls-tls-webpki-roots` feature).
        .tls_built_in_root_certs(true)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .https_only(true)
        // Never follow redirects: a 3xx must surface rather than transparently
        // re-send the mTLS client identity to a server-named Location.
        .redirect(reqwest::redirect::Policy::none());

    if let Some(ca_pem) = extra_ca_pem {
        for cert in rustls_pemfile::certs(&mut BufReader::new(ca_pem)) {
            let cert =
                cert.map_err(|err| TlsBuilderError::Parse(format!("parse CA cert: {err}")))?;
            let reqwest_cert = Certificate::from_der(cert.as_ref())
                .map_err(|err| TlsBuilderError::Parse(format!("convert CA cert: {err}")))?;
            builder = builder.add_root_certificate(reqwest_cert);
        }
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_builder_builds_with_public_roots_only() {
        // UPL-8: with no extra CA the upload builder produces a usable client
        // trusting only the public roots. Installs the crypto provider the
        // rustls backend requires.
        let _ = rustls::crypto::ring::default_provider().install_default();
        rustls_builder_for_upload(None)
            .expect("builder")
            .build()
            .expect("public-roots client builds");
    }

    #[test]
    fn upload_builder_ignores_unparseable_extra_ca() {
        // A non-PEM blob parses to zero extra roots (public roots still apply),
        // so the builder is still produced rather than erroring.
        let _ = rustls::crypto::ring::default_provider().install_default();
        rustls_builder_for_upload(Some(b"not a pem at all"))
            .expect("builder")
            .build()
            .expect("client builds with public roots and no usable extra CA");
    }
}
