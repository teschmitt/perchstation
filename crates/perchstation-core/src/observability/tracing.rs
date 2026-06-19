//! Structured logging: format selection, event-code constants, and the
//! secret-redaction layer.
//!
//! Events are emitted via the `tracing` crate's `info!`/`warn!`/`error!`
//! macros, with `event = events::SOMETHING` keyed against the constants in
//! the [`events`] module. The contract is `specs/001-clip-delivery/contracts/log-events.md`.
//!
//! Redaction: the process-wide [`RedactionRegistry`] holds the set of
//! literal byte sequences (`auth_token`, CSR PEM body, station private-key
//! PEM body, …) that must never appear in any log line. A
//! [`RedactingMakeWriter`] sits in front of the actual stderr writer and
//! scrubs each formatted log line — replacing any registered marker with
//! `[REDACTED]` — before the bytes leave the process. Producers register
//! secrets via [`register_secret`] at the moment the material is
//! constructed (QR decode, CSR generation, identity load); from that
//! point on, no field, span, panic backtrace, or stray `eprintln!`
//! routed through the configured writer can leak it.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const REDACTED_PLACEHOLDER: &str = "[REDACTED]";

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
    let writer = RedactingMakeWriter::new(redaction_registry().clone());
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
                .with_writer(writer)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|e| TracingInitError { message: e.to_string() })?;
        }
        LogFormat::Text => {
            let subscriber =
                tracing_subscriber::fmt().with_env_filter(filter).with_writer(writer).finish();
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
    /// A `pending/`/`delivered/` sidecar failed to deserialise and was
    /// moved to `corrupt/` so the scan head advances instead of failing on
    /// the same file every tick (PS-02).
    pub const QUEUE_CORRUPT_SIDECAR: &str = "queue.corrupt_sidecar";
    /// Boot sweep reclaimed an `*.mp4` with no matching sidecar — media
    /// stranded by a crash between the media rename and the sidecar write
    /// (PS-07).
    pub const QUEUE_ORPHAN_MEDIA: &str = "queue.orphan_media";
    /// A `pending/` sidecar referenced media that no longer exists; the
    /// orphan sidecar was quarantined so the delivery head advances (PS-04).
    pub const QUEUE_MISSING_MEDIA: &str = "queue.missing_media";
    /// A `delivered/` sidecar whose classify task is finished (terminal or
    /// lost) and which has sat past the retention window was pruned, so
    /// `delivered/` does not grow without bound and the classify poller's
    /// per-tick scan stays cheap (PS-25).
    pub const QUEUE_PRUNED_DELIVERED: &str = "queue.pruned_delivered";

    // Delivery
    pub const DELIVERY_ATTEMPT_STARTED: &str = "delivery.attempt_started";
    pub const DELIVERY_UPLOAD_SUCCEEDED: &str = "delivery.upload_succeeded";
    /// A 2xx upload whose classify-task body could not be decoded (PS-06):
    /// perchpub accepted the bytes, but the station could not read the
    /// classify task, so the clip is recorded `Delivered` with an unknown
    /// classify status. Distinct from `delivery.upload_succeeded` because it
    /// carries no `classify_task_id`/`duration_ms`.
    pub const DELIVERY_UPLOAD_UNDECODABLE: &str = "delivery.upload_undecodable";
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
    /// SIGHUP-triggered hot reload of the on-disk mTLS credentials after a
    /// re-enrollment, so the running delivery/classify workers present the
    /// new station cert without a `serve` restart (PS-18).
    pub const SERVICE_CREDENTIALS_RELOADED: &str = "service.credentials_reloaded";
    /// A supervised worker task ended unexpectedly (panic or non-cancellation
    /// error). The wrapper logs this and intentionally lets the other tasks
    /// keep running so a capture-side fault cannot stop delivery (and vice
    /// versa). See `specs/002-capture-subsystem/contracts/cli.md` §Failure
    /// isolation / FR-012.
    pub const SERVICE_TASK_PANICKED: &str = "service.task_panicked";

    // Capture (see `specs/002-capture-subsystem/contracts/log-events.md`).
    pub const CAPTURE_READY: &str = "capture.ready";
    pub const CAPTURE_SHUTDOWN: &str = "capture.shutdown";
    /// `serve` could not bring the capture subsystem up at boot (sensor
    /// open failed, staging purge failed, …). Delivery continues
    /// regardless (FR-012). Carries `reason` and `error`.
    pub const CAPTURE_INIT_FAILED: &str = "capture.init_failed";
    /// Emitted by `serve` on non-Linux hosts where the production
    /// capture adapters do not exist; the capture supervisor task is
    /// simply not spawned.
    pub const CAPTURE_SKIPPED: &str = "capture.skipped";
    pub const CAPTURE_STAGING_PURGED: &str = "capture.staging_purged";
    pub const CAPTURE_TRIGGER_OBSERVED: &str = "capture.trigger_observed";
    pub const CAPTURE_RECORDING_STARTED: &str = "capture.recording_started";
    pub const CAPTURE_RECORDING_COMPLETED: &str = "capture.recording_completed";
    pub const CAPTURE_RECORDING_FAILED: &str = "capture.recording_failed";
    pub const CAPTURE_RECORDING_HUNG: &str = "capture.recording_hung";
    pub const CAPTURE_COOLDOWN_SKIP: &str = "capture.cooldown_skip";
    pub const CAPTURE_DEGRADED_SKIP: &str = "capture.degraded_skip";
    pub const CAPTURE_DISK_PRESSURE_SKIP: &str = "capture.disk_pressure_skip";
    /// Internal probe failure: the pre-record `staging_bytes` probe
    /// returned an I/O error (not bytes-over-ceiling). The trigger
    /// is NOT skipped — the supervisor falls through to the recording
    /// attempt and lets the camera adapter surface any further error.
    pub const CAPTURE_STAGING_PROBE_FAILED: &str = "capture.staging_probe_failed";
    pub const CAPTURE_QUEUE_REFUSED: &str = "capture.queue_refused";
    pub const CAPTURE_SENSOR_DEGRADED: &str = "capture.sensor_degraded";
    pub const CAPTURE_SENSOR_RECOVERED: &str = "capture.sensor_recovered";
}

