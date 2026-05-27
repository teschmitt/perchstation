//! Structured logging: format selection, event-code constants, and the
//! secret-redaction registry stub.
//!
//! Events are emitted via the `tracing` crate's `info!`/`warn!`/`error!`
//! macros, with `event = events::SOMETHING` keyed against the constants in
//! the [`events`] module. The contract is `specs/001-clip-delivery/contracts/log-events.md`.
//!
//! Redaction: this module currently exposes a [`RedactionRegistry`] that
//! call-sites can register secrets against. The full filtering [`tracing`]
//! layer that scans event fields and drops events containing registered
//! markers is implemented in T059; until then, the registry is a holding
//! pen and producers stay disciplined about which fields they log (the
//! contract test `log_redaction.rs` will fail loudly if they don't).

use std::sync::{Mutex, OnceLock};

use tracing_subscriber::EnvFilter;

/// Choice between machine-friendly JSON (the default, for journald) and
/// a human-friendly text format for interactive SSH use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    #[default]
    Json,
    Text,
}

impl LogFormat {
    /// Parse `--log-format` flag values.
    ///
    /// Returns `None` for unrecognised values so the CLI can emit a
    /// usage-error exit code (64) with a clear message.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(Self::Json),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}

/// Install the global `tracing` subscriber. Idempotent: subsequent calls
/// are no-ops (useful for tests that run multiple binary invocations in a
/// single process).
///
/// `level` is parsed as a `tracing_subscriber::EnvFilter` filter string —
/// accepts `info`, `debug`, per-target settings like
/// `perchstation_core::delivery=debug,info`, etc.
pub fn init(format: LogFormat, level: &str) -> Result<(), TracingInitError> {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.get().is_some() {
        return Ok(());
    }
    let filter =
        EnvFilter::try_new(level).map_err(|e| TracingInitError { message: e.to_string() })?;
    match format {
        LogFormat::Json => {
            let subscriber = tracing_subscriber::fmt()
                .json()
                // Flatten event fields into the JSON root so downstream
                // tooling can match on `event` / `clip_id` / `station_id`
                // without unwrapping a `fields` object — matches the schema
                // documented in `contracts/log-events.md` §Common fields.
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|e| TracingInitError { message: e.to_string() })?;
        }
        LogFormat::Text => {
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|e| TracingInitError { message: e.to_string() })?;
        }
    }
    let _ = INSTALLED.set(());
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("tracing subscriber could not be installed: {message}")]
pub struct TracingInitError {
    pub message: String,
}

/// Stable machine-readable event codes. Every `tracing::info!`/`warn!`/
/// `error!` site in the workspace must use one of these via
/// `event = events::CODE`, so the set in this module is the contract
/// surface enumerated in `contracts/log-events.md`.
pub mod events {
    // Enrollment
    pub const ENROLLMENT_QR_DECODED: &str = "enrollment.qr_decoded";
    pub const ENROLLMENT_CSR_GENERATED: &str = "enrollment.csr_generated";
    pub const ENROLLMENT_SENT: &str = "enrollment.sent";
    pub const ENROLLMENT_PERSISTED: &str = "enrollment.persisted";
    pub const ENROLLMENT_REFUSED: &str = "enrollment.refused";
    pub const ENROLLMENT_REFUSED_OVERWRITE: &str = "enrollment.refused_overwrite";
    pub const ENROLLMENT_FAILED: &str = "enrollment.failed";
    pub const ENROLLMENT_SESSION_INVALID: &str = "enrollment.session_invalid";
    /// Fired in addition to `enrollment.persisted` whenever
    /// `perchstation enroll --force` overwrote a pre-existing identity.
    /// Carries `previous_station_id` and `station_id` so the operator
    /// can audit the substitution in journald.
    pub const ENROLLMENT_OVERWRITTEN: &str = "enrollment.overwritten";

