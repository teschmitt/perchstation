//! Contract: the mirrored `perchpub::types` schemas in `perchstation-core`
//! match `references/openapi.json` field-for-field.
//!
//! This test deserialises the `OpenAPI` document, walks the
//! `components.schemas.<Name>` object for each schema enumerated in
//! `contracts/perchpub-api.md` §Schemas, and asserts:
//!
//! 1. The schema exists.
//! 2. Its property set is exactly the field set we expect.
//! 3. Each property's "kind" (string / integer / object / array / enum /
//!    optional-of-X) matches the local Rust type.
//!
//! Drift between perchpub and this station fails the test loudly with a
//! diff. Reconcile by updating either side — but never silently.

use std::collections::BTreeMap;

use serde_json::Value;

const OPENAPI_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../references/openapi.json");

const TYPES_TO_CHECK: &[&str] = &[
    "EnrollmentRequest",
    "EnrollmentResponse",
    "ClassifyTaskPublic",
    "ClassifyTaskStatus",
    "UploadPublic",
    "ObservationPublic",
    "HTTPValidationError",
    "ValidationError",
];

/// Coarse-grained field kind. Captures only the distinctions that the
/// station's local mirror cares about — finer detail (e.g. UUID vs plain
/// string, date-time vs plain string) is intentionally lossy because the
/// Rust mirror treats those as `Uuid` / `DateTime<Utc>` and the contract
/// is satisfied as long as the underlying JSON shape is "string".
#[derive(Debug, PartialEq, Eq, Clone)]
enum Kind {
    String,
    Integer,
    Number,
    Boolean,
    Array,
    /// `$ref` to another schema, or an inline `object`. We don't dive into
    /// the referenced schema — that schema, if listed in
    /// `TYPES_TO_CHECK`, gets its own assertions.
    Object,
    /// `anyOf` containing `null` plus one other kind ⇒ `Option<T>` in Rust.
    Optional(Box<Kind>),
}

fn load_openapi() -> Value {
    let raw = std::fs::read_to_string(OPENAPI_PATH).unwrap_or_else(|e| {
        panic!("could not read {OPENAPI_PATH}: {e}");
    });
    serde_json::from_str(&raw).expect("openapi.json is valid JSON")
}

/// Walk an `OpenAPI` `properties` map and return field-name → kind in
/// alphabetical order.
fn extract_properties(schema: &Value) -> BTreeMap<String, Kind> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (name, def) in props {
        out.insert(name.clone(), classify_field(def));
    }
    out
}

fn classify_field(def: &Value) -> Kind {
    if def.get("$ref").is_some() {
        return Kind::Object;
    }
    if let Some(any_of) = def.get("anyOf").and_then(Value::as_array) {
        // anyOf with null + one other kind ⇒ Optional(other)
        let has_null = any_of.iter().any(|v| v.get("type") == Some(&Value::String("null".into())));
        let other: Vec<&Value> = any_of
            .iter()
            .filter(|v| v.get("type") != Some(&Value::String("null".into())))
            .collect();
        if has_null && other.len() == 1 {
            return Kind::Optional(Box::new(classify_field(other[0])));
        }
        if has_null && other.iter().all(|v| v.get("type") == other[0].get("type")) {
            // anyOf containing null + several variants of the same primitive
            return Kind::Optional(Box::new(classify_field(other[0])));
        }
        if !has_null {
            // Heterogeneous anyOf without null — used inside `ValidationError.loc`
            // for "string or integer" items. We treat the outer kind as the
            // first variant's kind; the loc test below covers this specially.
            return classify_field(other.first().copied().unwrap_or(def));
        }
    }
    match def.get("type").and_then(Value::as_str) {
        Some("string") => Kind::String,
        Some("integer") => Kind::Integer,
        Some("number") => Kind::Number,
        Some("boolean") => Kind::Boolean,
        Some("array") => Kind::Array,
        // `Some("object")` and the unknown/$ref branch both collapse to
        // `Kind::Object` — the contract test does not need to distinguish
        // between an inline object body and a `$ref` to another schema.
        _ => Kind::Object,
    }
}

