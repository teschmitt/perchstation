//! Full-chain client identity (F5) over a *require-and-verify* mTLS edge.
//!
//! Closes a CI gap: the shared `fakepub` harness uses **optional** client auth
//! anchored at a single CA, so the leaf+intermediate identity that
//! `perchpub::client::build_inner` assembles is never exercised over a real
//! `RequireAndVerifyClientCert` handshake. perchpub's production `:8443` edge
//! does exactly that, against the **device CA** (contract §6).
//!
//! We stand up a minimal mTLS upload server whose client-cert verifier trusts
//! **only the root** of a `root → intermediate → leaf` device CA. The station
//! must therefore supply the intermediate itself. Three tests pin the
//! behaviour and prove the discrimination is real:
//!
//! 1. **positive** — `ca_chain.pem` = intermediate+root ⇒ the station presents
//!    leaf+intermediate ⇒ the verifier builds leaf→intermediate→root ⇒ the
//!    upload succeeds. *This is the F5 regression guard: a `build_inner` that
//!    fell back to leaf-only would fail it.*
//! 2. **negative** — `ca_chain.pem` = root only ⇒ the station presents
//!    leaf-only ⇒ the verifier cannot bridge leaf→root ⇒ the handshake fails.
//!    Confirms the verifier genuinely requires the full chain.
//! 3. **control** — a leaf issued *directly by the root* authenticates
//!    leaf-only against the same root-anchored verifier. This proves the
//!    server cert validates under root-only station trust and that a
//!    complete-to-anchor leaf-only chain is accepted — so test 2 fails
//!    specifically because the *intermediate* is absent from the presented
//!    chain, not because of any server-trust problem.

use std::io::BufReader;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{Router, extract::Multipart, response::Json, routing::post};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PKCS_ED25519,
};
use rustls::{RootCertStore, ServerConfig, pki_types::PrivateKeyDer, server::WebPkiClientVerifier};
use serde_json::json;
use uuid::Uuid;

use perchstation_core::identity::{CREDENTIALS_DIR, STATION_CERT_FILE, STATION_KEY_FILE};
use perchstation_core::perchpub::client::{ClientError, PerchpubClient};

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn cn(name: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, name);
    dn
}

/// A `root → intermediate` device CA. The intermediate signs the fake server's
/// TLS cert and the station's leaf, mirroring step-ca; the root is exposed too
/// so the control test can issue a leaf directly beneath it.
struct DeviceCa {
    root: Certificate,
    root_key: KeyPair,
    root_pem: String,
    intermediate: Certificate,
    intermediate_key: KeyPair,
    intermediate_pem: String,
}

fn build_device_ca() -> DeviceCa {
    let root_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
    let mut root_params = CertificateParams::new(vec!["device-root-ca".into()]).unwrap();
    root_params.distinguished_name = cn("device-root-ca");
    root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    root_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    root_params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    root_params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    let root = root_params.self_signed(&root_key).unwrap();
    let root_pem = root.pem();

    let intermediate_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
    let mut int_params = CertificateParams::new(vec!["device-intermediate-ca".into()]).unwrap();
    int_params.distinguished_name = cn("device-intermediate-ca");
    int_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    int_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    int_params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    int_params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    let intermediate = int_params.signed_by(&intermediate_key, &root, &root_key).unwrap();
    let intermediate_pem = intermediate.pem();

    DeviceCa { root, root_key, root_pem, intermediate, intermediate_key, intermediate_pem }
}

/// Mint an end-entity leaf signed by `ca`, returning `(cert_pem, key_pem)`.
fn leaf_signed_by(
    ca: &Certificate,
    ca_key: &KeyPair,
    common_name: &str,
    sans: &[&str],
    eku: ExtendedKeyUsagePurpose,
) -> (String, String) {
    let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
    let san_vec: Vec<String> = sans.iter().map(|s| (*s).to_string()).collect();
    let mut params = CertificateParams::new(san_vec).unwrap();
    params.distinguished_name = cn(common_name);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    params.extended_key_usages = vec![eku];
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    let cert = params.signed_by(&key, ca, ca_key).unwrap();
    (cert.pem(), key.serialize_pem())
}