    // Queue
    pub const QUEUE_ENQUEUED: &str = "queue.enqueued";
    pub const QUEUE_RECOVERED_INFLIGHT: &str = "queue.recovered_inflight";
    pub const QUEUE_EVICTED: &str = "queue.evicted";
    pub const QUEUE_ZERO_LENGTH_SKIPPED: &str = "queue.zero_length_skipped";
    pub const QUEUE_DISK_FULL: &str = "queue.disk_full";

    // Delivery
    pub const DELIVERY_ATTEMPT_STARTED: &str = "delivery.attempt_started";
    pub const DELIVERY_UPLOAD_SUCCEEDED: &str = "delivery.upload_succeeded";
    pub const DELIVERY_UPLOAD_TRANSIENT: &str = "delivery.upload_transient";
    pub const DELIVERY_UPLOAD_TERMINAL: &str = "delivery.upload_terminal";
    pub const DELIVERY_ATTEMPTS_EXHAUSTED: &str = "delivery.attempts_exhausted";
    pub const DELIVERY_CERT_EXPIRED: &str = "delivery.cert_expired";

    // Classify-task polling
    pub const CLASSIFY_POLLED: &str = "classify.polled";
    pub const CLASSIFY_TERMINAL: &str = "classify.terminal";
    pub const CLASSIFY_LOST: &str = "classify.lost";

    // Lifecycle
    pub const SERVICE_READY: &str = "service.ready";
    pub const SERVICE_SHUTDOWN: &str = "service.shutdown";
    pub const SERVICE_CONFIG_INVALID: &str = "service.config_invalid";
}

/// In-memory registry of strings that must never appear in any log event.
///
/// T013 ships the registry; T059 hooks it up to a `tracing` layer that
/// scans every event's recorded field values and drops events containing
/// any registered marker.
#[derive(Default)]
pub struct RedactionRegistry {
    secrets: Mutex<Vec<String>>,
}

impl RedactionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a secret marker. Empty strings are ignored (so callers can
    /// register the body of a string field that turns out to be empty
    /// without polluting the registry).
    pub fn register(&self, secret: impl Into<String>) {
        let secret = secret.into();
        if secret.is_empty() {
            return;
        }
        let mut guard = self.secrets.lock().expect("registry lock poisoned");
        if !guard.iter().any(|existing| existing == &secret) {
            guard.push(secret);
        }
    }

    /// `true` if `text` contains any registered secret as a substring.
    /// Used by T059's filter layer.
    #[must_use]
    pub fn contains_any(&self, text: &str) -> bool {
        let guard = self.secrets.lock().expect("registry lock poisoned");
        guard.iter().any(|secret| text.contains(secret))
    }

    /// Number of registered secrets. Convenience for tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.secrets.lock().expect("registry lock poisoned").len()
    }

    /// `true` if no secrets are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_parses_known_values() {
        assert_eq!(LogFormat::parse("json"), Some(LogFormat::Json));
        assert_eq!(LogFormat::parse("text"), Some(LogFormat::Text));
        assert_eq!(LogFormat::parse("yaml"), None);
    }

    #[test]
    fn redaction_registry_dedupes_and_detects() {
        let reg = RedactionRegistry::new();
        reg.register("hunter2");
        reg.register("hunter2");
        reg.register("");
        reg.register("api-key-7");
        assert_eq!(reg.len(), 2);
        assert!(reg.contains_any("the password is hunter2 today"));
        assert!(reg.contains_any("api-key-7"));
        assert!(!reg.contains_any("nothing to see here"));
    }

    #[test]
    fn event_codes_match_log_contract_strings() {
        // Spot-check that the constants reflect the documented strings —
        // the contract test enforces the rest.
        assert_eq!(events::ENROLLMENT_QR_DECODED, "enrollment.qr_decoded");
        assert_eq!(events::DELIVERY_UPLOAD_SUCCEEDED, "delivery.upload_succeeded");
        assert_eq!(events::SERVICE_READY, "service.ready");
    }
}
