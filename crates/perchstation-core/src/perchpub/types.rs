//! Mirrors of the perchpub `OpenAPI` schemas the station consumes.
//!
//! Drift between this module and `references/openapi.json` is a bug; the
//! `tests/contract/openapi_sync.rs` test enforces field-set and type-kind
//! parity at workspace test time.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
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