fn server_cert(ca: &Certificate, ca_key: &KeyPair) -> (String, String) {
    leaf_signed_by(
        ca,
        ca_key,
        "127.0.0.1",
        &["127.0.0.1", "localhost"],
        ExtendedKeyUsagePurpose::ServerAuth,
    )
}

/// A running mTLS upload server; shuts down on drop.
struct MtlsUploadServer {
    url: String,
    handle: Handle,
}

impl Drop for MtlsUploadServer {
    fn drop(&mut self) {
        self.handle.graceful_shutdown(Some(Duration::from_millis(100)));
    }
}

/// `POST /api/v1/upload/` over mTLS, `RequireAndVerifyClientCert` against
/// `client_auth_root_pem`. The server presents `server_chain_pem` so a client
/// trusting only the root can still validate the server.
async fn start_mtls_upload_server(
    client_auth_root_pem: &str,
    server_chain_pem: &str,
    server_key_pem: &str,
) -> MtlsUploadServer {
    install_crypto_provider();

    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut BufReader::new(client_auth_root_pem.as_bytes())) {
        roots.add(cert.expect("parse client-auth root")).expect("add client-auth root");
    }
    // No `.allow_unauthenticated()` → RequireAndVerifyClientCert.
    let verifier =
        WebPkiClientVerifier::builder(Arc::new(roots)).build().expect("build client verifier");

    let server_certs: Vec<_> =
        rustls_pemfile::certs(&mut BufReader::new(server_chain_pem.as_bytes()))
            .collect::<Result<_, _>>()
            .expect("parse server chain");
    let server_key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(server_key_pem.as_bytes()))
            .expect("parse server key")
            .expect("server key present");

    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(server_certs, server_key)
        .expect("build server config");
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("local addr").port();
    let url = format!("https://127.0.0.1:{port}");

    let app = Router::new().route("/api/v1/upload/", post(handle_upload));
    let handle = Handle::new();
    let handle_for_task = handle.clone();
    let rustls_cfg = RustlsConfig::from_config(Arc::new(config));

    tokio::spawn(async move {
        if let Err(err) = axum_server::from_tcp_rustls(listener, rustls_cfg)
            .handle(handle_for_task)
            .serve(app.into_make_service())
            .await
        {
            eprintln!("mtls upload server ended: {err}");
        }
    });
    handle.listening().await.expect("server failed to start listening");

    MtlsUploadServer { url, handle }
}

/// Consume the multipart body and answer with a minimal valid
/// `ClassifyTaskPublic`. Reaching this handler at all means the mTLS
/// handshake (incl. client-cert verification) succeeded.
async fn handle_upload(mut multipart: Multipart) -> Json<serde_json::Value> {
    while let Ok(Some(field)) = multipart.next_field().await {
        let _ = field.bytes().await;
    }
    Json(json!({
        "object_name": "clip-1.mp4",
        "status": "Prepared",
        "id": Uuid::new_v4(),
        "upload": { "station_id": Uuid::nil(), "object_name": "clip-1.mp4" },
        "observation": null,
    }))
}

/// Write a station credential set: `station.crt` = `leaf_pem`, `station.key` =
/// `key_pem`, `ca_chain.pem` = `ca_chain_pem`.
fn write_station_credentials(data_dir: &Path, leaf_pem: &str, key_pem: &str, ca_chain_pem: &str) {
    let creds = data_dir.join(CREDENTIALS_DIR);
    std::fs::create_dir_all(&creds).unwrap();
    std::fs::write(creds.join(STATION_CERT_FILE), leaf_pem).unwrap();
    std::fs::write(creds.join(STATION_KEY_FILE), key_pem).unwrap();
    std::fs::write(creds.join("ca_chain.pem"), ca_chain_pem).unwrap();
    std::fs::write(creds.join("identity.json"), "{}").unwrap();
}

