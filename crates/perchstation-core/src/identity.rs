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

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use rcgen::KeyPair;
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
    #[error("private key file `{path}` is not a usable keypair: {message}")]
    KeyPem { path: PathBuf, message: String },
    #[error("certificate file `{path}` does not contain a parsable X.509: {message}")]
    CertX509 { path: PathBuf, message: String },
    #[error("certificate `not_after` ({timestamp}) is not representable as a UTC timestamp")]
    CertNotAfter { timestamp: i64 },
    #[error(
        "credentials already exist for station {existing_station_id} at `{path}`; pass overwrite to replace"
    )]
    AlreadyExists { path: PathBuf, existing_station_id: Uuid },
    #[error("could not serialise identity.json: {0}")]
    Serialise(#[source] serde_json::Error),
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

    /// `true` once the cert's `not_after` has been reached relative to
    /// `now`. Used by `status` and by the per-upload pre-flight check. The
    /// comparison is inclusive (`<=`): `not_after` is parsed truncated to a
    /// whole second, so the boundary second is a real, reachable instant at
    /// which the cert must be treated as already expired (halt before
    /// perchpub rejects the upload as expired).
    #[must_use]
    pub fn cert_is_expired(&self, now: DateTime<Utc>) -> bool {
        self.cert_not_after <= now
    }
}

/// Inputs to [`save`]. PEM strings are written verbatim; `enrolled_at`
/// stamps `identity.json`; `cert_not_after` is parsed back out of
/// `station_cert_pem` for the returned [`StationIdentity`].
#[derive(Debug, Clone)]
pub struct SaveOptions<'a> {
    pub station_id: Uuid,
    pub enrolled_at: DateTime<Utc>,
    pub perchpub_url: &'a str,
    pub station_key_pem: &'a str,
    pub station_cert_pem: &'a str,
    pub ca_chain_pem: &'a str,
    /// `false` (the default) refuses to clobber an existing
    /// `credentials/` directory and returns [`IdentityError::AlreadyExists`].
    /// `true` rotates the existing directory out atomically and replaces
    /// it. Satisfies FR-003.
    pub overwrite: bool,
}

