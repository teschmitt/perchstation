//! Station identity: the four files written atomically at end of enrollment.
//!
//! Layout (`data_model.md`):
//!
//! ```text
//! <data_dir>/credentials/
//! ├── station.key          # PEM, Ed25519 private key, mode 0600
//! ├── station.crt          # PEM, enrollment-issued cert
//! ├── ca_chain.pem         # PEM, perchpub CA chain
//! └── identity.json        # StationIdentity metadata
//! ```
//!
//! `cert_not_after` is *parsed from the cert itself* at load time, not
//! stored in `identity.json`; the JSON sidecar carries `station_id`,
//! `enrolled_at`, and `perchpub_url` only.

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const CREDENTIALS_DIR: &str = "credentials";
pub const IDENTITY_FILE: &str = "identity.json";
pub const STATION_KEY_FILE: &str = "station.key";
pub const STATION_CERT_FILE: &str = "station.crt";
pub const CA_CHAIN_FILE: &str = "ca_chain.pem";

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("identity file `{path}` could not be read: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("identity file `{path}` is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("certificate file `{path}` is not valid PEM: {message}")]
    CertPem { path: PathBuf, message: String },
    #[error("certificate file `{path}` does not contain a parsable X.509: {message}")]
    CertX509 { path: PathBuf, message: String },
    #[error("certificate `not_after` ({timestamp}) is not representable as a UTC timestamp")]
    CertNotAfter { timestamp: i64 },
}

/// On-disk identity metadata. `cert_not_after` is filled by [`StationIdentity::load`]
/// from `station.crt`; it is *not* present in `identity.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StationIdentity {
    pub station_id: Uuid,
    pub enrolled_at: DateTime<Utc>,
    pub perchpub_url: String,
    #[serde(skip)]
    pub cert_not_after: DateTime<Utc>,
}

/// Subset of [`StationIdentity`] that is actually serialised to
/// `identity.json`. `cert_not_after` is derived from `station.crt` and not
/// duplicated on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdentityFile {
    station_id: Uuid,
    enrolled_at: DateTime<Utc>,
    perchpub_url: String,
}

impl StationIdentity {
    /// Load `identity.json` from `<data_dir>/credentials/` and enrich with
    /// `cert_not_after` parsed from `station.crt`.
    pub fn load(data_dir: &Path) -> Result<Self, IdentityError> {
        let creds = data_dir.join(CREDENTIALS_DIR);
        let identity_path = creds.join(IDENTITY_FILE);
        let cert_path = creds.join(STATION_CERT_FILE);

        let identity_text = std::fs::read_to_string(&identity_path)
            .map_err(|source| IdentityError::Io { path: identity_path.clone(), source })?;
        let file: IdentityFile = serde_json::from_str(&identity_text)
            .map_err(|source| IdentityError::Parse { path: identity_path, source })?;

        let cert_pem = std::fs::read(&cert_path)
            .map_err(|source| IdentityError::Io { path: cert_path.clone(), source })?;
        let cert_not_after = parse_cert_not_after(&cert_path, &cert_pem)?;

        Ok(Self {
            station_id: file.station_id,
            enrolled_at: file.enrolled_at,
            perchpub_url: file.perchpub_url,
            cert_not_after,
        })
    }

    /// `true` if the cert's `not_after` is strictly in the past relative to
    /// `now`. Used by `status` and by the per-upload pre-flight check.
    #[must_use]
    pub fn cert_is_expired(&self, now: DateTime<Utc>) -> bool {
        self.cert_not_after < now
    }
}

/// Extract `cert_not_after` from the PEM-encoded certificate at `path`.
fn parse_cert_not_after(path: &Path, pem_bytes: &[u8]) -> Result<DateTime<Utc>, IdentityError> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem_bytes).map_err(|err| {
        IdentityError::CertPem { path: path.to_path_buf(), message: err.to_string() }
    })?;
    let cert = pem.parse_x509().map_err(|err| IdentityError::CertX509 {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    let timestamp = cert.tbs_certificate.validity.not_after.timestamp();
    Utc.timestamp_opt(timestamp, 0).single().ok_or(IdentityError::CertNotAfter { timestamp })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
    use std::fs;
    use tempfile::TempDir;

    /// Mint a self-signed Ed25519 cert (using rcgen) so the cert parser has
    /// something realistic to chew on. Returns the cert's expected
    /// `not_after` in chrono terms.
    fn write_test_cert(path: &Path) -> DateTime<Utc> {
        let key = KeyPair::generate_for(&PKCS_ED25519).expect("ed25519 keypair");
        let mut params = CertificateParams::new(vec!["test.local".into()]).expect("params");
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2099, 1, 1);
        let cert = params.self_signed(&key).expect("self-sign");
        fs::write(path, cert.pem()).expect("write cert");
        // The cert encodes `not_after = 2099-01-01T00:00:00Z`.
        Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).single().unwrap()
    }

    fn write_identity_json(path: &Path, station_id: Uuid, perchpub_url: &str) -> DateTime<Utc> {
        let enrolled_at = Utc::now();
        // Strip sub-second precision so equality checks survive the JSON
        // round-trip.
        let enrolled_at = Utc.timestamp_opt(enrolled_at.timestamp(), 0).single().unwrap();
        let body = serde_json::json!({
            "station_id": station_id,
            "enrolled_at": enrolled_at,
            "perchpub_url": perchpub_url,
        });
        fs::write(path, serde_json::to_vec_pretty(&body).unwrap()).expect("write identity.json");
        enrolled_at
    }

    #[test]
    fn load_returns_identity_with_cert_not_after_parsed_from_cert() {
        let dir = TempDir::new().expect("tempdir");
        let creds = dir.path().join(CREDENTIALS_DIR);
        fs::create_dir_all(&creds).unwrap();
        let station_id = Uuid::new_v4();
        let perchpub_url = "https://perchpub.example.org";
        let enrolled_at = write_identity_json(&creds.join(IDENTITY_FILE), station_id, perchpub_url);
        let expected_not_after = write_test_cert(&creds.join(STATION_CERT_FILE));

        let identity = StationIdentity::load(dir.path()).expect("load");
        assert_eq!(identity.station_id, station_id);
        assert_eq!(identity.enrolled_at, enrolled_at);
        assert_eq!(identity.perchpub_url, perchpub_url);
        assert_eq!(identity.cert_not_after, expected_not_after);
        assert!(!identity.cert_is_expired(Utc::now()));
    }

    #[test]
    fn load_reports_missing_identity_file() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir_all(dir.path().join(CREDENTIALS_DIR)).unwrap();
        let err = StationIdentity::load(dir.path()).expect_err("should fail");
        assert!(matches!(err, IdentityError::Io { .. }));
    }

    #[test]
    fn load_reports_bad_cert_pem() {
        let dir = TempDir::new().expect("tempdir");
        let creds = dir.path().join(CREDENTIALS_DIR);
        fs::create_dir_all(&creds).unwrap();
        write_identity_json(&creds.join(IDENTITY_FILE), Uuid::new_v4(), "https://x");
        fs::write(creds.join(STATION_CERT_FILE), b"not a pem\n").unwrap();
        let err = StationIdentity::load(dir.path()).expect_err("should fail");
        assert!(matches!(err, IdentityError::CertPem { .. }));
    }
}
