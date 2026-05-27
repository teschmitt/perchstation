//! Axum-server perchpub double.
//!
//! Each integration test typically does `let pub_ = FakePerchpub::start().await;`
//! to spin up an isolated HTTPS server with its own CA, server cert, and
//! recorded request state.
//!
//! The double covers the three station-to-perchpub endpoints described in
//! `specs/001-clip-delivery/contracts/perchpub-api.md`:
//!
//! - `POST /api/v1/enrollment/confirm/{session_id}` — signs the CSR with
//!   the internal CA and returns an `EnrollmentResponse`. Response mode is
//!   settable so tests can drive the 422/session-expired branch.
//! - `POST /api/v1/upload/` — accepts a multipart upload and returns a
//!   `ClassifyTaskPublic` with `status = Prepared`.
//! - `GET /api/v1/classify-task/{id}` — returns the stored task; first
//!   poll yields the recorded status (typically `Prepared`), second-plus
//!   poll yields `Success`.
//!
//! TLS uses optional mTLS — the `/enrollment/confirm` flow presents no
//! client cert, while `/upload/` and `/classify-task` present the
//! station's enrollment-issued cert.

use std::collections::HashMap;
use std::io::BufReader;
use std::net::TcpListener as StdTcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{Certificate, CertificateSigningRequestParams, KeyPair};
use rustls::{RootCertStore, ServerConfig, pki_types::PrivateKeyDer, server::WebPkiClientVerifier};
use serde_json::json;
use uuid::Uuid;

use perchstation_core::perchpub::types::{
    ClassifyTaskPublic, ClassifyTaskStatus, EnrollmentRequest, EnrollmentResponse, UploadPublic,
};

use super::fixtures;

/// Snapshot of everything the fake perchpub observed during a test.
#[derive(Default, Debug, Clone)]
pub struct Recorded {
    pub enrollment_requests: Vec<RecordedEnrollment>,
    pub upload_requests: Vec<RecordedUpload>,
    pub classify_polls: Vec<Uuid>,
}

#[derive(Debug, Clone)]
pub struct RecordedEnrollment {
    pub session_id: String,
    pub auth_token: String,
    pub csr_pem: String,
}

#[derive(Debug, Clone)]
pub struct RecordedUpload {
    pub byte_size: usize,
    pub filename: Option<String>,
    pub content_type: Option<String>,
}

/// How the enrollment-confirm endpoint should respond on the next call.
/// Tests flip this to exercise the 422 branch.
#[derive(Debug, Clone, Copy)]
pub enum EnrollmentResponseMode {
    /// Sign the CSR, return success.
    Ok,
    /// Return 422 with an `HTTPValidationError` body. Mirrors the
    /// "session expired" branch in `contracts/perchpub-api.md` §1.
    SessionExpired,
}

struct FakeState {
    ca_cert: Certificate,
    ca_key: KeyPair,
    ca_pem: String,
    recorded: Mutex<Recorded>,
    /// Stored classify-tasks keyed by id. Each test can pre-seed entries
    /// via `with_classify_task`, or rely on the upload endpoint to mint
    /// one.
    tasks: Mutex<HashMap<Uuid, ClassifyTaskPublic>>,
    /// Per-task poll count; the second-plus poll flips status to Success.
    poll_counts: Mutex<HashMap<Uuid, u32>>,
    enrollment_mode: Mutex<EnrollmentResponseMode>,
}

pub struct FakePerchpub {
    url: String,
    ca_pem: String,
    state: Arc<FakeState>,
    handle: Handle,
}

impl Drop for FakePerchpub {
    fn drop(&mut self) {
        self.handle.graceful_shutdown(Some(Duration::from_millis(100)));
    }
}

