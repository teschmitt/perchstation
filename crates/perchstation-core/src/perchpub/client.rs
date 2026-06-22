//! Post-enrollment mTLS perchpub client.
//!
//! Distinct from `crate::enrollment::confirm` — that client uses plain
//! TLS against the QR-bound CA pin and has no client certificate. This
//! one:
//!
//! - Validates perchpub's *server* certificate against the public root store
//!   (UPL-8): the perchpub edge terminates TLS with a publicly-rooted (e.g.
//!   Let's Encrypt) cert. The enrollment CA chain (`ca_chain.pem`) is trusted
//!   *additionally* when present, so a privately-rooted deployment works too.
//! - Presents the station's enrollment-issued cert (`station.crt`/`station.key`)
//!   as a TLS *client* identity (mTLS).
//! - Refuses any request whose host authority does not match the configured
//!   upload base — the SC-007 outbound-allowlist invariant, enforced
//!   before the connection is opened.
//!
//! Endpoints implemented here (per `contracts/perchpub-api.md` §2 + §3):
//!
//! - [`PerchpubClient::upload_clip`] — streaming multipart `POST /api/v1/upload/`.
//! - [`PerchpubClient::get_classify_task`] — `GET /api/v1/classify-task/{id}`.
//!
//! Response handling: any 2xx is success (PS-22); a 2xx whose body won't
//! decode becomes [`ClientError::UndecodableSuccess`] (PS-06, the clip is
//! already stored — never re-uploaded); every non-2xx surfaces as
//! [`ClientError::Http`] for the retry classifier (T046 / T052). All bodies
//! are read under a fixed size cap to bound memory on the Pi (PS-16).
//! Credentials can be hot-reloaded after re-enrollment via
//! [`PerchpubClient::reload`] (PS-18) — no `serve` restart required.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::header;
use reqwest::{Body, Client, Identity, StatusCode, Url};
use thiserror::Error;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::identity::{CA_CHAIN_FILE, CREDENTIALS_DIR, STATION_CERT_FILE, STATION_KEY_FILE};
use crate::perchpub::types::ClassifyTaskPublic;
use crate::queue::store::QueueStore;
use crate::tls::{TlsBuilderError, rustls_builder_for_upload};

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
    /// A 2xx response whose body could not be decoded into the expected
    /// schema (PS-06). On the upload path the bytes are *already stored* by
    /// perchpub, so this must NOT trigger a re-upload — the runner records
    /// the clip delivered (with an unknown classify status). Distinct from
    /// [`ClientError::Decode`] precisely so the retry classifier can tell
    /// "accepted but unreadable" apart from "retryable transport failure".
    #[error("perchpub returned an undecodable 2xx body from `{url}`: {message}")]
    UndecodableSuccess { url: String, message: String },
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
///
/// The TLS material lives behind an `RwLock` so [`PerchpubClient::reload`]
/// can hot-swap the identity + root store after a re-enrollment overwrites
/// `credentials/` — without a `serve` restart (PS-18). All clones share the
/// lock, so a reload via any handle is observed by the runner and poller.
#[derive(Debug, Clone)]
pub struct PerchpubClient {
    base_url: Url,
    /// `<data_dir>/credentials/`, retained so `reload()` can rebuild the
    /// inner client from the latest on-disk TLS material.
    creds_dir: PathBuf,
    inner: Arc<RwLock<Client>>,
}

