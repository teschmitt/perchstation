//! Primitive helpers used across integration tests.
//!
//! No test depends on a pre-committed binary blob — every fixture is
//! generated at test setup time. That keeps the repo small, makes the
//! shape of each test obvious, and avoids regenerating PEMs by hand when
//! algorithms change.

use std::fs;
use std::io::{Cursor, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use chrono::Utc;
use image::{ImageBuffer, ImageFormat, Luma};
use qrcode::QrCode;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, IsCa, KeyPair, KeyUsagePurpose, PKCS_ED25519,
};
use serde_json::json;
use uuid::Uuid;

/// Synthetic MP4-shaped payload used as a stand-in for a captured clip in
/// tests that don't care about the bytes. Starts with a valid `ftyp` atom
/// so anything that briefly sniffs the prefix sees the right magic; the
/// fake perchpub doesn't decode the bytes regardless.
#[must_use]
pub fn sample_mp4_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4096);
    // box length = 0x20 (32 bytes)
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x20]);
    // type = "ftyp"
    bytes.extend_from_slice(b"ftyp");
    // major brand = "mp42"
    bytes.extend_from_slice(b"mp42");
    // minor version = 0
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    // compatible brands = mp42 isom avc1 (3 brands * 4 bytes = 12 bytes)
    bytes.extend_from_slice(b"mp42isomavc1");
    // pad out to 4096 bytes with zeros.
    bytes.resize(4096, 0);
    bytes
}

/// Build a self-signed Ed25519 CA suitable for both signing server certs
/// (so the station can validate perchpub's TLS) and signing station leaf
/// certs (so perchpub can verify client cert presentations).
///
/// Returns `(ca_certificate, ca_keypair, ca_pem_chain)`. The PEM is the
/// single-cert chain the station would persist as `credentials/ca_chain.pem`.
pub fn build_test_ca() -> (Certificate, KeyPair, String) {
    let key = KeyPair::generate_for(&PKCS_ED25519).expect("generate ed25519 CA keypair");
    let mut params =
        CertificateParams::new(vec!["perchstation-test-ca".into()]).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(2));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    let cert = params.self_signed(&key).expect("self-sign test CA");
    let pem = cert.pem();
    (cert, key, pem)
}

/// Mint a server leaf cert signed by the given CA, valid for the supplied
/// SANs. Used to give the fake perchpub a TLS identity that the station
/// can chain back to its CA pin.
///
/// Returns `(server_cert_pem, server_key_pem)`.
pub fn build_server_cert(ca: &Certificate, ca_key: &KeyPair, sans: &[&str]) -> (String, String) {
    let key = KeyPair::generate_for(&PKCS_ED25519).expect("generate ed25519 server keypair");
    let san_vec: Vec<String> = sans.iter().map(|s| (*s).to_string()).collect();
    let mut params = CertificateParams::new(san_vec).expect("server params");
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    let cert = params.signed_by(&key, ca, ca_key).expect("sign server cert");
    (cert.pem(), key.serialize_pem())
}

/// Mint a station leaf cert from an externally-held station keypair.
/// Used by `write_test_credentials` (delivery-side tests synthesise
/// credentials directly without going through the enrollment flow).
pub fn build_station_cert(
    station_key: &KeyPair,
    station_id: Uuid,
    ca: &Certificate,
    ca_key: &KeyPair,
) -> String {
    build_station_cert_with_validity(
        station_key,
        station_id,
        ca,
        ca_key,
        (2026, 1, 1),
        (2099, 1, 1),
    )
}

/// Mint a station leaf cert with caller-controlled validity dates. T053
/// uses this to mint an already-expired cert and exercise the `status`
/// `enrollment.state = "expired"` branch.
pub fn build_station_cert_with_validity(
    station_key: &KeyPair,
    station_id: Uuid,
    ca: &Certificate,
    ca_key: &KeyPair,
    not_before_ymd: (i32, u8, u8),
    not_after_ymd: (i32, u8, u8),
) -> String {
    let mut params =
        CertificateParams::new(vec![format!("station-{station_id}")]).expect("station params");
    params.not_before = rcgen::date_time_ymd(not_before_ymd.0, not_before_ymd.1, not_before_ymd.2);
    params.not_after = rcgen::date_time_ymd(not_after_ymd.0, not_after_ymd.1, not_after_ymd.2);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    let cert = params.signed_by(station_key, ca, ca_key).expect("sign station cert");
    cert.pem()
}

/// Render a PNG containing a QR code that encodes the enrollment session
/// JSON payload (perchpub `EnrollmentSession` shape + the
/// `ca_chain_pem` extension the station needs to bootstrap TLS for the
/// `/enrollment/confirm` call — documented in
/// `specs/001-clip-delivery/data-model.md` §QR payload format).
pub fn build_qr_png(session_id: Uuid, auth_token: &str, ca_chain_pem: &str) -> Vec<u8> {
    let payload = json!({
        "session_id": session_id,
        "auth_token": auth_token,
        "ca_chain_pem": ca_chain_pem,
    });
    let payload_bytes = serde_json::to_vec(&payload).expect("serialise QR payload");

    let code = QrCode::new(&payload_bytes).expect("build QR code");
    let image: ImageBuffer<Luma<u8>, Vec<u8>> =
        code.render::<Luma<u8>>().min_dimensions(400, 400).quiet_zone(true).build();

    let mut png_bytes = Vec::new();
    image.write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png).expect("encode PNG");
    png_bytes
}

/// Write a complete `<data_dir>/credentials/` directory that mirrors what
/// the enrollment flow would persist. Used by delivery-side tests
/// (`delivery_happy`, etc.) that need a pre-enrolled station.
///
/// Matches the file layout in `crates/perchstation-core/src/identity.rs`:
/// `identity.json`, `station.crt`, `station.key` (mode `0600`), and
/// `ca_chain.pem`.
pub fn write_test_credentials(
    data_dir: &Path,
    station_id: Uuid,
    perchpub_url: &str,
    station_key_pem: &str,
    station_cert_pem: &str,
    ca_chain_pem: &str,
) -> std::io::Result<()> {
    let creds = data_dir.join("credentials");
    fs::create_dir_all(&creds)?;

    let identity = json!({
        "station_id": station_id,
        "enrolled_at": Utc::now(),
        "perchpub_url": perchpub_url,
    });
    fs::write(creds.join("identity.json"), serde_json::to_vec_pretty(&identity).unwrap())?;
    fs::write(creds.join("station.crt"), station_cert_pem)?;
    fs::write(creds.join("ca_chain.pem"), ca_chain_pem)?;

    let mut key_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(creds.join("station.key"))?;
    key_file.write_all(station_key_pem.as_bytes())?;
    Ok(())
}

/// Build a fresh Ed25519 keypair for a station. Tests that don't drive
/// the enrollment flow use this to mint a station identity offline.
#[must_use]
pub fn build_station_keypair() -> KeyPair {
    KeyPair::generate_for(&PKCS_ED25519).expect("generate ed25519 station keypair")
}

/// Install the ring crypto provider for rustls. Idempotent. Each test
/// binary calls this once before constructing TLS configs.
pub fn install_crypto_provider() {
    // `install_default` returns Err if a provider is already installed,
    // which is the expected case on the second-plus call.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