impl FakePerchpub {
    /// Spin up an isolated fake perchpub. Binds to `127.0.0.1:0`; the
    /// allocated port is reflected in `url()`.
    pub async fn start() -> Self {
        fixtures::install_crypto_provider();

        let (ca_cert, ca_key, ca_pem) = fixtures::build_test_ca();
        let (server_cert_pem, server_key_pem) =
            fixtures::build_server_cert(&ca_cert, &ca_key, &["127.0.0.1", "localhost"]);

        let tls_config = build_tls_config(&ca_pem, &server_cert_pem, &server_key_pem);

        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.set_nonblocking(true).expect("nonblocking");
        let local_addr = listener.local_addr().expect("local addr");
        let url = format!("https://127.0.0.1:{}", local_addr.port());

        let state = Arc::new(FakeState {
            ca_cert,
            ca_key,
            ca_pem: ca_pem.clone(),
            recorded: Mutex::new(Recorded::default()),
            tasks: Mutex::new(HashMap::new()),
            poll_counts: Mutex::new(HashMap::new()),
            enrollment_mode: Mutex::new(EnrollmentResponseMode::Ok),
        });

        let app = Router::new()
            .route("/api/v1/enrollment/confirm/:session_id", post(handle_enrollment_confirm))
            .route("/api/v1/upload/", post(handle_upload))
            .route("/api/v1/classify-task/:id", get(handle_classify_get))
            .with_state(state.clone());

        let handle = Handle::new();
        let handle_for_task = handle.clone();
        let rustls_cfg = RustlsConfig::from_config(tls_config);

        tokio::spawn(async move {
            // `from_tcp_rustls` consumes the std listener and serves until
            // the handle is signalled.
            if let Err(err) = axum_server::from_tcp_rustls(listener, rustls_cfg)
                .handle(handle_for_task)
                .serve(app.into_make_service())
                .await
            {
                eprintln!("fake perchpub serve loop ended: {err}");
            }
        });

        // Wait until axum-server reports a bound listener before handing
        // the URL back to the test. Avoids a connect-before-listen race.
        let _addr = handle.listening().await.expect("fake perchpub failed to start listening");

        Self { url, ca_pem, state, handle }
    }

    /// `https://127.0.0.1:<port>` — the value tests stamp into the
    /// station's `config.toml::perchpub_url`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Single-cert PEM chain. Used both as the QR's `ca_chain_pem` and as
    /// the on-disk `credentials/ca_chain.pem` for delivery-side tests.
    #[must_use]
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// Mint a station leaf cert from a station-held public key, signed by
    /// the fake perchpub's CA. Used by tests like `delivery_happy` that
    /// stand up pre-enrolled credentials without going through the
    /// enrollment exchange.
    pub fn mint_station_cert(&self, station_key: &KeyPair, station_id: Uuid) -> String {
        fixtures::build_station_cert(
            station_key,
            station_id,
            &self.state.ca_cert,
            &self.state.ca_key,
        )
    }

    /// Flip the enrollment-confirm endpoint's response mode for the next
    /// (and subsequent) request.
    pub fn set_enrollment_response(&self, mode: EnrollmentResponseMode) {
        *self.state.enrollment_mode.lock().unwrap() = mode;
    }

    /// Clone-out the recorded request state for assertions.
    #[must_use]
    pub fn recorded(&self) -> Recorded {
        self.state.recorded.lock().unwrap().clone()
    }
}

fn build_tls_config(
    ca_pem: &str,
    server_cert_pem: &str,
    server_key_pem: &str,
) -> Arc<ServerConfig> {
    // Trust anchors for client cert verification: just the fake CA.
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut BufReader::new(ca_pem.as_bytes())) {
        let cert = cert.expect("parse CA cert");
        roots.add(cert).expect("add CA cert");
    }

    // Optional mTLS — confirm endpoint hits us with no cert; upload /
    // classify-task hit us with the station's cert.
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .expect("build client verifier");

    // Server presentation cert.
    let server_certs: Vec<_> =
        rustls_pemfile::certs(&mut BufReader::new(server_cert_pem.as_bytes()))
            .collect::<Result<_, _>>()
            .expect("parse server cert");
    let server_key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(server_key_pem.as_bytes()))
            .expect("parse server key")
            .expect("server key present");

    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(server_certs, server_key)
        .expect("build server config");
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