/// Build the mTLS `reqwest::Client` from the credential material in `creds`:
/// `station.crt` + `station.key` for the client identity (both required), and
/// — if present — `ca_chain.pem` as a supplementary server-trust anchor on top
/// of the public roots (UPL-8). Shared by [`PerchpubClient::new`] and
/// [`PerchpubClient::reload`] so the two can never drift in their TLS
/// hardening (PS-18).
fn build_inner(creds: &Path) -> Result<Client, ClientError> {
    let cert_path = creds.join(STATION_CERT_FILE);
    let key_path = creds.join(STATION_KEY_FILE);

    let cert_pem = std::fs::read(&cert_path)
        .map_err(|source| ClientError::CredentialIo { path: cert_path.clone(), source })?;
    let key_pem = std::fs::read(&key_path)
        .map_err(|source| ClientError::CredentialIo { path: key_path.clone(), source })?;

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

    // Additionally trust the operator's enrollment CA chain when present
    // (UPL-8): always there in a normal deployment, absent only for a pure
    // public-edge setup. `NotFound` is fine (public roots suffice); any other
    // read error surfaces.
    let ca_path = creds.join(CA_CHAIN_FILE);
    let ca_pem = match std::fs::read(&ca_path) {
        Ok(pem) => Some(pem),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(source) => return Err(ClientError::CredentialIo { path: ca_path, source }),
    };

    // Hardened rustls base (PS-31) validating perchpub's *server* cert against
    // the public root store plus the enrollment CA when present (UPL-8):
    // rustls backend, TLS >= 1.2, HTTPS-only, no redirect following. The mTLS
    // client layers on its station leaf as the *client* identity plus a
    // 1-minute request timeout. The no-redirect policy (SC-007 / T060) catches
    // server-driven URL swaps; the allowlist gate (`check_authority`) catches
    // caller-built URLs.
    let builder = rustls_builder_for_upload(ca_pem.as_deref()).map_err(|err| match err {
        TlsBuilderError::EmptyRoots => {
            ClientError::TlsConfig("upload TLS builder produced no roots".to_owned())
        }
        TlsBuilderError::Parse(message) => ClientError::TlsConfig(message),
    })?;

    builder
        .identity(identity)
        .timeout(Duration::from_mins(1))
        .build()
        .map_err(|err| ClientError::TlsConfig(err.to_string()))
}

impl PerchpubClient {
    /// Build a client from `<data_dir>/credentials/` and the configured
    /// upload base URL.
    ///
    /// Reads `station.crt` and `station.key` for the mTLS client identity, and
    /// `ca_chain.pem` (if present) as a supplementary server-trust anchor on
    /// top of the public roots (UPL-8).
    pub fn new(data_dir: &Path, base_url: &str) -> Result<Self, ClientError> {
        let creds = data_dir.join(CREDENTIALS_DIR);
        let inner = build_inner(&creds)?;

        let base_url = Url::parse(base_url.trim_end_matches('/')).map_err(|err| {
            ClientError::InvalidUrl { url: base_url.to_string(), message: err.to_string() }
        })?;

        Ok(Self { base_url, creds_dir: creds, inner: Arc::new(RwLock::new(inner)) })
    }

    /// Rebuild the inner mTLS client from the current on-disk credentials and
    /// hot-swap it in (PS-18). Call after a re-enrollment overwrites
    /// `credentials/` so the long-running delivery loop / classify poller
    /// present the new identity without a `serve` restart. On failure the
    /// previous working client is left untouched — a bad reload never bricks
    /// an already-running station. `base_url` / [`authority`](Self::authority)
    /// are unaffected.
    pub fn reload(&self) -> Result<(), ClientError> {
        let rebuilt = build_inner(&self.creds_dir)?;
        *self.inner.write().expect("perchpub client lock poisoned") = rebuilt;
        Ok(())
    }

    /// Snapshot of the current inner client (a cheap `Arc` clone) for one
    /// request. Taken under a short read lock so it never blocks a concurrent
    /// [`reload`](Self::reload).
    fn client(&self) -> Client {
        self.inner.read().expect("perchpub client lock poisoned").clone()
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
            .file_name(QueueStore::media_name(clip_id))
            .mime_str("video/mp4")
            .map_err(|err| ClientError::TlsConfig(format!("mime: {err}")))?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let response =
            self.client().post(url.clone()).multipart(form).send().await.map_err(|err| {
                ClientError::Network { url: url.to_string(), message: err.to_string() }
            })?;

        let capped = read_capped(response, Utc::now(), url.as_str()).await?;
        classify_response(&capped, url.as_str())
    }

    /// `GET /api/v1/classify-task/{id}` — fetch the latest perchpub-side
    /// status for a previously-uploaded clip.
    pub async fn get_classify_task(&self, id: Uuid) -> Result<ClassifyTaskPublic, ClientError> {
        let url = self.endpoint(&format!("/api/v1/classify-task/{id}"))?;
        self.check_authority(&url)?;

        let response = self.client().get(url.clone()).send().await.map_err(|err| {
            ClientError::Network { url: url.to_string(), message: err.to_string() }
        })?;

        let capped = read_capped(response, Utc::now(), url.as_str()).await?;
        classify_response(&capped, url.as_str())
    }

