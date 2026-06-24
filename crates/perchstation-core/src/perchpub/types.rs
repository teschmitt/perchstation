//! Mirrors of the perchpub `OpenAPI` schemas the station consumes.
//!
//! Drift between this module and `references/openapi.json` is a bug; the
//! `tests/contract/openapi_sync.rs` test enforces field-set and type-kind
//! parity at workspace test time.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Lenient `DateTime<Utc>` deserialisers for perchpub response bodies.
///
/// perchpub serialises its timestamps from naive (timezone-less) DB values,
/// e.g. `2026-06-24T09:16:24.658315` — structurally valid JSON, but a strict
/// `DateTime<Utc>` rejects the missing offset: chrono's RFC3339 parser runs off
/// the end looking for the zone and returns `TOO_SHORT`, whose Display is
/// "premature end of input" — which the station otherwise mistakes for a
/// truncated/undecodable response (PS-06, observed live 2026-06-24). These
/// accept either an offset-bearing RFC3339 timestamp (the contract's
/// `date-time`) or a tz-less one interpreted as UTC, so the station tolerates
/// both regardless of how perchpub is configured.
mod flexible_datetime {
    use chrono::{DateTime, NaiveDateTime, Utc};
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer};

    /// Naive (offset-less, UTC-assumed) timestamp layouts perchpub emits.
    const NAIVE_FORMATS: [&str; 3] =
        ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S%.f"];

    fn parse(raw: &str) -> Option<DateTime<Utc>> {
        // Prefer a timezone-aware RFC3339 timestamp (the contract's `date-time`).
        if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
            return Some(dt.with_timezone(&Utc));
        }
        // Fall back to a tz-less timestamp, interpreted as UTC.
        NAIVE_FORMATS
            .iter()
            .find_map(|fmt| NaiveDateTime::parse_from_str(raw, fmt).ok())
            .map(|naive| DateTime::from_naive_utc_and_offset(naive, Utc))
    }

    pub(super) fn required<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let raw = String::deserialize(d)?;
        parse(&raw).ok_or_else(|| D::Error::custom(format!("unrecognised datetime `{raw}`")))
    }

    pub(super) fn optional<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<DateTime<Utc>>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(raw) => parse(&raw)
                .map(Some)
                .ok_or_else(|| D::Error::custom(format!("unrecognised datetime `{raw}`"))),
            None => Ok(None),
        }
    }
}

/// `POST /api/v1/enrollment/confirm/{session_id}` request body.
///
/// `EnrollmentRequest` in `references/openapi.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentRequest {
    pub auth_token: String,
    pub csr_pem: String,
}

/// `POST /api/v1/enrollment/confirm/{session_id}` response body.
///
/// `EnrollmentResponse` in `references/openapi.json`.
///
/// On the success path, `certificate_pem`, `ca_chain_pem`, and `station_id`
/// are all populated. On the failure path (server-side rejection),
/// `success == false` and `reason` carries the human-readable cause.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentResponse {
    pub success: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub certificate_pem: Option<String>,
    #[serde(default)]
    pub ca_chain_pem: Option<String>,
    #[serde(default)]
    pub station_id: Option<Uuid>,
}

/// Lifecycle status of a classification task in perchpub.
///
/// `ClassifyTaskStatus` in `references/openapi.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClassifyTaskStatus {
    Prepared,
    Queued,
    Processing,
    Success,
    Failed,
    /// Any status string perchpub returns that this station does not model
    /// (e.g. a future `Cancelled`). PS-06: a 200 carrying an unknown status
    /// must still deserialise — collapsing it to `Decode → Transient` would
    /// re-upload an already-accepted clip / poll forever. Stays
    /// non-terminal so a genuinely-still-running unknown keeps polling
    /// (bounded by the poller's finite budget).
    #[serde(other)]
    Unknown,
}

impl ClassifyTaskStatus {
    /// `true` when no further perchpub-side transitions are possible.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Failed)
    }
}

/// Response from `POST /api/v1/upload/` and `GET /api/v1/classify-task/{id}`.
///
/// `ClassifyTaskPublic` in `references/openapi.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifyTaskPublic {
    pub object_name: String,
    #[serde(default = "default_classify_status")]
    pub status: ClassifyTaskStatus,
    pub id: Uuid,
    pub upload: UploadPublic,
    pub observation: Option<ObservationPublic>,
}

const fn default_classify_status() -> ClassifyTaskStatus {
    ClassifyTaskStatus::Prepared
}