/// Return the `station_id` recorded in `<data_dir>/credentials/identity.json`
/// if the file is present and parseable. Used by the enrollment command to
/// name the prior station in the `enrollment.overwritten` audit when `--force`
/// mints a new identity.
///
/// Returns `Ok(None)` if `identity.json` is absent — the directory may not
/// exist at all (fresh station) or may have been partially staged.
pub fn peek_existing_station_id(data_dir: &Path) -> Result<Option<Uuid>, IdentityError> {
    let identity_path = data_dir.join(CREDENTIALS_DIR).join(IDENTITY_FILE);
    match std::fs::read_to_string(&identity_path) {
        Ok(text) => {
            let file: IdentityFile = serde_json::from_str(&text)
                .map_err(|source| IdentityError::Parse { path: identity_path, source })?;
            Ok(Some(file.station_id))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(IdentityError::Io { path: identity_path, source }),
    }
}

/// Load the persisted private key back into an rcgen [`KeyPair`].
///
/// Reads `<data_dir>/credentials/station.key` (PEM) and parses it. This is the
/// generate-once / reuse-for-life path (device-cert contract §2/§8): a renewal
/// or a same-station re-enroll rebuilds its CSR over *this* key (via
/// [`crate::enrollment::csr::build_from_keypair`]) so the issued leaf keeps the
/// same `SHA256(SubjectPublicKeyInfo)` and perchpub recognises the same
/// station with no server-side re-pin. The returned key never leaves the device.
pub fn load_keypair(data_dir: &Path) -> Result<KeyPair, IdentityError> {
    let key_path = data_dir.join(CREDENTIALS_DIR).join(STATION_KEY_FILE);
    let pem = std::fs::read_to_string(&key_path)
        .map_err(|source| IdentityError::Io { path: key_path.clone(), source })?;
    KeyPair::from_pem(&pem)
        .map_err(|err| IdentityError::KeyPem { path: key_path, message: err.to_string() })
}

/// Atomically persist a freshly-enrolled identity into
/// `<data_dir>/credentials/`.
///
/// Steps:
/// 1. Stage every file into `<data_dir>/credentials.tmp/` (cleaned of any
///    leftovers from a prior crashed run).
/// 2. If `credentials/` already exists, rotate it to `credentials.old/`
///    (refused if `overwrite` is false; FR-003).
/// 3. Rename `credentials.tmp/` → `credentials/` (a same-filesystem
///    directory rename — atomic with respect to readers that see the
///    final pathname), then `fsync` the parent `data_dir` so the rename
///    itself is durable, not just the file contents.
/// 4. Best-effort delete `credentials.old/` (a crash before this step
///    leaves the old directory behind; a subsequent invocation will
///    sweep it on its own staging pass).
///
/// `station.key` is written with `O_CREAT|O_EXCL|O_WRONLY` and mode
/// `0o600` — the most restrictive permission compatible with the
/// `perchstation` system user reading its own key. The other PEMs and
/// `identity.json` are written `0o644` (key material discipline applies
/// only to the private key).
pub fn save(data_dir: &Path, opts: &SaveOptions<'_>) -> Result<StationIdentity, IdentityError> {
    let creds_path = data_dir.join(CREDENTIALS_DIR);
    let staging_path = data_dir.join("credentials.tmp");
    let rotated_path = data_dir.join("credentials.old");

    // Refuse early if credentials/ exists and overwrite isn't set.
    if !opts.overwrite
        && let Some(existing) = peek_existing_station_id(data_dir)?
    {
        return Err(IdentityError::AlreadyExists {
            path: creds_path.join(IDENTITY_FILE),
            existing_station_id: existing,
        });
    }

    // Sweep any leftover staging / rotated directories from a prior aborted run.
    remove_dir_if_exists(&staging_path)?;
    remove_dir_if_exists(&rotated_path)?;

    // Make sure data_dir itself exists (the operator may have only
    // pointed config.toml at it, not created it).
    fs::create_dir_all(data_dir)
        .map_err(|source| IdentityError::Io { path: data_dir.to_path_buf(), source })?;

    // Stage all four files in credentials.tmp/.
    fs::create_dir(&staging_path)
        .map_err(|source| IdentityError::Io { path: staging_path.clone(), source })?;

    write_mode(&staging_path.join(STATION_CERT_FILE), opts.station_cert_pem.as_bytes(), 0o644)?;
    write_mode(&staging_path.join(CA_CHAIN_FILE), opts.ca_chain_pem.as_bytes(), 0o644)?;
    write_mode(&staging_path.join(STATION_KEY_FILE), opts.station_key_pem.as_bytes(), 0o600)?;

    let identity_file = IdentityFile {
        station_id: opts.station_id,
        enrolled_at: opts.enrolled_at,
        perchpub_url: opts.perchpub_url.to_string(),
    };
    let identity_json =
        serde_json::to_vec_pretty(&identity_file).map_err(IdentityError::Serialise)?;
    write_mode(&staging_path.join(IDENTITY_FILE), &identity_json, 0o644)?;

    // Make the staged directory entries durable before we publish them via
    // rename — the file contents were sync'd by write_mode, but the dir
    // entries linking them may still be in the page cache.
    fsync_dir(&staging_path)?;

    // Rotate any existing credentials/ out of the way.
    let creds_existed = creds_path.exists();
    if creds_existed {
        fs::rename(&creds_path, &rotated_path)
            .map_err(|source| IdentityError::Io { path: creds_path.clone(), source })?;
    }

    // Promote staging → credentials/. On failure, restore the rotated
    // directory so the operator's prior identity isn't left orphaned.
    if let Err(source) = fs::rename(&staging_path, &creds_path) {
        if creds_existed {
            // Best-effort restore — if even this fails the user is in a
            // bad spot, but the original error is the more informative one.
            let _ = fs::rename(&rotated_path, &creds_path);
        }
        return Err(IdentityError::Io { path: creds_path, source });
    }

    // Persist the rename(s) into data_dir itself. Both the rotation rename
    // and the staging→credentials promote modify entries in data_dir; one
    // fsync of the parent makes them durable so a power cut can't lose the
    // just-enrolled identity after save() reported success.
    fsync_dir(data_dir)?;

    if creds_existed {
        // Best-effort cleanup; a leftover credentials.old/ is harmless
        // because the next save() invocation sweeps it.
        let _ = fs::remove_dir_all(&rotated_path);
    }

    // Re-load so the caller gets the same cert_not_after the rest of the
    // process will see (parsed from the freshly-written station.crt).
    StationIdentity::load(data_dir)
}

/// Flush a directory's own entries to stable storage by `fsync`-ing the
/// directory. [`write_mode`] makes file *contents* durable, but a `rename`
/// that publishes those files only updates the parent directory, whose
/// entry change can linger in the page cache until the directory itself is
/// sync'd.
fn fsync_dir(path: &Path) -> Result<(), IdentityError> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|source| IdentityError::Io { path: path.to_path_buf(), source })
}