/// Hand-coded mirror of the local Rust type fields. Each tuple is
/// `(field_name, kind)`. Matches the property names + JSON kinds the
/// `perchstation_core::perchpub::types` module declares.
fn expected_fields(type_name: &str) -> BTreeMap<String, Kind> {
    let pairs: &[(&str, Kind)] = match type_name {
        "EnrollmentRequest" => &[("auth_token", Kind::String), ("csr_pem", Kind::String)],
        "EnrollmentResponse" => &[
            ("success", Kind::Boolean),
            ("reason", Kind::String),
            ("certificate_pem", Kind::Optional(Box::new(Kind::String))),
            ("ca_chain_pem", Kind::Optional(Box::new(Kind::String))),
            ("station_id", Kind::Optional(Box::new(Kind::String))),
        ],
        "ClassifyTaskPublic" => &[
            ("object_name", Kind::String),
            ("status", Kind::Object), // $ref ClassifyTaskStatus
            ("id", Kind::String),
            ("upload", Kind::Object), // $ref UploadPublic
            ("observation", Kind::Optional(Box::new(Kind::Object))), // anyOf $ref ObservationPublic | null
        ],
        "UploadPublic" => &[
            ("station_id", Kind::String),
            ("object_name", Kind::String),
            ("id", Kind::String),
            ("created_at", Kind::String),
            ("updated_at", Kind::String),
        ],
        "ObservationPublic" => &[
            ("confidence_score", Kind::Optional(Box::new(Kind::Number))),
            ("classification_result", Kind::Optional(Box::new(Kind::Array))),
            ("id", Kind::String),
            ("species", Kind::Optional(Box::new(Kind::Object))),
            ("station", Kind::Object),
            ("observed_at", Kind::String),
            ("object_name", Kind::String),
        ],
        "HTTPValidationError" => &[("detail", Kind::Array)],
        "ValidationError" => &[("loc", Kind::Array), ("msg", Kind::String), ("type", Kind::String)],
        "ClassifyTaskStatus" => &[], // handled separately as an enum
        other => panic!("expected_fields: unhandled schema {other}"),
    };
    pairs.iter().cloned().map(|(k, v)| (k.to_string(), v)).collect()
}

#[test]
fn all_documented_schemas_are_present() {
    let openapi = load_openapi();
    let schemas = openapi["components"]["schemas"]
        .as_object()
        .expect("openapi components.schemas is an object");
    for name in TYPES_TO_CHECK {
        assert!(
            schemas.contains_key(*name),
            "schema `{name}` missing from references/openapi.json"
        );
    }
}

#[test]
fn enrollment_request_mirror_matches_openapi() {
    assert_struct_drift("EnrollmentRequest");
}

#[test]
fn enrollment_response_mirror_matches_openapi() {
    assert_struct_drift("EnrollmentResponse");
}

#[test]
fn classify_task_public_mirror_matches_openapi() {
    assert_struct_drift("ClassifyTaskPublic");
}

#[test]
fn upload_public_mirror_matches_openapi() {
    assert_struct_drift("UploadPublic");
}

#[test]
fn observation_public_mirror_matches_openapi() {
    assert_struct_drift("ObservationPublic");
}

#[test]
fn http_validation_error_mirror_matches_openapi() {
    assert_struct_drift("HTTPValidationError");
}

#[test]
fn validation_error_mirror_matches_openapi() {
    assert_struct_drift("ValidationError");
}

#[test]
fn classify_task_status_enum_matches_openapi() {
    let openapi = load_openapi();
    let schema = &openapi["components"]["schemas"]["ClassifyTaskStatus"];
    assert_eq!(schema["type"], "string", "ClassifyTaskStatus must be a string enum");
    let values: Vec<&str> = schema["enum"]
        .as_array()
        .expect("enum array")
        .iter()
        .map(|v| v.as_str().expect("enum value is string"))
        .collect();
    assert_eq!(
        values,
        vec!["Prepared", "Queued", "Processing", "Success", "Failed"],
        "ClassifyTaskStatus drift — perchstation_core::perchpub::types::ClassifyTaskStatus is out of sync",
    );
}

fn assert_struct_drift(type_name: &str) {
    let openapi = load_openapi();
    let schema = &openapi["components"]["schemas"][type_name];
    let actual = extract_properties(schema);
    let expected = expected_fields(type_name);

    let actual_names: Vec<&String> = actual.keys().collect();
    let expected_names: Vec<&String> = expected.keys().collect();
    assert_eq!(
        actual_names, expected_names,
        "{type_name}: property name set differs from local mirror"
    );

    for (name, expected_kind) in &expected {
        let actual_kind = actual.get(name).expect("name present");
        assert_eq!(
            actual_kind, expected_kind,
            "{type_name}.{name}: kind differs between OpenAPI and local mirror"
        );
    }
}