/// Nested upload metadata inside [`ClassifyTaskPublic`].
///
/// `UploadPublic` in `references/openapi.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadPublic {
    pub station_id: Uuid,
    pub object_name: String,
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default, deserialize_with = "flexible_datetime::optional")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "flexible_datetime::optional")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Observation joined into [`ClassifyTaskPublic`] once perchpub has run the
/// classifier on the uploaded clip.
///
/// `ObservationPublic` in `references/openapi.json`.
///
/// The station deliberately does not model the nested `species` /
/// `station` types — those are perchpub-side concerns and may grow fields
/// over time. They are passed through as opaque JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationPublic {
    pub confidence_score: Option<f64>,
    pub classification_result: Option<serde_json::Value>,
    pub id: Uuid,
    pub species: Option<serde_json::Value>,
    pub station: serde_json::Value,
    #[serde(deserialize_with = "flexible_datetime::required")]
    pub observed_at: DateTime<Utc>,
    pub object_name: String,
}

/// Standard `FastAPI` 4xx validation body. `HTTPValidationError` in
/// `references/openapi.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HTTPValidationError {
    #[serde(default)]
    pub detail: Option<Vec<ValidationError>>,
}

/// Element of `HTTPValidationError.detail`. `ValidationError` in
/// `references/openapi.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    /// `FastAPI` emits `loc` as a heterogeneous list of strings and
    /// integers (e.g. `["body", "csr_pem"]` or `["body", "items", 3]`).
    pub loc: Vec<serde_json::Value>,
    pub msg: String,
    #[serde(rename = "type")]
    pub type_: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_task_status_terminal_set() {
        assert!(ClassifyTaskStatus::Success.is_terminal());
        assert!(ClassifyTaskStatus::Failed.is_terminal());
        assert!(!ClassifyTaskStatus::Prepared.is_terminal());
        assert!(!ClassifyTaskStatus::Queued.is_terminal());
        assert!(!ClassifyTaskStatus::Processing.is_terminal());
        assert!(!ClassifyTaskStatus::Unknown.is_terminal());
    }

    /// Real `POST /api/v1/upload/` 200 body captured live on 2026-06-24:
    /// perchpub serialises `created_at`/`updated_at` **without a timezone**
    /// (`...658315`, no `Z`). The body is complete and valid JSON, but a strict
    /// `DateTime<Utc>` cannot parse a tz-less timestamp — chrono returns its
    /// `TOO_SHORT` error whose Display is "premature end of input", which the
    /// station previously mistook for a truncated/undecodable response (PS-06).
    /// The station must tolerate naive (UTC-assumed) timestamps.
    const LIVE_UPLOAD_BODY_NAIVE_TS: &str = r#"{"object_name":"f3/41/f34162a311610fbbafbc1d47780198e70a9b56926927c199acc1c0f58ed7930e.mp4","status":"Prepared","id":"59371497-d528-404d-a653-2c3bf694d526","upload":{"station_id":"50d40ccc-0ba4-4614-b2ef-ac002ede62d7","object_name":"f3/41/f34162a311610fbbafbc1d47780198e70a9b56926927c199acc1c0f58ed7930e.mp4","id":"5a0bf864-ea7c-452e-95ae-b2fd4922685d","created_at":"2026-06-24T09:16:24.658315","updated_at":"2026-06-24T09:16:24.658322"},"observation":null}"#;

    #[test]
    fn upload_response_with_timezoneless_timestamps_decodes() {
        let task: ClassifyTaskPublic = serde_json::from_str(LIVE_UPLOAD_BODY_NAIVE_TS)
            .expect("a complete 200 body with naive UTC timestamps must decode");
        assert_eq!(task.status, ClassifyTaskStatus::Prepared);
        let created = task.upload.created_at.expect("created_at present");
        // The naive timestamp is interpreted as UTC.
        assert_eq!(created.to_rfc3339(), "2026-06-24T09:16:24.658315+00:00");
        assert!(task.upload.updated_at.is_some());
    }

    #[test]
    fn upload_response_with_offset_timestamps_still_decodes() {
        // A timezone-aware (`Z`) timestamp — the contract's `date-time` — must
        // keep working, so the fix tolerates *both* formats.
        let json = r#"{"object_name":"c.mp4","status":"Prepared","id":"00000000-0000-0000-0000-000000000010","upload":{"station_id":"00000000-0000-0000-0000-000000000001","object_name":"c.mp4","created_at":"2026-06-24T09:16:24.658315Z"},"observation":null}"#;
        let task: ClassifyTaskPublic =
            serde_json::from_str(json).expect("offset timestamp decodes");
        assert_eq!(
            task.upload.created_at.unwrap().to_rfc3339(),
            "2026-06-24T09:16:24.658315+00:00"
        );
    }

    #[test]
    fn classify_poll_observation_with_naive_observed_at_decodes() {
        // The classify poller reads the same naive-timestamp serialisation in
        // the (required) `observation.observed_at` — it must decode too, or a
        // successful classification would be unreadable for the same reason.
        let json = r#"{"object_name":"c.mp4","status":"Success","id":"00000000-0000-0000-0000-000000000010","upload":{"station_id":"00000000-0000-0000-0000-000000000001","object_name":"c.mp4"},"observation":{"confidence_score":0.93,"classification_result":null,"id":"00000000-0000-0000-0000-000000000020","species":null,"station":{},"observed_at":"2026-06-24T07:27:01.306942","object_name":"c.mp4"}}"#;
        let task: ClassifyTaskPublic = serde_json::from_str(json)
            .expect("populated observation with naive observed_at decodes");
        let obs = task.observation.expect("observation present");
        assert_eq!(obs.observed_at.to_rfc3339(), "2026-06-24T07:27:01.306942+00:00");
    }

    #[test]
    fn classify_status_unknown_deserialises() {
        // An unknown status string (e.g. a future `Cancelled`) must map to
        // `Unknown`, never fail the whole deserialise (PS-06).
        let status: ClassifyTaskStatus = serde_json::from_str(r#""Cancelled""#).unwrap();
        assert_eq!(status, ClassifyTaskStatus::Unknown);
        assert!(!status.is_terminal());
    }

    #[test]
    fn classify_task_public_with_unknown_status_deserialises() {
        // A full 200 body whose `status` is unmodelled still parses.
        let json = r#"{
            "object_name": "clip-1.mp4",
            "id": "00000000-0000-0000-0000-000000000010",
            "status": "Cancelled",
            "upload": {
                "station_id": "00000000-0000-0000-0000-000000000001",
                "object_name": "clip-1.mp4"
            },
            "observation": null
        }"#;
        let task: ClassifyTaskPublic = serde_json::from_str(json).unwrap();
        assert_eq!(task.status, ClassifyTaskStatus::Unknown);
    }

    #[test]
    fn enrollment_request_round_trip() {
        let payload = r#"{"auth_token":"tok","csr_pem":"-----BEGIN ...-----"}"#;
        let req: EnrollmentRequest = serde_json::from_str(payload).unwrap();
        assert_eq!(req.auth_token, "tok");
        assert!(req.csr_pem.starts_with("-----BEGIN"));
    }

    #[test]
    fn enrollment_response_only_success_required() {
        // Only `success` is in the OpenAPI required list.
        let json = r#"{"success": false}"#;
        let resp: EnrollmentResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.success);
        assert_eq!(resp.reason, "");
        assert!(resp.certificate_pem.is_none());
    }

    #[test]
    fn enrollment_response_success_path() {
        let json = r#"{
            "success": true,
            "reason": "",
            "certificate_pem": "-----BEGIN CERTIFICATE-----\nfoo\n-----END CERTIFICATE-----\n",
            "ca_chain_pem":    "-----BEGIN CERTIFICATE-----\nbar\n-----END CERTIFICATE-----\n",
            "station_id":      "00000000-0000-0000-0000-000000000001"
        }"#;
        let resp: EnrollmentResponse = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        assert!(resp.certificate_pem.is_some());
        assert!(resp.ca_chain_pem.is_some());
        assert!(resp.station_id.is_some());
    }

    #[test]
    fn classify_task_public_deserialises_with_default_status() {
        let json = r#"{
            "object_name": "clip-1.mp4",
            "id": "00000000-0000-0000-0000-000000000010",
            "upload": {
                "station_id": "00000000-0000-0000-0000-000000000001",
                "object_name": "clip-1.mp4"
            },
            "observation": null
        }"#;
        let task: ClassifyTaskPublic = serde_json::from_str(json).unwrap();
        assert_eq!(task.status, ClassifyTaskStatus::Prepared);
        assert_eq!(task.object_name, "clip-1.mp4");
        assert!(task.upload.id.is_none());
    }

    #[test]
    fn http_validation_error_parses_fastapi_body() {
        let json = r#"{
            "detail": [
                {"loc": ["body", "csr_pem"], "msg": "field required", "type": "value_error.missing"}
            ]
        }"#;
        let body: HTTPValidationError = serde_json::from_str(json).unwrap();
        let detail = body.detail.unwrap();
        assert_eq!(detail.len(), 1);
        assert_eq!(detail[0].type_, "value_error.missing");
    }
}