    fn endpoint(&self, path: &str) -> Result<Url, ClientError> {
        let s = format!("{}{path}", self.base_url.as_str().trim_end_matches('/'));
        Url::parse(&s).map_err(|err| ClientError::InvalidUrl { url: s, message: err.to_string() })
    }

    fn check_authority(&self, url: &Url) -> Result<(), ClientError> {
        if url.authority() != self.base_url.authority() {
            // SC-007 / T060 invariant — surface the offending URL so a
            // future regression is visible in journald rather than hidden
            // inside the typed error.
            tracing::error!(
                actual = %url.authority(),
                expected = %self.base_url.authority(),
                "refused outbound request to off-allowlist authority",
            );
            return Err(ClientError::OutboundDisallowed {
                actual: url.authority().to_string(),
                expected: self.base_url.authority().to_string(),
            });
        }
        Ok(())
    }
}

/// Maximum response body the station will buffer from perchpub (PS-16).
/// The bodies we parse (`ClassifyTaskPublic`, `HTTPValidationError`) are a
/// few hundred bytes; 1 MiB is a generous ceiling that still stops a
/// buggy/compromised — but CA-pinned — perchpub from OOM-killing the Pi by
/// streaming a multi-gigabyte body under the request timeout.
const MAX_RESPONSE_BYTES: u64 = 1 << 20;

/// A perchpub response read into memory under the [`MAX_RESPONSE_BYTES`]
/// cap, with the status and `Retry-After` captured before the body was
/// consumed (so they survive an over-limit bail).
#[derive(Debug)]
struct CappedResponse {
    status: StatusCode,
    retry_after: Option<Duration>,
    body: Vec<u8>,
}

/// Buffer a response body into memory, refusing anything larger than
/// [`MAX_RESPONSE_BYTES`] (PS-16). `status` and `Retry-After` are read
/// before the body is consumed. The streaming accumulator is the real
/// guard; a `Content-Length` over the cap short-circuits before any read.
async fn read_capped(
    response: reqwest::Response,
    now: DateTime<Utc>,
    url: &str,
) -> Result<CappedResponse, ClientError> {
    let status = response.status();
    let retry_after = parse_retry_after(response.headers(), now);

    // Defence-in-depth: reject before reading when the server advertises an
    // over-cap Content-Length (chunked responses report none, hence the
    // streaming guard below is the real enforcement).
    if response.content_length().is_some_and(|len| len > MAX_RESPONSE_BYTES) {
        return Err(ClientError::Decode {
            url: url.to_string(),
            message: format!("response Content-Length exceeds {MAX_RESPONSE_BYTES}-byte cap"),
        });
    }

    let mut response = response;
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() as u64 + chunk.len() as u64 > MAX_RESPONSE_BYTES {
                    return Err(ClientError::Decode {
                        url: url.to_string(),
                        message: format!("response body exceeds {MAX_RESPONSE_BYTES}-byte cap"),
                    });
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(err) => {
                return Err(ClientError::Network {
                    url: url.to_string(),
                    message: err.to_string(),
                });
            }
        }
    }
    Ok(CappedResponse { status, retry_after, body })
}

/// Turn a (size-capped) perchpub response into a [`ClassifyTaskPublic`] or
/// a typed error. Pure, so unit-testable without a live server.
fn classify_response(
    capped: &CappedResponse,
    url: &str,
) -> Result<ClassifyTaskPublic, ClientError> {
    // PS-22: any 2xx is success — perchpub or its Traefik front may answer
    // 201/202 for an accepted upload.
    if !capped.status.is_success() {
        return Err(ClientError::Http {
            url: url.to_string(),
            status: capped.status.as_u16(),
            message: String::from_utf8_lossy(&capped.body).into_owned(),
            retry_after: capped.retry_after,
        });
    }
    // PS-06: a 2xx whose body won't decode means the clip is already stored
    // — `UndecodableSuccess` (not `Decode`) so the retry classifier keeps it
    // off the re-upload path. An *unknown status string* decodes fine now
    // (`ClassifyTaskStatus::Unknown`), so this only fires on malformed JSON.
    serde_json::from_slice::<ClassifyTaskPublic>(&capped.body).map_err(|err| {
        ClientError::UndecodableSuccess { url: url.to_string(), message: err.to_string() }
    })
}

