//! Post-enrollment mTLS perchpub client.
//!
//! Distinct from `crate::enrollment::confirm` — that client uses plain
//! TLS against the QR-bound CA pin and has no client certificate. This
//! one:
//!
//! - Pins to `credentials/ca_chain.pem` (the same CA, re-loaded from disk).
//! - Presents the station's enrollment-issued cert (`station.crt`/`station.key`)
//!   as a TLS client identity (mTLS).
//! - Refuses any request whose host authority does not match the configured
//!   `perchpub_url` — the SC-007 outbound-allowlist invariant, enforced
//!   before the connection is opened.
//!
//! Endpoints implemented here (per `contracts/perchpub-api.md` §2 + §3):
//!
//! - [`PerchpubClient::upload_clip`] — streaming multipart `POST /api/v1/upload/`.
//! - [`PerchpubClient::get_classify_task`] — `GET /api/v1/classify-task/{id}`.
//!
//! Non-200 responses surface as [`ClientError::Http`] in MVP; T046 / T052
//! layer the per-status retry classification on top.

use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header;
use reqwest::{Body, Certificate, Client, Identity, StatusCode, Url};
use thiserror::Error;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::identity::{CA_CHAIN_FILE, CREDENTIALS_DIR, STATION_CERT_FILE, STATION_KEY_FILE};
use crate::perchpub::types::ClassifyTaskPublic;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("could not read credential file `{path}`: {source}")]
    CredentialIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not build mTLS client: {0}")]
    TlsConfig(String),
    #[error("configured perchpub_url `{url}` is not a valid URL: {message}")]
    InvalidUrl { url: String, message: String },
    #[error(
        "refused outbound request to `{actual}` (does not match configured perchpub_url authority `{expected}`)"
    )]
    OutboundDisallowed { actual: String, expected: String },
    #[error("network error talking to `{url}`: {message}")]
    Network { url: String, message: String },
    #[error("perchpub returned HTTP {status} for `{url}`: {message}")]
    Http {
        url: String,
        status: u16,
        message: String,
        /// `Retry-After` header value (in seconds) if present. Used by
        /// the retry scheduler as a floor on `next_attempt_after` (T050).
        retry_after: Option<Duration>,
    },
    #[error("could not decode response from `{url}`: {message}")]
    Decode { url: String, message: String },
    #[error("could not open clip file `{path}`: {source}")]
    ClipOpen {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// `reqwest::Client` configured for mTLS against a specific perchpub
/// origin. Clone is cheap (the inner client is internally `Arc`'d), so
/// the delivery loop and the classify poller share a single instance.
#[derive(Debug, Clone)]
pub struct PerchpubClient {
    base_url: Url,
    inner: Client,
}

impl PerchpubClient {
    /// Build a client from `<data_dir>/credentials/` and the configured
    /// perchpub URL.
    ///
    /// Reads `station.crt`, `station.key`, and `ca_chain.pem`; constructs
    /// a `rustls` `RootCertStore` containing only the pinned CA; presents
    /// the station leaf as the TLS client identity.
    pub fn new(data_dir: &Path, base_url: &str) -> Result<Self, ClientError> {
        let creds = data_dir.join(CREDENTIALS_DIR);

        let cert_path = creds.join(STATION_CERT_FILE);
        let key_path = creds.join(STATION_KEY_FILE);
        let ca_path = creds.join(CA_CHAIN_FILE);

        let cert_pem = std::fs::read(&cert_path)
            .map_err(|source| ClientError::CredentialIo { path: cert_path.clone(), source })?;
        let key_pem = std::fs::read(&key_path)
            .map_err(|source| ClientError::CredentialIo { path: key_path.clone(), source })?;
        let ca_pem = std::fs::read(&ca_path)
            .map_err(|source| ClientError::CredentialIo { path: ca_path.clone(), source })?;

        // reqwest::Identity::from_pem wants cert(s) followed by the private
        // key, all in one PEM-encoded buffer.
        let mut identity_pem = Vec::with_capacity(cert_pem.len() + key_pem.len() + 1);
        identity_pem.extend_from_slice(&cert_pem);
        if !cert_pem.ends_with(b"\n") {
            identity_pem.push(b'\n');
        }
        identity_pem.extend_from_slice(&key_pem);

        let identity = Identity::from_pem(&identity_pem)
            .map_err(|err| ClientError::TlsConfig(format!("identity: {err}")))?;

        let mut roots: Vec<Certificate> = Vec::new();
        for cert in rustls_pemfile::certs(&mut BufReader::new(ca_pem.as_slice())) {
            let cert =
                cert.map_err(|err| ClientError::TlsConfig(format!("parse CA cert: {err}")))?;
            let reqwest_cert = Certificate::from_der(cert.as_ref())
                .map_err(|err| ClientError::TlsConfig(format!("convert CA cert: {err}")))?;
            roots.push(reqwest_cert);
        }
        if roots.is_empty() {
            return Err(ClientError::TlsConfig(format!(
                "`{}` contained no certificates",
                ca_path.display()
            )));
        }

        let mut builder = Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            .https_only(true)
            .identity(identity)
            .timeout(Duration::from_mins(1));
        for root in roots {
            builder = builder.add_root_certificate(root);
        }

        let inner = builder.build().map_err(|err| ClientError::TlsConfig(err.to_string()))?;

        let base_url = Url::parse(base_url.trim_end_matches('/')).map_err(|err| {
            ClientError::InvalidUrl { url: base_url.to_string(), message: err.to_string() }
        })?;

        Ok(Self { base_url, inner })
    }

    /// Origin authority (`host[:port]`) of the configured perchpub. Used by
    /// the outbound-allowlist gate; surfaced for diagnostic logging.
    #[must_use]
    pub fn authority(&self) -> &str {
        self.base_url.authority()
    }

    /// `POST /api/v1/upload/` — streaming multipart upload of a clip
    /// previously moved into `inflight/` by the delivery loop.
    ///
    /// The body is streamed from disk via `tokio::fs::File` +
    /// `tokio_util::io::ReaderStream`; the clip is never buffered in RAM.
    pub async fn upload_clip(
        &self,
        clip_path: &Path,
        clip_id: &str,
    ) -> Result<ClassifyTaskPublic, ClientError> {
        let url = self.endpoint("/api/v1/upload/")?;
        self.check_authority(&url)?;

        let file = tokio::fs::File::open(clip_path)
            .await
            .map_err(|source| ClientError::ClipOpen { path: clip_path.to_path_buf(), source })?;
        let metadata = file
            .metadata()
            .await
            .map_err(|source| ClientError::ClipOpen { path: clip_path.to_path_buf(), source })?;
        let byte_size = metadata.len();

        let body = Body::wrap_stream(ReaderStream::new(file));
        let part = reqwest::multipart::Part::stream_with_length(body, byte_size)
            .file_name(format!("{clip_id}.mp4"))
            .mime_str("video/mp4")
            .map_err(|err| ClientError::TlsConfig(format!("mime: {err}")))?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let response =
            self.inner.post(url.clone()).multipart(form).send().await.map_err(|err| {
                ClientError::Network { url: url.to_string(), message: err.to_string() }
            })?;

        let status = response.status();
        if status != StatusCode::OK {
            let retry_after = parse_retry_after(response.headers());
            let message = response.text().await.unwrap_or_default();
            return Err(ClientError::Http {
                url: url.to_string(),
                status: status.as_u16(),
                message,
                retry_after,
            });
        }

        response
            .json::<ClassifyTaskPublic>()
            .await
            .map_err(|err| ClientError::Decode { url: url.to_string(), message: err.to_string() })
    }

    /// `GET /api/v1/classify-task/{id}` — fetch the latest perchpub-side
    /// status for a previously-uploaded clip.
    pub async fn get_classify_task(&self, id: Uuid) -> Result<ClassifyTaskPublic, ClientError> {
        let url = self.endpoint(&format!("/api/v1/classify-task/{id}"))?;
        self.check_authority(&url)?;

        let response = self.inner.get(url.clone()).send().await.map_err(|err| {
            ClientError::Network { url: url.to_string(), message: err.to_string() }
        })?;

        let status = response.status();
        if status != StatusCode::OK {
            let retry_after = parse_retry_after(response.headers());
            let message = response.text().await.unwrap_or_default();
            return Err(ClientError::Http {
                url: url.to_string(),
                status: status.as_u16(),
                message,
                retry_after,
            });
        }

        response
            .json::<ClassifyTaskPublic>()
            .await
            .map_err(|err| ClientError::Decode { url: url.to_string(), message: err.to_string() })
    }

    fn endpoint(&self, path: &str) -> Result<Url, ClientError> {
        let s = format!("{}{path}", self.base_url.as_str().trim_end_matches('/'));
        Url::parse(&s).map_err(|err| ClientError::InvalidUrl { url: s, message: err.to_string() })
    }

    fn check_authority(&self, url: &Url) -> Result<(), ClientError> {
        if url.authority() != self.base_url.authority() {
            return Err(ClientError::OutboundDisallowed {
                actual: url.authority().to_string(),
                expected: self.base_url.authority().to_string(),
            });
        }
        Ok(())
    }
}

/// Parse a `Retry-After` header value as a delta-seconds. HTTP-date
/// form is not supported (perchpub's Traefik front sends seconds).
fn parse_retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose, PKCS_ED25519,
    };
    use std::fs;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn write_credentials(dir: &Path) -> (String, String, String) {
        // Self-signed CA + station leaf, written into <dir>/credentials/.
        let ca_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let mut ca_params = CertificateParams::new(vec!["test-ca".into()]).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        ca_params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        ca_params.not_after = rcgen::date_time_ymd(2099, 1, 1);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca.pem();

        let leaf_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let mut leaf_params =
            CertificateParams::new(vec![format!("station-{}", Uuid::new_v4())]).unwrap();
        leaf_params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        leaf_params.not_after = rcgen::date_time_ymd(2099, 1, 1);
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
        let cert_pem = leaf.pem();
        let key_pem = leaf_key.serialize_pem();

        let creds = dir.join(CREDENTIALS_DIR);
        fs::create_dir_all(&creds).unwrap();
        fs::write(creds.join(STATION_CERT_FILE), &cert_pem).unwrap();
        fs::write(creds.join(STATION_KEY_FILE), &key_pem).unwrap();
        fs::write(creds.join(CA_CHAIN_FILE), &ca_pem).unwrap();
        fs::write(creds.join("identity.json"), "{}").unwrap();
        (cert_pem, key_pem, ca_pem)
    }

    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn new_loads_credentials_and_builds_client() {
        install_crypto_provider();
        let dir = TempDir::new().unwrap();
        write_credentials(dir.path());
        let client = PerchpubClient::new(dir.path(), "https://perchpub.example.org").unwrap();
        assert_eq!(client.authority(), "perchpub.example.org");
    }

    #[test]
    fn new_rejects_invalid_base_url() {
        install_crypto_provider();
        let dir = TempDir::new().unwrap();
        write_credentials(dir.path());
        let err = PerchpubClient::new(dir.path(), "not a url").expect_err("invalid url");
        assert!(matches!(err, ClientError::InvalidUrl { .. }));
    }

    #[test]
    fn new_rejects_empty_ca_chain() {
        install_crypto_provider();
        let dir = TempDir::new().unwrap();
        write_credentials(dir.path());
        let creds = dir.path().join(CREDENTIALS_DIR);
        fs::write(creds.join(CA_CHAIN_FILE), b"").unwrap();
        let err =
            PerchpubClient::new(dir.path(), "https://perchpub.example.org").expect_err("empty ca");
        assert!(matches!(err, ClientError::TlsConfig(_)));
    }

    #[tokio::test]
    async fn upload_refuses_to_change_authority() {
        // Force the URL pre-flight gate by handing the endpoint a different
        // host than the configured base. Implementation reaches this branch
        // before the network is touched.
        install_crypto_provider();
        let dir = TempDir::new().unwrap();
        write_credentials(dir.path());
        let client = PerchpubClient::new(dir.path(), "https://perchpub.example.org").unwrap();
        let evil = Url::parse("https://attacker.example.org/api/v1/upload/").unwrap();
        let err = client.check_authority(&evil).expect_err("must refuse");
        match err {
            ClientError::OutboundDisallowed { actual, expected } => {
                assert_eq!(actual, "attacker.example.org");
                assert_eq!(expected, "perchpub.example.org");
            }
            other => panic!("expected OutboundDisallowed, got {other:?}"),
        }
    }
}
