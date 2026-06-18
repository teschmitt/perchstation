//! Shared `#[cfg(test)]` fixtures for the delivery runner and classify
//! poller tests: a self-signed mTLS client whose endpoint is never
//! actually reached, a settable fake clock, a short backoff schedule, and
//! a never-expiring station identity. Lets those loops' queue-side
//! behaviour be exercised without standing up a real perchpub.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose, PKCS_ED25519};
use uuid::Uuid;

use crate::delivery::retry::BackoffSchedule;
use crate::hw_traits::Clock;
use crate::identity::{
    CA_CHAIN_FILE, CREDENTIALS_DIR, STATION_CERT_FILE, STATION_KEY_FILE, StationIdentity,
};
use crate::perchpub::client::PerchpubClient;

/// Idempotently install the rustls crypto provider (required before any
/// `PerchpubClient::new`).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Write a self-signed CA + station leaf into `<dir>/credentials/` so
/// [`PerchpubClient::new`] can build an mTLS client. The endpoint these
/// credentials authenticate against is never contacted in these tests.
fn write_credentials(dir: &Path) {
    let ca_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
    let mut ca_params = CertificateParams::new(vec!["test-ca".into()]).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    ca_params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    ca_params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    let ca = ca_params.self_signed(&ca_key).unwrap();

    let leaf_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
    let mut leaf_params =
        CertificateParams::new(vec![format!("station-{}", Uuid::new_v4())]).unwrap();
    leaf_params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    leaf_params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();

    let creds = dir.join(CREDENTIALS_DIR);
    std::fs::create_dir_all(&creds).unwrap();
    std::fs::write(creds.join(STATION_CERT_FILE), leaf.pem()).unwrap();
    std::fs::write(creds.join(STATION_KEY_FILE), leaf_key.serialize_pem()).unwrap();
    std::fs::write(creds.join(CA_CHAIN_FILE), ca.pem()).unwrap();
}

/// Build an mTLS client pointed at a loopback URL that is never served —
/// suitable for tests that exercise queue-side behaviour without a real
/// perchpub. Installs the rustls crypto provider and writes credentials
/// under `dir` first.
pub fn fake_client(dir: &Path) -> PerchpubClient {
    install_crypto_provider();
    write_credentials(dir);
    PerchpubClient::new(dir, "https://127.0.0.1:1").expect("build test mTLS client")
}

/// A station identity whose cert never expires, so the delivery loop's
/// pre-flight expiry check never short-circuits productive work.
pub fn far_future_identity() -> StationIdentity {
    StationIdentity {
        station_id: Uuid::new_v4(),
        enrolled_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        perchpub_url: "https://127.0.0.1:1".to_string(),
        cert_not_after: Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap(),
    }
}

/// A settable clock for deterministic delivery/poller tests.
pub struct FakeClock {
    now: Mutex<DateTime<Utc>>,
}

impl FakeClock {
    pub fn new(t: DateTime<Utc>) -> Self {
        Self { now: Mutex::new(t) }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }
}

/// An `Arc<dyn Clock>` fixed at a representative instant.
pub fn arc_clock() -> Arc<dyn Clock> {
    Arc::new(FakeClock::new(Utc.with_ymd_and_hms(2026, 5, 27, 12, 0, 0).unwrap()))
}

/// A short, jitter-free backoff schedule so runner tests don't sleep long.
pub fn fast_schedule() -> BackoffSchedule {
    BackoffSchedule {
        initial_delay: Duration::from_millis(1),
        max_attempt_delay: Duration::from_millis(5),
        multiplier: 2.0,
        jitter_fraction: 0.0,
        per_clip_max_attempts: 3,
        per_clip_max_wallclock: Duration::from_mins(1),
    }
}