/// Parse a `Retry-After` header into a delay relative to `now`.
///
/// Both RFC-7231 forms are honoured (PS-23): the delta-seconds fast path
/// and the HTTP-date (IMF-fixdate / RFC-2822-compatible) form. A date in
/// the past clamps to zero; an unparseable value yields `None`. `now` is
/// passed in (rather than read from a `Clock`) because this free function
/// has none in scope — the caller supplies `chrono::Utc::now()`.
fn parse_retry_after(headers: &header::HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let raw = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    // Fast path: delta-seconds.
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // Fallback: HTTP-date form (`Wed, 21 Oct 2015 07:28:00 GMT`), which is
    // RFC-2822-compatible. A past date clamps to zero.
    let when = DateTime::parse_from_rfc2822(raw).ok()?.with_timezone(&Utc);
    Some((when - now).to_std().unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perchpub::types::ClassifyTaskStatus;
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
    fn new_builds_without_pinned_ca() {
        // UPL-8: the upload client validates perchpub's *server* cert against
        // the public root store (the edge presents a Let's Encrypt cert), so
        // it no longer needs `ca_chain.pem` on disk — only the station leaf +
        // key for its mTLS *client* identity.
        install_crypto_provider();
        let dir = TempDir::new().unwrap();
        write_credentials(dir.path());
        let creds = dir.path().join(CREDENTIALS_DIR);
        fs::remove_file(creds.join(CA_CHAIN_FILE)).unwrap();
        let client = PerchpubClient::new(dir.path(), "https://api.perchpub.net:8443")
            .expect("upload client builds without a pinned CA on disk");
        assert_eq!(client.authority(), "api.perchpub.net:8443");
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
    fn new_rejects_missing_station_cert() {
        // The mTLS client identity is still mandatory — a missing
        // `station.crt` must surface, even though the pinned CA is gone.
        install_crypto_provider();
        let dir = TempDir::new().unwrap();
        write_credentials(dir.path());
        let creds = dir.path().join(CREDENTIALS_DIR);
        fs::remove_file(creds.join(STATION_CERT_FILE)).unwrap();
        let err = PerchpubClient::new(dir.path(), "https://perchpub.example.org")
            .expect_err("missing station cert");
        assert!(matches!(err, ClientError::CredentialIo { .. }), "got {err:?}");
    }

    #[test]
    fn reload_rebuilds_with_fresh_credentials() {
        // PS-18: after re-enrollment overwrites credentials/, reload() picks
        // up the new TLS material and the configured authority is unchanged.
        install_crypto_provider();
        let dir = TempDir::new().unwrap();
        write_credentials(dir.path());
        let client = PerchpubClient::new(dir.path(), "https://perchpub.example.org").unwrap();
        let before = client.authority().to_string();

        // Re-enroll: a brand-new CA + leaf land in credentials/.
        write_credentials(dir.path());
        client.reload().expect("reload with fresh valid credentials");
        assert_eq!(client.authority(), before, "reload must not change the pinned authority");
    }

    #[test]
    fn reload_failure_leaves_client_usable() {
        // PS-18: a bad reload (here a corrupt station.crt — no parseable
        // client identity) must NOT poison the already-running client; the
        // previous identity stays in place and a later valid reload succeeds.
        install_crypto_provider();
        let dir = TempDir::new().unwrap();
        write_credentials(dir.path());
        let client = PerchpubClient::new(dir.path(), "https://perchpub.example.org").unwrap();

        let creds = dir.path().join(CREDENTIALS_DIR);
        fs::write(
            creds.join(STATION_CERT_FILE),
            b"-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let err = client.reload().expect_err("corrupt station cert must fail reload");
        assert!(matches!(err, ClientError::TlsConfig(_)), "got {err:?}");
        // Still usable: authority intact, and restoring valid creds reloads.
        assert_eq!(client.authority(), "perchpub.example.org");
        write_credentials(dir.path());
        client.reload().expect("valid reload after a failed one");
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

    // --- PS-23: Retry-After parsing (delta-seconds + HTTP-date forms) ---

    fn retry_after_headers(value: &str) -> header::HeaderMap {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::RETRY_AFTER, value.parse().expect("header value"));
        headers
    }

    fn ra_now() -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 5, 27, 12, 0, 0).unwrap()
    }

    #[test]
    fn retry_after_delta_seconds() {
        assert_eq!(
            parse_retry_after(&retry_after_headers("120"), ra_now()),
            Some(Duration::from_mins(2)),
        );
    }

    #[test]
    fn retry_after_http_date_future_returns_delta() {
        // IMF-fixdate 2 min in the future of `ra_now()`.
        let got =
            parse_retry_after(&retry_after_headers("Wed, 27 May 2026 12:02:00 GMT"), ra_now());
        assert_eq!(got, Some(Duration::from_mins(2)));
    }

    #[test]
    fn retry_after_http_date_past_clamps_to_zero() {
        let got =
            parse_retry_after(&retry_after_headers("Wed, 27 May 2026 11:58:00 GMT"), ra_now());
        assert_eq!(got, Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_garbage_is_none() {
        assert!(parse_retry_after(&retry_after_headers("not-a-date"), ra_now()).is_none());
    }

    #[test]
    fn retry_after_absent_is_none() {
        assert!(parse_retry_after(&header::HeaderMap::new(), ra_now()).is_none());
    }

    // --- PS-16 / PS-22 / PS-06: capped read + response classification ---

    fn valid_task_json() -> Vec<u8> {
        br#"{"object_name":"clip-1.mp4","status":"Prepared","id":"00000000-0000-0000-0000-000000000010","upload":{"station_id":"00000000-0000-0000-0000-000000000001","object_name":"clip-1.mp4"},"observation":null}"#.to_vec()
    }

    fn capped(status: u16, body: Vec<u8>) -> CappedResponse {
        CappedResponse { status: StatusCode::from_u16(status).unwrap(), retry_after: None, body }
    }

    #[test]
    fn classify_response_accepts_200() {
        let task = classify_response(&capped(200, valid_task_json()), "https://p/x").unwrap();
        assert_eq!(task.object_name, "clip-1.mp4");
    }

    #[test]
    fn classify_response_accepts_2xx_other() {
        // PS-22: a 201/202 from perchpub or its Traefik front is success,
        // not a `Http`→Terminal→Undeliverable drop.
        let task = classify_response(&capped(201, valid_task_json()), "https://p/x").unwrap();
        assert_eq!(task.status, ClassifyTaskStatus::Prepared);
    }

    #[test]
    fn classify_response_unknown_status_2xx_is_ok() {
        // PS-06: an unknown status string still yields a decoded task.
        let body = br#"{"object_name":"c.mp4","status":"Cancelled","id":"00000000-0000-0000-0000-000000000010","upload":{"station_id":"00000000-0000-0000-0000-000000000001","object_name":"c.mp4"},"observation":null}"#.to_vec();
        let task = classify_response(&capped(200, body), "https://p/x").unwrap();
        assert_eq!(task.status, ClassifyTaskStatus::Unknown);
    }

    #[test]
    fn classify_response_undecodable_2xx_is_undecodable_success() {
        // PS-06: a 2xx whose body genuinely won't parse means the clip is
        // already stored — surface `UndecodableSuccess`, never `Decode`
        // (which the runner would retry → re-upload an accepted clip).
        let err = classify_response(&capped(200, b"{ not json".to_vec()), "https://p/x")
            .expect_err("undecodable success");
        assert!(matches!(err, ClientError::UndecodableSuccess { .. }), "got {err:?}");
    }

    #[test]
    fn classify_response_non_2xx_is_http_preserving_retry_after() {
        let c = CappedResponse {
            status: StatusCode::SERVICE_UNAVAILABLE,
            retry_after: Some(Duration::from_secs(7)),
            body: b"down".to_vec(),
        };
        match classify_response(&c, "https://p/x") {
            Err(ClientError::Http { status, retry_after, .. }) => {
                assert_eq!(status, 503);
                assert_eq!(retry_after, Some(Duration::from_secs(7)));
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_capped_rejects_oversized_body() {
        // PS-16: a body over the cap must error out rather than buffer
        // unboundedly (OOM on a Pi).
        let big = vec![b'x'; usize::try_from(MAX_RESPONSE_BYTES).unwrap() + 1];
        let resp = reqwest::Response::from(http::Response::new(big));
        let err = read_capped(resp, ra_now(), "https://p/x").await.expect_err("over cap");
        assert!(matches!(err, ClientError::Decode { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn read_capped_returns_small_body() {
        let resp = reqwest::Response::from(http::Response::new(valid_task_json()));
        let capped = read_capped(resp, ra_now(), "https://p/x").await.unwrap();
        assert_eq!(capped.status, StatusCode::OK);
        assert_eq!(capped.body, valid_task_json());
    }
}
