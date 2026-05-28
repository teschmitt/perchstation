# Contract: perchpub HTTP API (consumed subset)

**Direction**: outbound (station → perchpub).
**Source of truth**: `references/openapi.json` (perchpub v0.1.0). This document
extracts the slice the delivery subsystem actually uses and records the
station-side behavioural contract on top of it.

The full set of authenticated calls the station makes is exactly three:

1. `POST /api/v1/enrollment/confirm/{session_id}` — enrollment (one-shot).
2. `POST /api/v1/upload/` — clip upload (per delivered clip).
3. `GET  /api/v1/classify-task/{id}` — poll the outcome (per delivered clip,
   until a terminal status is observed).

No other endpoint listed in `references/openapi.json` is called by the
station. The operator-facing `POST /api/v1/enrollment/create` is invoked by
the perchpub web UI, not by us.

---

## Transport and authentication

- **TLS**: TLS 1.2+ (TLS 1.3 expected in practice) terminating at perchpub's
  Traefik front. The station validates perchpub's server certificate
  against **only** the CA chain returned at enrollment time
  (`credentials/ca_chain.pem`); the OS trust store is **not** consulted.
- **Client identity**: every call **except** `/enrollment/confirm` presents
  the station's enrollment-issued certificate as a TLS client certificate
  (mTLS) signed by `station.key`. Perchpub identifies the station server-
  side from the SPKI of the presented cert; the station MUST NOT add a
  bearer token, cookie, or any other auth header.
- **`/enrollment/confirm`** is special: the station has no client certificate
  yet. The call is made over plain TLS (no client cert presented). The
  `auth_token` in the request body is the only credential. The same CA
  pinning applies.

---

## 1. Enrollment confirm

### Request

`POST /api/v1/enrollment/confirm/{session_id}`

- `session_id` (path): UUID from the QR payload.
- Body (`application/json`, schema `EnrollmentRequest`):

```json
{
  "auth_token": "<from QR payload>",
  "csr_pem":    "<freshly generated PKCS#10 CSR, PEM-encoded>"
}
```

### Responses

| Status | Body schema           | Station handling                                                                                             |
| ------ | --------------------- | ------------------------------------------------------------------------------------------------------------ |
| 200    | `EnrollmentResponse`  | If `success == true` and `certificate_pem`, `ca_chain_pem`, and `station_id` are all present, persist atomically (see `data-model.md`). Otherwise treat as failure with `reason`. |
| 422    | `HTTPValidationError` | Log structured error; do not retry (CSR is wrong; operator must re-enroll).                                  |
| 4xx (other) | (any)            | Log and abort; operator must re-enroll.                                                                      |
| 5xx / network failure | (any)     | Retry the *same* `session_id`/`auth_token` once after 5 s, then twice more at 30 s and 120 s. Beyond that, report enrollment failed and leave on-disk state untouched. |

### Behavioural contract (station side)

- The CSR sent in the body MUST be signed by `station.key` generated on
  this device during this enrollment attempt; no key reuse across attempts.
- The CSR's subject is not required to encode anything specific; perchpub
  rewrites the subject in the issued cert.
- The station MUST NOT log `auth_token` or `csr_pem` at any level.
- On a successful `EnrollmentResponse`, the station MUST validate that the
  returned `certificate_pem` chains to `ca_chain_pem` and that the cert's
  public key matches the private key the station holds. Mismatch → abort,
  no on-disk writes.

---

## 2. Clip upload

### Request

`POST /api/v1/upload/`

- TLS client certificate: required (mTLS).
- Body: `multipart/form-data` with exactly one part:
  - name `file`, filename = the station-side `<clip-id>.mp4`,
    content-type `video/mp4`, body = the clip bytes streamed from disk.

### Responses

| Status      | Body schema             | Station handling                                                                                       |
| ----------- | ----------------------- | ------------------------------------------------------------------------------------------------------ |
| 200         | `ClassifyTaskPublic`    | Record `id` as `classify_task_id` on the queue entry; mark `Delivered`; schedule classify-task poll.   |
| 408         | (any)                   | Transient: backoff and retry.                                                                          |
| 422         | `HTTPValidationError`   | Terminal for this clip: mark `Undeliverable`, log with full detail.                                    |
| 429         | (any)                   | Transient: honour `Retry-After` if present, otherwise standard backoff.                                |
| 4xx (other) | (any)                   | Terminal for this clip.                                                                                |
| 5xx         | (any)                   | Transient: backoff and retry.                                                                          |
| network err | n/a                     | Transient: backoff and retry.                                                                          |

### Behavioural contract (station side)

- The body MUST be streamed from disk; the station MUST NOT load the entire
  clip into memory.
- The station MUST validate locally before sending that the clip file is
  readable and `byte_size > 0`; failures here are recorded as
  `Undeliverable` and never reach the wire.
- The `multipart` boundary MUST be generated freshly per request.
- On 200, the station MUST persist the `classify_task_id` durably before
  unlinking the local clip media — duplicate uploads on retry are
  tolerated by the server but must not happen here purely because of a
  crash window.

---

## 3. Classify-task poll

### Request

`GET /api/v1/classify-task/{id}`

- `id` (path): the `classify_task_id` recorded after a successful upload.
- TLS client certificate: required (mTLS).
- No body.

### Responses

| Status      | Body schema           | Station handling                                                                |
| ----------- | --------------------- | ------------------------------------------------------------------------------- |
| 200         | `ClassifyTaskPublic`  | Update `last_classify_status` and `observation_id` on the delivered entry.      |
| 404         | (any)                 | Terminal: log error referencing the lost task id; stop polling that entry.      |
| 422         | `HTTPValidationError` | Terminal: log; stop polling that entry.                                         |
| 5xx / netw  | (any)                 | Transient: backoff and retry (poll-tier backoff, see retry table).              |

### Polling cadence (station side)

| Observed status                  | Next poll               |
| -------------------------------- | ----------------------- |
| `Prepared` / `Queued`            | in 30 s                 |
| `Processing`                     | in 30 s                 |
| `Success` / `Failed` (terminal)  | never (entry is final)  |
| (no response observed)           | per retry table         |

Polling respects the same outbound allowlist as upload.

---

## Schemas (mirror of `references/openapi.json`)

The station's `perchstation_core::perchpub::types` module declares
`serde::Deserialize` types that mirror the following perchpub schemas
verbatim. Drift between this list and the OpenAPI document is a bug:

- `EnrollmentRequest` (request body of `/enrollment/confirm`)
- `EnrollmentResponse` (response body of `/enrollment/confirm`)
- `ClassifyTaskPublic` (response body of `/upload/` and `/classify-task/{id}`)
- `ClassifyTaskStatus` (enum: `Prepared`, `Queued`, `Processing`, `Success`, `Failed`)
- `UploadPublic` (nested in `ClassifyTaskPublic`)
- `ObservationPublic` (nested in `ClassifyTaskPublic.observation`, nullable)
- `HTTPValidationError` + `ValidationError` (4xx body shape)

The station deliberately *does not* model the species, observation, user,
or station-administration endpoints — those are perchpub-side concerns.

---

## Test obligations

Each contract row above maps to an integration test against a fake perchpub
(axum) that returns the corresponding status code and body shape; the test
asserts that the station's queue state evolves as documented. The full test
matrix lives in `tests/integration/`.

Contract-drift test: a `tests/contract/openapi_sync.rs` test deserialises
`references/openapi.json` and asserts that the schemas the station relies
on (listed above) match field-for-field with the local mirror types.
