//! Queue layer: directory-of-files state machine for clips waiting to upload.
//!
//! Layout (per `data-model.md`):
//!
//! ```text
//! <data_dir>/queue/
//! ├── pending/   <clip-id>.mp4 + <clip-id>.json
//! ├── inflight/  <clip-id>.mp4 + <clip-id>.json
//! └── delivered/ <clip-id>.json  (mp4 unlinked on success)
//! ```
//!
//! State is encoded by which directory the entry lives in (`Pending`/`Inflight`)
//! plus, for entries in `delivered/`, the `outcome` field distinguishing
//! `Delivered` from `Undeliverable`.

pub mod inbox;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::perchpub::types::ClassifyTaskStatus;

/// Terminal disposition for entries that have reached `delivered/`.
///
/// `Delivered` — perchpub accepted the upload and returned a classify-task id.
/// `Undeliverable` — local pre-flight or perchpub returned a terminal error;
/// the clip will never be uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Delivered,
    Undeliverable,
}

/// Structured failure context attached to a [`ClipQueueEntry`] when an
/// attempt failed. Cleared on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastError {
    /// Short stable identifier — `"network"`, `"http_status"`, `"zero_length"`,
    /// `"disk_full"`, `"validation"`, etc. Used by `status` and by tests to
    /// classify the failure without parsing `message`.
    pub kind: String,
    /// HTTP status, when the failure is a non-2xx response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub message: String,
}

/// Sidecar JSON record persisted alongside each clip media file. Mirrors the
/// fields documented in `specs/001-clip-delivery/data-model.md` §Entity:
/// `ClipQueueEntry`.
///
/// All optional fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`
/// so the on-disk JSON stays compact and the upstream capture subsystem can
/// produce a minimal sidecar with just the required fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipQueueEntry {
    pub clip_id: String,
    pub captured_at: DateTime<Utc>,
    pub enqueued_at: DateTime<Utc>,
    pub byte_size: u64,
    pub attempts: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_attempt_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LastError>,

    /// `Some` while the entry is in `pending/` and a backoff is in effect;
    /// `None` for fresh entries and after success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_after: Option<DateTime<Utc>>,

    /// `Some` only when the sidecar lives in `delivered/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classify_task_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_classify_status: Option<ClassifyTaskStatus>,
}

impl ClipQueueEntry {
    /// Minimal constructor used by the capture-side `Inbox` and by tests.
    #[must_use]
    pub fn new(
        clip_id: impl Into<String>,
        captured_at: DateTime<Utc>,
        enqueued_at: DateTime<Utc>,
        byte_size: u64,
    ) -> Self {
        Self {
            clip_id: clip_id.into(),
            captured_at,
            enqueued_at,
            byte_size,
            attempts: 0,
            first_attempt_at: None,
            last_attempt_at: None,
            last_error: None,
            next_attempt_after: None,
            outcome: None,
            classify_task_id: None,
            delivered_at: None,
            last_classify_status: None,
        }
    }

    /// `true` once `outcome` has been set — i.e., the entry is in
    /// `delivered/` with a final disposition.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.outcome.is_some()
    }
}

/// Errors surfaced by [`store`] and [`inbox`] operations on the queue.
#[derive(Debug, Error)]
pub enum QueueError {
    #[error("queue I/O error at `{path}`: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not serialise sidecar JSON: {0}")]
    Serialise(#[source] serde_json::Error),
    #[error("could not deserialise sidecar JSON at `{path}`: {source}")]
    Deserialise {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("queue entry `{clip_id}` is missing its `.mp4` media file")]
    MissingMedia { clip_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn instant(ts: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(ts).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn minimal_sidecar_round_trips_with_optionals_absent() {
        let json_text = r#"{
            "clip_id": "20260527T120000Z-001",
            "captured_at": "2026-05-27T12:00:00Z",
            "enqueued_at": "2026-05-27T12:00:00Z",
            "byte_size": 4096,
            "attempts": 0
        }"#;
        let entry: ClipQueueEntry = serde_json::from_str(json_text).unwrap();
        assert_eq!(entry.clip_id, "20260527T120000Z-001");
        assert_eq!(entry.byte_size, 4096);
        assert!(entry.outcome.is_none());
        assert!(entry.classify_task_id.is_none());

        let reserialised = serde_json::to_value(&entry).unwrap();
        assert!(reserialised.get("first_attempt_at").is_none());
        assert!(reserialised.get("outcome").is_none());
        assert!(reserialised.get("classify_task_id").is_none());
    }

    #[test]
    fn delivered_sidecar_round_trips_with_terminal_fields() {
        let mut entry = ClipQueueEntry::new(
            "clip-1",
            instant("2026-05-27T12:00:00Z"),
            instant("2026-05-27T12:00:01Z"),
            1024,
        );
        entry.attempts = 1;
        entry.first_attempt_at = Some(instant("2026-05-27T12:00:02Z"));
        entry.last_attempt_at = Some(instant("2026-05-27T12:00:02Z"));
        entry.outcome = Some(Outcome::Delivered);
        entry.classify_task_id = Some(Uuid::from_u128(1));
        entry.delivered_at = Some(instant("2026-05-27T12:00:03Z"));
        entry.last_classify_status = Some(ClassifyTaskStatus::Prepared);

        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["outcome"], json!("Delivered"));
        assert_eq!(v["classify_task_id"], json!(Uuid::from_u128(1)));
        assert_eq!(v["last_classify_status"], json!("Prepared"));

        let decoded: ClipQueueEntry = serde_json::from_value(v).unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn outcome_serialises_to_capitalised_string() {
        let v = serde_json::to_value(Outcome::Delivered).unwrap();
        assert_eq!(v, json!("Delivered"));
        let v = serde_json::to_value(Outcome::Undeliverable).unwrap();
        assert_eq!(v, json!("Undeliverable"));
    }

    #[test]
    fn last_error_serialises_with_optional_status() {
        let err = LastError {
            kind: "http_status".into(),
            status: Some(503),
            message: "Service Unavailable".into(),
        };
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "http_status");
        assert_eq!(v["status"], 503);

        let err = LastError { kind: "network".into(), status: None, message: "timeout".into() };
        let v = serde_json::to_value(&err).unwrap();
        assert!(v.get("status").is_none(), "None status should be skipped");
    }

    #[test]
    fn is_terminal_tracks_outcome_presence() {
        let mut entry = ClipQueueEntry::new(
            "x",
            Utc.timestamp_opt(0, 0).unwrap(),
            Utc.timestamp_opt(0, 0).unwrap(),
            0,
        );
        assert!(!entry.is_terminal());
        entry.outcome = Some(Outcome::Undeliverable);
        assert!(entry.is_terminal());
    }
}