async fn handle_enrollment_confirm(
    State(state): State<Arc<FakeState>>,
    Path(session_id): Path<String>,
    Json(body): Json<EnrollmentRequest>,
) -> Result<Json<EnrollmentResponse>, (StatusCode, Json<serde_json::Value>)> {
    state.recorded.lock().unwrap().enrollment_requests.push(RecordedEnrollment {
        session_id: session_id.clone(),
        auth_token: body.auth_token.clone(),
        csr_pem: body.csr_pem.clone(),
    });

    let mode = *state.enrollment_mode.lock().unwrap();
    match mode {
        EnrollmentResponseMode::SessionExpired => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "detail": [
                        {
                            "loc": ["path", "session_id"],
                            "msg": "session expired or already used",
                            "type": "value_error",
                        }
                    ],
                })),
            ));
        }
        EnrollmentResponseMode::Ok => {}
    }

    let csr_params = match CertificateSigningRequestParams::from_pem(&body.csr_pem) {
        Ok(p) => p,
        Err(err) => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "detail": [
                        {
                            "loc": ["body", "csr_pem"],
                            "msg": format!("invalid CSR: {err}"),
                            "type": "value_error",
                        }
                    ],
                })),
            ));
        }
    };

    let leaf = csr_params.signed_by(&state.ca_cert, &state.ca_key).expect("sign CSR with test CA");
    let station_id = Uuid::new_v4();

    Ok(Json(EnrollmentResponse {
        success: true,
        reason: String::new(),
        certificate_pem: Some(leaf.pem()),
        ca_chain_pem: Some(state.ca_pem.clone()),
        station_id: Some(station_id),
    }))
}

async fn handle_upload(
    State(state): State<Arc<FakeState>>,
    mut multipart: Multipart,
) -> Result<Json<ClassifyTaskPublic>, (StatusCode, String)> {
    let mut byte_size = 0_usize;
    let mut filename = None;
    let mut content_type = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("multipart: {err}")))?
    {
        if field.name() == Some("file") {
            filename = field.file_name().map(str::to_string);
            content_type = field.content_type().map(str::to_string);
            let bytes = field
                .bytes()
                .await
                .map_err(|err| (StatusCode::BAD_REQUEST, format!("read part: {err}")))?;
            byte_size = bytes.len();
        }
    }

    state.recorded.lock().unwrap().upload_requests.push(RecordedUpload {
        byte_size,
        filename: filename.clone(),
        content_type,
    });

    let task_id = Uuid::new_v4();
    let object_name = filename.unwrap_or_else(|| format!("clip-{task_id}.mp4"));
    let task = ClassifyTaskPublic {
        object_name: object_name.clone(),
        status: ClassifyTaskStatus::Prepared,
        id: task_id,
        upload: UploadPublic {
            station_id: Uuid::nil(),
            object_name,
            id: Some(Uuid::new_v4()),
            created_at: Some(chrono::Utc::now()),
            updated_at: Some(chrono::Utc::now()),
        },
        observation: None,
    };
    state.tasks.lock().unwrap().insert(task_id, task.clone());

    Ok(Json(task))
}

async fn handle_classify_get(
    State(state): State<Arc<FakeState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<ClassifyTaskPublic>, StatusCode> {
    state.recorded.lock().unwrap().classify_polls.push(id);

    let count = {
        let mut counts = state.poll_counts.lock().unwrap();
        let entry = counts.entry(id).or_insert(0);
        *entry += 1;
        *entry
    };

    let mut tasks = state.tasks.lock().unwrap();
    let task = tasks.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
    // First poll: leave status as recorded (typically Prepared).
    // Second-plus poll: flip to Success so the loop reaches a terminal
    // status promptly. Matches the "default flips on second poll" line in
    // the handoff doc.
    if count >= 2 {
        task.status = ClassifyTaskStatus::Success;
    }
    Ok(Json(task.clone()))
}

/// Convert an `IntoResponse` into the JSON form. Helper exists to keep
/// the route handler signatures terse.
#[allow(dead_code)]
fn validation_error(field: &str, message: &str) -> impl IntoResponse {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({
            "detail": [
                {"loc": ["body", field], "msg": message, "type": "value_error"},
            ],
        })),
    )
}
