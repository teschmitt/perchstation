//! Dev-only fake perchpub for quickstart smoke tests.
//!
//! This binary mirrors the happy-path behaviour of
//! `tests/integration/support/fakepub.rs` (signs CSRs from
//! `/enrollment/confirm`, accepts multipart uploads, returns
//! `ClassifyTaskPublic` from `/classify-task/{id}`) but loads its TLS
//! material from on-disk PEM files rather than minting fresh certs in
//! memory. It is wired into the workspace via `quickstart.md` §2 and is
//! **excluded from release artefacts** — the production CI workflow only
//! ships the `perchstation` binary.
//!
//! Differences from the integration-test fake:
//!  - No per-test failure knobs (no transient 503 budget, no rate-limit
//!    mode, no 422 override). Quickstart only exercises the happy path.
//!  - PEMs come from the filesystem, so the operator can pair this with
//!    a QR PNG that embeds the same CA chain.

use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use chrono::Utc;
use clap::Parser;
use rcgen::{Certificate, CertificateParams, CertificateSigningRequestParams, KeyPair};
use rustls::{RootCertStore, ServerConfig, pki_types::PrivateKeyDer, server::WebPkiClientVerifier};
use serde_json::json;
use uuid::Uuid;

use perchstation_core::perchpub::types::{
    ClassifyTaskPublic, ClassifyTaskStatus, EnrollmentRequest, EnrollmentResponse, UploadPublic,
};

#[derive(Parser, Debug)]
#[command(
    name = "fakepub",
    about = "Dev-only fake perchpub: serves the three station-facing endpoints over TLS.",
    version
)]
struct Args {
    /// `host:port` to bind. Use `127.0.0.1:8443` for the quickstart.
    #[arg(long)]
    listen: SocketAddr,

    /// PEM file with the server's TLS leaf certificate (chained to `--ca`).
    #[arg(long, value_name = "PATH")]
    tls_cert: PathBuf,

    /// PEM file with the server's TLS private key.
    #[arg(long, value_name = "PATH")]
    tls_key: PathBuf,

    /// PEM file with the CA certificate. Used both as the trust anchor
    /// for verifying station client certs and as the `ca_chain_pem` field
    /// returned in `EnrollmentResponse`.
    #[arg(long, value_name = "PATH")]
    ca: PathBuf,

    /// PEM file with the CA private key. Required so the server can sign
    /// CSRs presented to `/api/v1/enrollment/confirm/{session_id}`.
    #[arg(long, value_name = "PATH")]
    ca_key: PathBuf,
}

struct FakeState {
    ca_cert: Certificate,
    ca_key: KeyPair,
    ca_pem: String,
    tasks: Mutex<HashMap<Uuid, ClassifyTaskPublic>>,
    poll_counts: Mutex<HashMap<Uuid, u32>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    install_crypto_provider();

