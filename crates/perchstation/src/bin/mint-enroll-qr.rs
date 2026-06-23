//! `mint-enroll-qr` — dev-only helper that renders an enrollment QR PNG.
//!
//! perchstation's `enroll` only *consumes* a QR (camera or PNG); it never
//! produces one. perchpub's `POST /api/v1/enrollment/create` returns the
//! session id and auth token but **not** the CA chain the station must pin,
//! and no perchpub API emits the QR image (the web UI does that). On a host
//! with no camera and no perchpub frontend this helper bridges the gap: feed
//! it the `/enrollment/create` JSON response plus the perchpub CA-chain PEM
//! and it writes a QR PNG that `enroll --qr-source file` will accept.
//!
//! The QR payload mirrors `EnrollmentSessionMaterial`
//! (`perchstation_core::enrollment`) — `{ session_id, auth_token,
//! ca_chain_pem }` — rendered with the same `qrcode` + `image` recipe the
//! integration-test fixtures use, so the bytes decode through the real
//! `decode_enrollment_session` path. Dev-only; not shipped in releases.

use std::io::{Cursor, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use clap::Parser;
use image::{ImageBuffer, ImageFormat, Luma};
use qrcode::{EcLevel, QrCode};
use uuid::Uuid;

/// Render an enrollment QR PNG that `enroll --qr-source file` will accept.
#[derive(Debug, Parser)]
#[command(
    name = "mint-enroll-qr",
    about = "Dev helper: render an enrollment QR PNG from a perchpub /enrollment/create response + CA chain."
)]
struct Args {
    /// JSON body returned by `POST /api/v1/enrollment/create` (needs
    /// `session_id` and `auth_token`). Use `-` to read it from stdin.
    #[arg(long, value_name = "FILE")]
    create_response: String,

    /// Perchpub **device CA** chain PEM file: the CA that signs issued station
    /// certificates (intermediate [+ root]). Device-CA-only — it does **not**
    /// carry the public/Let's Encrypt `:443` edge intermediate, because the
    /// station validates that edge against system trust (device-cert contract
    /// §7). The station uses this chain to verify the issued leaf and as the
    /// upload server-trust additive anchor.
    #[arg(long, value_name = "FILE")]
    ca_chain: PathBuf,

    /// Output PNG path. Use `-` to write the PNG bytes to stdout.
    #[arg(long, value_name = "FILE")]
    out: String,
}

/// Render the enrollment QR PNG bytes for the given session material.
///
/// EC level `L` maximises capacity (~2953 bytes) because the PNG is decoded
/// from a clean file, not a noisy camera frame; a multi-cert device-CA chain
/// can still approach that ceiling (now smaller — the public/edge intermediate
/// is no longer carried, per device-cert contract §7).
fn render_enrollment_qr_png(
    session_id: Uuid,
    auth_token: &str,
    ca_chain_pem: &str,
) -> anyhow::Result<Vec<u8>> {
    if ca_chain_pem.trim().is_empty() {
        return Err(anyhow!("CA chain PEM is empty; `enroll` would reject the QR"));
    }
    let payload = serde_json::json!({
        "session_id": session_id,
        "auth_token": auth_token,
        "ca_chain_pem": ca_chain_pem,
    });
    let payload_bytes = serde_json::to_vec(&payload)?;

    let code = QrCode::with_error_correction_level(&payload_bytes, EcLevel::L).map_err(|e| {
        anyhow!(
            "payload is {} bytes — too large for a single QR code (max ~2953 at EC level L): {e}. \
             Use a shorter CA chain (the issuing CA certificate alone, not the full bundle).",
            payload_bytes.len()
        )
    })?;
    let image: ImageBuffer<Luma<u8>, Vec<u8>> =
        code.render::<Luma<u8>>().min_dimensions(600, 600).quiet_zone(true).build();

    let mut png_bytes = Vec::new();
    image.write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)?;
    Ok(png_bytes)
}

/// Fields the helper needs out of the `/enrollment/create` response. Extra
/// fields (e.g. `expires_at`) are ignored.
#[derive(Debug, serde::Deserialize)]
struct CreateResponse {
    session_id: Uuid,
    auth_token: String,
}