fn write_clip(dir: &Path) -> PathBuf {
    let path = dir.join("clip.mp4");
    std::fs::write(&path, b"\x00\x00\x00\x20ftypmp42").expect("write clip");
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_chain_identity_authenticates_against_require_verify_edge() {
    // F5/§6 regression guard. The edge requires + verifies a client cert,
    // trusting ONLY the root. ca_chain.pem carries intermediate + root, so the
    // station presents leaf + intermediate and the handshake must succeed. A
    // `build_inner` regressed to leaf-only would fail this test.
    let ca = build_device_ca();
    let (server_leaf, server_key) = server_cert(&ca.intermediate, &ca.intermediate_key);
    let server_chain = format!("{server_leaf}{}", ca.intermediate_pem);
    let server = start_mtls_upload_server(&ca.root_pem, &server_chain, &server_key).await;

    let dir = tempfile::tempdir().unwrap();
    let (leaf, key) = leaf_signed_by(
        &ca.intermediate,
        &ca.intermediate_key,
        "station-fullchain-test",
        &["station-fullchain-test"],
        ExtendedKeyUsagePurpose::ClientAuth,
    );
    write_station_credentials(
        dir.path(),
        &leaf,
        &key,
        &format!("{}{}", ca.intermediate_pem, ca.root_pem),
    );

    let client = PerchpubClient::new(dir.path(), &server.url).expect("build mTLS client");
    let clip = write_clip(dir.path());

    let task = client
        .upload_clip(&clip, "clip-1")
        .await
        .expect("leaf+intermediate identity must authenticate against a require-verify edge");
    assert_eq!(task.object_name, "clip-1.mp4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaf_only_identity_is_rejected_by_require_verify_edge() {
    // With ca_chain.pem = root only, `build_inner` finds no issuing
    // intermediate, so the station presents leaf-only. The verifier (anchored
    // at the root, no intermediate to bridge) rejects the client cert and the
    // handshake fails. The server cert still validates (the station trusts the
    // root and the server presents its intermediate — see the control test),
    // so the failure is the *client* certificate.
    let ca = build_device_ca();
    let (server_leaf, server_key) = server_cert(&ca.intermediate, &ca.intermediate_key);
    let server_chain = format!("{server_leaf}{}", ca.intermediate_pem);
    let server = start_mtls_upload_server(&ca.root_pem, &server_chain, &server_key).await;

    let dir = tempfile::tempdir().unwrap();
    let (leaf, key) = leaf_signed_by(
        &ca.intermediate,
        &ca.intermediate_key,
        "station-fullchain-test",
        &["station-fullchain-test"],
        ExtendedKeyUsagePurpose::ClientAuth,
    );
    write_station_credentials(dir.path(), &leaf, &key, &ca.root_pem); // root only — no intermediate

    let client = PerchpubClient::new(dir.path(), &server.url).expect("build mTLS client");
    let clip = write_clip(dir.path());

    let err = client
        .upload_clip(&clip, "clip-1")
        .await
        .expect_err("leaf-only must fail client-cert verification at a require-verify edge");
    assert!(
        matches!(err, ClientError::Network { .. }),
        "expected a handshake/transport failure (Network), got {err:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaf_issued_directly_by_the_anchor_authenticates_leaf_only() {
    // Control isolating WHY the negative test fails. Here the station leaf and
    // the server cert are signed DIRECTLY by the root, and the verifier is
    // anchored at that root. The station still presents leaf-only (a
    // self-signed root issuer is never appended), but now leaf→root needs no
    // intermediate → the handshake SUCCEEDS. This proves that under root-only
    // station trust the server cert validates and a complete-to-anchor
    // leaf-only chain is accepted — so the negative test fails specifically
    // because the *intermediate* is missing, the exact F5 gap this suite guards.
    let ca = build_device_ca();
    let (server_leaf, server_key) = server_cert(&ca.root, &ca.root_key);
    let server = start_mtls_upload_server(&ca.root_pem, &server_leaf, &server_key).await;

    let dir = tempfile::tempdir().unwrap();
    let (leaf, key) = leaf_signed_by(
        &ca.root,
        &ca.root_key,
        "station-under-root",
        &["station-under-root"],
        ExtendedKeyUsagePurpose::ClientAuth,
    );
    write_station_credentials(dir.path(), &leaf, &key, &ca.root_pem);

    let client = PerchpubClient::new(dir.path(), &server.url).expect("build mTLS client");
    let clip = write_clip(dir.path());

    let task = client
        .upload_clip(&clip, "clip-1")
        .await
        .expect("a leaf issued directly by the trusted root authenticates leaf-only");
    assert_eq!(task.object_name, "clip-1.mp4");
}