    let ca_pem =
        fs::read_to_string(&args.ca).with_context(|| format!("read --ca {}", args.ca.display()))?;
    let ca_key_pem = fs::read_to_string(&args.ca_key)
        .with_context(|| format!("read --ca-key {}", args.ca_key.display()))?;
    let ca_key = KeyPair::from_pem(&ca_key_pem).context("parse --ca-key PEM")?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca_pem).context("parse --ca PEM")?;
    let ca_cert = ca_params.self_signed(&ca_key).context("rebuild CA certificate")?;

    let server_cert_pem = fs::read_to_string(&args.tls_cert)
        .with_context(|| format!("read --tls-cert {}", args.tls_cert.display()))?;
    let server_key_pem = fs::read_to_string(&args.tls_key)
        .with_context(|| format!("read --tls-key {}", args.tls_key.display()))?;

    let tls_config = build_tls_config(&ca_pem, &server_cert_pem, &server_key_pem)
        .context("build server TLS config")?;

    let state = Arc::new(FakeState {
        ca_cert,
        ca_key,
        ca_pem: ca_pem.clone(),
        tasks: Mutex::new(HashMap::new()),
        poll_counts: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/api/v1/enrollment/confirm/:session_id", post(handle_enrollment_confirm))
        .route("/api/v1/upload/", post(handle_upload))
        .route("/api/v1/classify-task/:id", get(handle_classify_get))
        .with_state(state);

    eprintln!("fakepub: listening on https://{}", args.listen);
    axum_server::bind_rustls(args.listen, RustlsConfig::from_config(tls_config))
        .serve(app.into_make_service())
        .await
        .context("axum server loop")?;

    Ok(())
}

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn build_tls_config(
    ca_pem: &str,
    server_cert_pem: &str,
    server_key_pem: &str,
) -> Result<Arc<ServerConfig>> {
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut BufReader::new(ca_pem.as_bytes())) {
        let cert = cert.context("parse CA certificate entry")?;
        roots.add(cert).context("install CA into trust store")?;
    }

    // Enrollment hits us with no client cert; upload + classify-task hit
    // us with the station's enrollment-issued cert. `allow_unauthenticated`
    // means both are accepted at the TLS layer — handlers do not consume
    // the peer identity for the happy path the quickstart exercises.
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .context("build client verifier")?;

    let server_certs: Vec<_> =
        rustls_pemfile::certs(&mut BufReader::new(server_cert_pem.as_bytes()))
            .collect::<Result<_, _>>()
            .context("parse server certificate")?;
    let server_key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(server_key_pem.as_bytes()))
            .context("parse server private key")?
            .context("server key file contained no PEM private key")?;

    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(server_certs, server_key)
        .context("assemble ServerConfig")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

async fn handle_enrollment_confirm(
    State(state): State<Arc<FakeState>>,
    Path(_session_id): Path<String>,
    Json(body): Json<EnrollmentRequest>,
) -> Result<Json<EnrollmentResponse>, (StatusCode, Json<serde_json::Value>)> {
    let csr_params = CertificateSigningRequestParams::from_pem(&body.csr_pem).map_err(|err| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "detail": [{
                    "loc": ["body", "csr_pem"],
                    "msg": format!("invalid CSR: {err}"),
                    "type": "value_error",
                }],
            })),
        )
    })?;

    let leaf = csr_params.signed_by(&state.ca_cert, &state.ca_key).map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "detail": [{
                    "loc": ["body", "csr_pem"],
                    "msg": format!("sign CSR: {err}"),
                    "type": "internal_error",
                }],
            })),
        )
    })?;

    Ok(Json(EnrollmentResponse {
        success: true,
        reason: String::new(),
        certificate_pem: Some(leaf.pem()),
        ca_chain_pem: Some(state.ca_pem.clone()),
        station_id: Some(Uuid::new_v4()),
    }))
}

async fn handle_upload(State(state): State<Arc<FakeState>>, mut multipart: Multipart) -> Response {
    let mut filename: Option<String> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(err) => {
                return (StatusCode::BAD_REQUEST, format!("multipart: {err}")).into_response();
            }
        };
        if field.name() == Some("file") {
            filename = field.file_name().map(str::to_string);
            // Drain the part so the connection completes; we discard the
            // bytes because the quickstart only checks that the upload
            // succeeded, not the body.
            if let Err(err) = field.bytes().await {
                return (StatusCode::BAD_REQUEST, format!("read part: {err}")).into_response();
            }
        }
    }

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
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        },
        observation: None,
    };
    state.tasks.lock().unwrap().insert(task_id, task.clone());

    Json(task).into_response()
}

async fn handle_classify_get(
    State(state): State<Arc<FakeState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<ClassifyTaskPublic>, StatusCode> {
    let count = {
        let mut counts = state.poll_counts.lock().unwrap();
        let entry = counts.entry(id).or_insert(0);
        *entry += 1;
        *entry
    };

    let mut tasks = state.tasks.lock().unwrap();
    let task = tasks.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
    // First poll: Prepared (the default minted at upload time).
    // Second-plus poll: flip to Success so the station's classify loop
    // promptly observes a terminal status. Matches the test fake.
    if count >= 2 {
        task.status = ClassifyTaskStatus::Success;
    }
    Ok(Json(task.clone()))
}