/// Extract `(session_id, auth_token)` from a `/enrollment/create` JSON body.
/// Extra fields such as `expires_at` are ignored.
fn parse_create_response(json: &str) -> anyhow::Result<(Uuid, String)> {
    // A failed create returns a perchpub error envelope (`{"detail": ...}`)
    // instead of a session — e.g. an expired/invalid bearer token yields
    // `{"detail":"Could not validate credentials"}`. Surface that legibly
    // rather than the cryptic serde "missing field `session_id`".
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(json)
        && let Some(detail) = map.get("detail")
        && !map.contains_key("session_id")
    {
        return Err(anyhow!(
            "the /enrollment/create call did not return a session — perchpub responded with an \
             error: {detail}. The usual cause is an expired or invalid bearer token; refresh it \
             and retry."
        ));
    }

    let parsed: CreateResponse = serde_json::from_str(json)
        .map_err(|e| anyhow!("parse /enrollment/create JSON response: {e}"))?;
    if parsed.auth_token.is_empty() {
        return Err(anyhow!("`auth_token` in the /enrollment/create response is empty"));
    }
    Ok((parsed.session_id, parsed.auth_token))
}

fn read_text_input(path: &str) -> anyhow::Result<String> {
    if path == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).context("read /enrollment/create from stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("read {path}"))
    }
}

fn write_png_output(path: &str, bytes: &[u8]) -> anyhow::Result<()> {
    if path == "-" {
        std::io::stdout().write_all(bytes).context("write PNG to stdout")
    } else {
        std::fs::write(path, bytes).with_context(|| format!("write {path}"))
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let create_json = read_text_input(&args.create_response)?;
    let (session_id, auth_token) = parse_create_response(&create_json)?;
    let ca_chain_pem = std::fs::read_to_string(&args.ca_chain)
        .with_context(|| format!("read {}", args.ca_chain.display()))?;

    let png = render_enrollment_qr_png(session_id, &auth_token, &ca_chain_pem)?;
    write_png_output(&args.out, &png)?;

    if args.out != "-" {
        eprintln!(
            "wrote enrollment QR for session {session_id} to {} ({} bytes)",
            args.out,
            png.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use perchstation_core::enrollment::decode_enrollment_session;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, PKCS_ED25519};

    /// Two self-signed CA certs concatenated, to mirror a root+intermediate
    /// bundle and stress QR capacity the way a real perchpub chain would.
    fn realistic_ca_chain_pem() -> String {
        let mint = || {
            let key = KeyPair::generate_for(&PKCS_ED25519).expect("ed25519 key");
            let mut params =
                CertificateParams::new(vec!["perchpub-device-ca".to_string()]).expect("ca params");
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.self_signed(&key).expect("self-sign").pem()
        };
        format!("{}{}", mint(), mint())
    }

    #[test]
    fn rendered_qr_round_trips_through_the_real_decoder() {
        let session_id = Uuid::new_v4();
        let auth_token = "enrollment-auth-token-0123456789abcdef";
        let ca_chain_pem = realistic_ca_chain_pem();

        let png =
            render_enrollment_qr_png(session_id, auth_token, &ca_chain_pem).expect("render QR png");

        let img = image::load_from_memory(&png).expect("decode png").into_luma8();
        let material = decode_enrollment_session(&img).expect("real decoder accepts the QR");

        assert_eq!(material.session_id, session_id);
        assert_eq!(material.auth_token, auth_token);
        assert_eq!(material.ca_chain_pem, ca_chain_pem);
    }

    #[test]
    fn empty_ca_chain_is_rejected_before_rendering() {
        let err = render_enrollment_qr_png(Uuid::new_v4(), "tok", "   \n")
            .expect_err("blank CA chain must be refused");
        assert!(err.to_string().contains("CA chain"), "got: {err}");
    }

    #[test]
    fn parse_create_response_surfaces_perchpub_error_envelope() {
        // A failed create (e.g. an expired bearer token) returns
        // `{"detail": ...}` and no session. The helper must explain that —
        // not emit a cryptic "missing field `session_id`".
        let err = parse_create_response(r#"{"detail":"Could not validate credentials"}"#)
            .expect_err("an error envelope must not parse as a session");
        let msg = err.to_string();
        assert!(
            msg.contains("did not return a session")
                && msg.contains("Could not validate credentials"),
            "error should name the perchpub detail; got: {msg}"
        );
        assert!(!msg.contains("missing field"), "should not leak the raw serde error; got: {msg}");
    }

    #[test]
    fn parse_create_response_extracts_session_and_ignores_expires_at() {
        let session_id = Uuid::new_v4();
        let body = serde_json::json!({
            "session_id": session_id,
            "auth_token": "tok-abc",
            "expires_at": "2099-01-01T00:00:00Z",
        })
        .to_string();

        let (parsed_id, token) = parse_create_response(&body).expect("parse create response");
        assert_eq!(parsed_id, session_id);
        assert_eq!(token, "tok-abc");
    }
}