/// Create `path` exclusively (`O_CREAT|O_EXCL|O_WRONLY`) with the given unix
/// permission `mode`, write `bytes`, and `fsync` the contents. The `0o600`
/// private-key discipline is a unix concept; the import + `.mode()` are
/// cfg-gated so `perchstation-core` stays compilable on non-unix targets
/// (PS-28). perchstation's only production target is Linux.
#[cfg(unix)]
fn write_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<(), IdentityError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|source| IdentityError::Io { path: path.to_path_buf(), source })?;
    file.write_all(bytes)
        .map_err(|source| IdentityError::Io { path: path.to_path_buf(), source })?;
    file.sync_all().map_err(|source| IdentityError::Io { path: path.to_path_buf(), source })?;
    Ok(())
}

/// Non-unix fallback: still creates the file exclusively, but cannot apply
/// the unix `mode`. Exists only so the platform-agnostic crate compiles off
/// unix; production runs on Linux where the unix `write_mode` variant
/// enforces `0o600` on `station.key`.
#[cfg(not(unix))]
fn write_mode(path: &Path, bytes: &[u8], _mode: u32) -> Result<(), IdentityError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| IdentityError::Io { path: path.to_path_buf(), source })?;
    file.write_all(bytes)
        .map_err(|source| IdentityError::Io { path: path.to_path_buf(), source })?;
    file.sync_all().map_err(|source| IdentityError::Io { path: path.to_path_buf(), source })?;
    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<(), IdentityError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(IdentityError::Io { path: path.to_path_buf(), source }),
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

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Build a self-signed Ed25519 cert and return `(cert_pem, key_pem)`,
    /// so the `save()` tests have realistic PEM bytes to round-trip.
    fn build_signing_material() -> (String, String) {
        let key = KeyPair::generate_for(&PKCS_ED25519).expect("ed25519 keypair");
        let mut params = CertificateParams::new(vec!["test.local".into()]).expect("params");
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2099, 1, 1);
        let cert = params.self_signed(&key).expect("self-sign");
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn load_keypair_round_trips_to_same_spki() {
        // §2/§8: a saved-then-loaded key must yield the same SPKI, so a CSR
        // rebuilt from the reloaded key re-presents the same station identity.
        let dir = TempDir::new().expect("tempdir");
        let creds = dir.path().join(CREDENTIALS_DIR);
        fs::create_dir_all(&creds).unwrap();
        let original = KeyPair::generate_for(&PKCS_ED25519).expect("keypair");
        fs::write(creds.join(STATION_KEY_FILE), original.serialize_pem()).unwrap();

        let loaded = load_keypair(dir.path()).expect("load station.key");
        assert_eq!(
            loaded.public_key_der(),
            original.public_key_der(),
            "reloaded key has a different SPKI",
        );
    }

    #[test]
    fn load_keypair_reports_missing_key() {
        let dir = TempDir::new().expect("tempdir");
        let err = load_keypair(dir.path()).expect_err("no station.key present");
        assert!(matches!(err, IdentityError::Io { .. }));
    }

    #[test]
    fn peek_existing_returns_none_when_absent() {
        let dir = TempDir::new().expect("tempdir");
        let id = peek_existing_station_id(dir.path()).expect("peek");
        assert!(id.is_none());
    }

    #[test]
    fn peek_existing_extracts_station_id_when_present() {
        let dir = TempDir::new().expect("tempdir");
        let creds = dir.path().join(CREDENTIALS_DIR);
        fs::create_dir_all(&creds).unwrap();
        let station_id = Uuid::new_v4();
        write_identity_json(&creds.join(IDENTITY_FILE), station_id, "https://x");
        let id = peek_existing_station_id(dir.path()).expect("peek").expect("present");
        assert_eq!(id, station_id);
    }

    // The `0o600` permission assertion is unix-only; on non-unix targets the
    // `write_mode` fallback cannot set a mode (PS-28), so this test is gated.
    #[cfg(unix)]
    #[test]
    fn save_writes_all_four_files_with_correct_modes() {
        let dir = TempDir::new().expect("tempdir");
        let (cert_pem, key_pem) = build_signing_material();
        let station_id = Uuid::new_v4();
        let enrolled_at = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();

        let identity = save(
            dir.path(),
            &SaveOptions {
                station_id,
                enrolled_at,
                perchpub_url: "https://perchpub.example.org",
                station_key_pem: &key_pem,
                station_cert_pem: &cert_pem,
                ca_chain_pem: &cert_pem, // self-signed, so cert == CA
                overwrite: false,
            },
        )
        .expect("save");

        assert_eq!(identity.station_id, station_id);
        assert_eq!(identity.enrolled_at, enrolled_at);

        let creds = dir.path().join(CREDENTIALS_DIR);
        let key_mode =
            fs::metadata(creds.join(STATION_KEY_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(key_mode, 0o600, "station.key permissions are 0o{key_mode:o}");

        let on_disk = fs::read_to_string(creds.join(IDENTITY_FILE)).expect("read identity.json");
        let parsed: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(parsed["station_id"], serde_json::json!(station_id));
        assert_eq!(parsed["perchpub_url"], "https://perchpub.example.org");

        assert!(!dir.path().join("credentials.tmp").exists(), "credentials.tmp should be gone");
        assert!(!dir.path().join("credentials.old").exists(), "credentials.old should be gone");
    }

    #[test]
    fn save_refuses_when_credentials_present_and_overwrite_false() {
        let dir = TempDir::new().expect("tempdir");
        let (cert_pem, key_pem) = build_signing_material();
        let existing_id = Uuid::new_v4();
        // First save — fresh, succeeds.
        save(
            dir.path(),
            &SaveOptions {
                station_id: existing_id,
                enrolled_at: Utc::now(),
                perchpub_url: "https://a",
                station_key_pem: &key_pem,
                station_cert_pem: &cert_pem,
                ca_chain_pem: &cert_pem,
                overwrite: false,
            },
        )
        .expect("first save");

        // Second save — same dir, overwrite=false, must refuse with the
        // existing station_id surfaced.
        let err = save(
            dir.path(),
            &SaveOptions {
                station_id: Uuid::new_v4(),
                enrolled_at: Utc::now(),
                perchpub_url: "https://b",
                station_key_pem: &key_pem,
                station_cert_pem: &cert_pem,
                ca_chain_pem: &cert_pem,
                overwrite: false,
            },
        )
        .expect_err("must refuse second save");
        match err {
            IdentityError::AlreadyExists { existing_station_id, .. } => {
                assert_eq!(existing_station_id, existing_id);
            }
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn save_overwrites_when_flag_set() {
        let dir = TempDir::new().expect("tempdir");
        let (cert_pem, key_pem) = build_signing_material();
        let first_id = Uuid::new_v4();
        save(
            dir.path(),
            &SaveOptions {
                station_id: first_id,
                enrolled_at: Utc::now(),
                perchpub_url: "https://a",
                station_key_pem: &key_pem,
                station_cert_pem: &cert_pem,
                ca_chain_pem: &cert_pem,
                overwrite: false,
            },
        )
        .expect("first save");

        let new_id = Uuid::new_v4();
        save(
            dir.path(),
            &SaveOptions {
                station_id: new_id,
                enrolled_at: Utc::now(),
                perchpub_url: "https://b",
                station_key_pem: &key_pem,
                station_cert_pem: &cert_pem,
                ca_chain_pem: &cert_pem,
                overwrite: true,
            },
        )
        .expect("force save");

        let identity = StationIdentity::load(dir.path()).expect("reload");
        assert_eq!(identity.station_id, new_id);
        assert_eq!(identity.perchpub_url, "https://b");
        assert!(!dir.path().join("credentials.old").exists());
    }

    #[test]
    fn save_sweeps_leftover_staging_from_prior_crash() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir_all(dir.path().join("credentials.tmp")).unwrap();
        fs::write(dir.path().join("credentials.tmp/leftover"), b"junk").unwrap();

        let (cert_pem, key_pem) = build_signing_material();
        save(
            dir.path(),
            &SaveOptions {
                station_id: Uuid::new_v4(),
                enrolled_at: Utc::now(),
                perchpub_url: "https://x",
                station_key_pem: &key_pem,
                station_cert_pem: &cert_pem,
                ca_chain_pem: &cert_pem,
                overwrite: false,
            },
        )
        .expect("save");
        // The leftover staging dir is gone (it was rotated into credentials/).
        assert!(!dir.path().join("credentials.tmp").exists());
        // And the leftover file did not bleed into the final credentials/.
        assert!(!dir.path().join("credentials/leftover").exists());
    }

    #[test]
    fn fsync_dir_succeeds_on_existing_dir() {
        let dir = TempDir::new().expect("tempdir");
        fsync_dir(dir.path()).expect("fsync an existing directory");
    }

    #[test]
    fn fsync_dir_errors_on_missing_path() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let err = fsync_dir(&missing).expect_err("fsync of a missing path must error");
        assert!(matches!(err, IdentityError::Io { .. }));
    }

    #[test]
    fn save_fsyncs_parent_dir_after_rename() {
        let dir = TempDir::new().expect("tempdir");
        let (cert_pem, key_pem) = build_signing_material();
        let station_id = Uuid::new_v4();

        let identity = save(
            dir.path(),
            &SaveOptions {
                station_id,
                enrolled_at: Utc.timestamp_opt(1_700_000_000, 0).single().unwrap(),
                perchpub_url: "https://perchpub.example.org",
                station_key_pem: &key_pem,
                station_cert_pem: &cert_pem,
                ca_chain_pem: &cert_pem,
                overwrite: false,
            },
        )
        .expect("save with parent-dir fsync");
        assert_eq!(identity.station_id, station_id);

        // The rename was made durable and the credentials round-trip back.
        let reloaded = StationIdentity::load(dir.path()).expect("reload after fsync");
        assert_eq!(reloaded.station_id, station_id);
    }

    fn identity_with_not_after(not_after: DateTime<Utc>) -> StationIdentity {
        StationIdentity {
            station_id: Uuid::new_v4(),
            enrolled_at: Utc.timestamp_opt(1_700_000_000, 0).single().unwrap(),
            perchpub_url: "https://perchpub.example.org".into(),
            cert_not_after: not_after,
        }
    }

    #[test]
    fn cert_is_expired_at_exact_not_after_second_is_true() {
        let not_after = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).single().unwrap();
        let identity = identity_with_not_after(not_after);
        // The boundary second is a reachable instant (the cert time is
        // truncated to whole seconds); halt conservatively *at* it.
        assert!(identity.cert_is_expired(not_after));
    }

    #[test]
    fn cert_is_not_expired_one_second_before_not_after() {
        let not_after = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).single().unwrap();
        let identity = identity_with_not_after(not_after);
        assert!(!identity.cert_is_expired(not_after - chrono::Duration::seconds(1)));
    }

    #[test]
    fn cert_is_expired_one_second_after_not_after() {
        let not_after = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).single().unwrap();
        let identity = identity_with_not_after(not_after);
        assert!(identity.cert_is_expired(not_after + chrono::Duration::seconds(1)));
    }
}
