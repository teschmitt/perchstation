//! Enrollment: QR decode → keypair + CSR → mTLS POST → atomic persist.
//!
//! Submodules:
//! - [`csr`]   — Ed25519 keypair + PKCS#10 CSR generation (T023).
//! - [`confirm`] — pre-enrollment TLS client + `/enrollment/confirm` exchange (T025).
//! - [`file_source`] — file-backed [`QrFrameSource`] for `--qr-source=file` (T028).
//!
//! This file owns the QR decode itself (T024) and the in-memory
//! [`EnrollmentSessionMaterial`] returned to the caller.

pub mod confirm;
pub mod csr;
pub mod file_source;

use chrono::{DateTime, Utc};
use image::GrayImage;
use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

/// In-memory material decoded from a single enrollment QR frame.
///
/// `data_model.md` §`EnrollmentSessionMaterial` documents the canonical
/// fields; `ca_chain_pem` is the CA pin the station uses to validate
/// perchpub's server cert during the *pre-enrollment* `/enrollment/confirm`
/// call (the station has no `credentials/ca_chain.pem` on disk yet).
///
/// All fields are discarded as soon as enrollment finishes (success or
/// failure) — none of them ever hit disk through this module.
#[derive(Debug, Clone)]
pub struct EnrollmentSessionMaterial {
    pub session_id: Uuid,
    pub auth_token: String,
    pub ca_chain_pem: String,
    pub decoded_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum QrDecodeError {
    #[error("no QR code found in the supplied frame")]
    NoCode,
    #[error("QR code body is not valid UTF-8: {0}")]
    NotUtf8(String),
    #[error("QR payload is not valid JSON: {0}")]
    NotJson(String),
    #[error("QR payload is missing required field `{field}`")]
    MissingField { field: &'static str },
    #[error("QR payload field `{field}` is malformed: {message}")]
    BadField { field: &'static str, message: String },
}

/// Wire shape of the QR JSON payload. `expires_at` is accepted-and-ignored
/// per `data_model.md` §QR payload format — the station does not enforce
/// session expiry, perchpub does that server-side on `/enrollment/confirm`.
#[derive(Debug, Deserialize)]
struct QrPayload {
    session_id: String,
    auth_token: String,
    ca_chain_pem: String,
    #[serde(default, rename = "expires_at")]
    _expires_at: Option<String>,
}

/// Decode a single enrollment QR frame.
///
/// Caller hands in a grayscale frame (production: from the libcamera shell-out;
/// dev/recovery: from a PNG/JPEG via [`file_source`]). Decoded material is
/// returned by value and must be threaded through [`csr::generate`] and
/// [`confirm::send`] before being dropped — none of it survives the call site.
pub fn decode_enrollment_session(
    image: &GrayImage,
) -> Result<EnrollmentSessionMaterial, QrDecodeError> {
    let (width, height) = image.dimensions();
    let mut img =
        rqrr::PreparedImage::prepare_from_greyscale(width as usize, height as usize, |x, y| {
            // rqrr's closure indices are bounded by the (width, height) passed
            // above, both of which originated as u32 from GrayImage::dimensions;
            // the round-trip can never truncate.
            let xu = u32::try_from(x).unwrap_or(u32::MAX);
            let yu = u32::try_from(y).unwrap_or(u32::MAX);
            image.get_pixel(xu, yu).0[0]
        });

    let grids = img.detect_grids();
    let grid = grids.into_iter().next().ok_or(QrDecodeError::NoCode)?;
    let (_meta, text) = grid.decode().map_err(|e| QrDecodeError::NotUtf8(e.to_string()))?;

    let payload: QrPayload =
        serde_json::from_str(&text).map_err(|e| QrDecodeError::NotJson(e.to_string()))?;

    if payload.auth_token.is_empty() {
        return Err(QrDecodeError::MissingField { field: "auth_token" });
    }
    if payload.ca_chain_pem.is_empty() {
        return Err(QrDecodeError::MissingField { field: "ca_chain_pem" });
    }
    let session_id = Uuid::parse_str(&payload.session_id)
        .map_err(|e| QrDecodeError::BadField { field: "session_id", message: e.to_string() })?;

    Ok(EnrollmentSessionMaterial {
        session_id,
        auth_token: payload.auth_token,
        ca_chain_pem: payload.ca_chain_pem,
        decoded_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    use image::{ImageBuffer, ImageFormat, Luma};
    use qrcode::QrCode;
    use serde_json::json;

    fn render_qr(payload: &str) -> GrayImage {
        let code = QrCode::new(payload.as_bytes()).expect("build QR");
        let img: ImageBuffer<Luma<u8>, Vec<u8>> =
            code.render::<Luma<u8>>().min_dimensions(300, 300).quiet_zone(true).build();
        // Round-trip through PNG so the buffer matches how prod actually
        // feeds frames (loaded from disk by file_source).
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png).expect("png encode");
        let back = image::load_from_memory(&bytes).expect("png decode");
        back.into_luma8()
    }

    #[test]
    fn decode_extracts_canonical_payload() {
        let session_id = Uuid::new_v4();
        let payload = json!({
            "session_id":   session_id,
            "auth_token":   "tok-123",
            "ca_chain_pem": "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n",
        });
        let img = render_qr(&payload.to_string());
        let mat = decode_enrollment_session(&img).expect("decode");
        assert_eq!(mat.session_id, session_id);
        assert_eq!(mat.auth_token, "tok-123");
        assert!(mat.ca_chain_pem.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn decode_ignores_extra_expires_at_field() {
        let session_id = Uuid::new_v4();
        let payload = json!({
            "session_id":   session_id,
            "auth_token":   "tok",
            "ca_chain_pem": "PEM",
            "expires_at":   "2099-01-01T00:00:00Z",
        });
        let img = render_qr(&payload.to_string());
        decode_enrollment_session(&img).expect("decode tolerates expires_at");
    }

    #[test]
    fn decode_reports_missing_qr() {
        let blank = GrayImage::from_pixel(200, 200, Luma([255u8]));
        let err = decode_enrollment_session(&blank).expect_err("blank should fail");
        assert!(matches!(err, QrDecodeError::NoCode));
    }

    #[test]
    fn decode_reports_malformed_session_id() {
        let payload = json!({
            "session_id": "not-a-uuid",
            "auth_token": "tok",
            "ca_chain_pem": "PEM",
        });
        let img = render_qr(&payload.to_string());
        let err = decode_enrollment_session(&img).expect_err("bad uuid should fail");
        assert!(matches!(err, QrDecodeError::BadField { field: "session_id", .. }));
    }

    #[test]
    fn decode_reports_missing_ca_chain() {
        let payload = json!({
            "session_id": Uuid::new_v4(),
            "auth_token": "tok",
        });
        let img = render_qr(&payload.to_string());
        let err = decode_enrollment_session(&img).expect_err("missing ca_chain should fail");
        // serde itself rejects the missing required field before our check fires.
        assert!(matches!(err, QrDecodeError::NotJson(_) | QrDecodeError::MissingField { .. }));
    }
}