/// In-memory registry of strings that must never appear in any log event.
///
/// Populated incrementally by producers ([`register_secret`]); consumed
/// by [`RedactingWriter`] on every stderr write to scrub any registered
/// marker before bytes leave the process.
#[derive(Debug, Default)]
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

    /// Snapshot of every registered secret. Cloned out from under the
    /// lock so the redaction writer can iterate without holding it.
    /// Callers should not retain the result across mutations — at most a
    /// few hundred bytes for this codebase, copied per stderr write.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.secrets.lock().expect("registry lock poisoned").clone()
    }
}

/// Process-wide redaction registry.
///
/// Populated incrementally as secrets are materialised — QR decode adds
/// the `auth_token`, CSR generation adds the CSR PEM body and the
/// station private-key PEM body, identity load (in `serve`) adds the
/// on-disk station-key body. The [`RedactingMakeWriter`] reads from this
/// registry on every write to stderr, so any secret registered before a
/// log line is emitted is guaranteed not to appear on the wire.
pub fn redaction_registry() -> &'static Arc<RedactionRegistry> {
    static REGISTRY: OnceLock<Arc<RedactionRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Arc::new(RedactionRegistry::new()))
}

/// Convenience wrapper for `redaction_registry().register(s)`. Accepts
/// anything that can become a `String` so call-sites can hand in
/// `&str`/`String` interchangeably.
pub fn register_secret(s: impl Into<String>) {
    redaction_registry().register(s);
}

/// `MakeWriter` that hands out [`RedactingWriter`] instances pointed at
/// the process-wide redaction registry. Sits in the `tracing-subscriber`
/// fmt-layer slot the bare `std::io::stderr` would otherwise occupy.
#[derive(Debug, Clone)]
pub struct RedactingMakeWriter {
    registry: Arc<RedactionRegistry>,
}

impl RedactingMakeWriter {
    #[must_use]
    pub fn new(registry: Arc<RedactionRegistry>) -> Self {
        Self { registry }
    }
}

impl<'a> MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter { registry: self.registry.clone() }
    }
}

/// `Write` adapter that scrubs registered secrets from every UTF-8
/// payload before forwarding to stderr.
///
/// Each `tracing-subscriber` fmt event emits one `write_all` call with
/// the complete formatted line + trailing newline. We snapshot the
/// registry at the start of `write`, decode the input lossily as UTF-8,
/// and replace every registered marker with `[REDACTED]`. Non-UTF-8
/// inputs (impossible for the fmt JSON layer in practice, but theoretically
/// possible for arbitrary writers) are passed through unchanged — the
/// `from_utf8_lossy` path already produces a String that can be scanned.
#[derive(Debug)]
pub struct RedactingWriter {
    registry: Arc<RedactionRegistry>,
}

impl Write for RedactingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let secrets = self.registry.snapshot();
        let mut stderr = io::stderr().lock();
        if secrets.is_empty() {
            stderr.write_all(buf)?;
            return Ok(buf.len());
        }
        let text = String::from_utf8_lossy(buf);
        let scrubbed = scrub(&text, &secrets);
        stderr.write_all(scrubbed.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if !secret.is_empty() && out.contains(secret) {
            out = out.replace(secret, REDACTED_PLACEHOLDER);
        }
    }
    out
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
        assert_eq!(events::CAPTURE_INIT_FAILED, "capture.init_failed");
        assert_eq!(events::CAPTURE_SKIPPED, "capture.skipped");
        assert_eq!(events::CAPTURE_STAGING_PROBE_FAILED, "capture.staging_probe_failed");
    }
}
