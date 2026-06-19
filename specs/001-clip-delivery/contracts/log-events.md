# Contract: Structured log events

**Direction**: station → operator (via stderr → journald).
**Format**: one JSON object per line, UTF-8, no embedded newlines.
**Producer**: `tracing` + `tracing-subscriber` JSON formatter.

Logs are the station's only outbound observability channel. They satisfy
constitution Principle IV (Observable, Not Chatty) and spec FR-010, US3.

## Common fields

Every event carries:

| Field           | Type           | Notes                                                                |
| --------------- | -------------- | -------------------------------------------------------------------- |
| `timestamp`     | RFC 3339 UTC   | Captured by the subscriber, not the producer                         |
| `level`         | string         | `trace`/`debug`/`info`/`warn`/`error`                                |
| `target`        | string         | `tracing` target, e.g. `perchstation_core::delivery`                 |
| `message`       | string         | Human-readable summary                                               |
| `event`         | string         | Stable machine-readable event code (see below)                       |
| `station_id`    | UUID, optional | Present once enrollment has been loaded                              |
| `span.clip_id`  | string, opt.   | Present inside the per-clip delivery span                            |

`event` is the contract surface for downstream tooling — its set is
listed below and changes only with an explicit version bump.

## Event codes

### Enrollment

| `event`                       | Level  | Required fields beyond common         | Triggered when                                            |
| ----------------------------- | ------ | -------------------------------------- | --------------------------------------------------------- |
| `enrollment.qr_decoded`       | info   | `session_id`                          | QR successfully decoded into session material             |
| `enrollment.csr_generated`    | info   | (none)                                 | Keypair + CSR built in memory                             |
| `enrollment.sent`             | info   | `session_id`, `perchpub_url`          | `POST /enrollment/confirm` returned 200                   |
| `enrollment.persisted`        | info   | `station_id`, `cert_not_after`        | Credentials written to disk                               |
| `enrollment.refused`          | warn   | `reason`                              | `EnrollmentResponse.success == false`                     |
| `enrollment.refused_overwrite`| error  | `existing_station_id`                 | `enroll` invoked with credentials present, no `--force`   |
| `enrollment.overwritten`      | warn   | `previous_station_id`, `station_id`   | `enroll --force` replaced an existing identity (audit trail) |
| `enrollment.failed`           | error  | `kind`, `message`                     | Network/TLS/validation failure (no credentials written)   |
| `enrollment.session_invalid`  | error  | `status`                              | perchpub rejected the enrollment session (422); operator must restart enrollment |

### Queue

| `event`                       | Level  | Required fields beyond common         | Triggered when                                            |
| ----------------------------- | ------ | -------------------------------------- | --------------------------------------------------------- |
| `queue.enqueued`              | info   | `clip_id`, `byte_size`                | Clip moved into `pending/`                                |
| `queue.recovered_inflight`    | warn   | `clip_id`                              | Boot reconciliation re-queued an `inflight/` entry        |
| `queue.evicted`               | warn   | `clip_id`, `reason`, `policy`, `remaining_clips`, `remaining_bytes` | Eviction policy dropped a clip                             |
| `queue.zero_length_skipped`   | warn   | `clip_id`, `kind`                      | Local pre-flight detected a zero-length or unreadable clip |
| `queue.disk_full`             | error  | `path`                                 | Queue write returned `ENOSPC`; runner backs off            |

### Delivery

| `event`                       | Level  | Required fields beyond common         | Triggered when                                            |
| ----------------------------- | ------ | -------------------------------------- | --------------------------------------------------------- |
| `delivery.attempt_started`    | info   | `clip_id`, `attempt`                  | Entry transitioned `pending/` → `inflight/`               |
| `delivery.upload_succeeded`   | info   | `clip_id`, `classify_task_id`, `attempt`, `duration_ms` | 200 from `/upload/`                                       |
| `delivery.upload_undecodable` | warn   | `clip_id`, `attempt`, `message`       | 2xx accepted but classify-task body undecodable (PS-06); entry → `Delivered`, classify status unknown |
| `delivery.upload_transient`   | warn   | `clip_id`, `attempt`, `kind`, `status?`, `next_attempt_after` | Retryable failure                                         |
| `delivery.upload_terminal`    | error  | `clip_id`, `attempt`, `kind`, `status?`, `message` | Non-retryable failure; entry → `Undeliverable`            |
| `delivery.attempts_exhausted` | error  | `clip_id`, `attempts`, `wallclock_secs`| Per-clip retry budget exhausted; entry → `Undeliverable`  |
| `delivery.cert_expired`       | error  | `cert_not_after`                       | Pre-flight cert check failed; loop halts                  |

### Classify-task polling

| `event`                       | Level  | Required fields beyond common         | Triggered when                                            |
| ----------------------------- | ------ | -------------------------------------- | --------------------------------------------------------- |
| `classify.polled`             | debug  | `clip_id`, `classify_task_id`, `status`| Successful poll, non-terminal                             |
| `classify.terminal`           | info   | `clip_id`, `classify_task_id`, `status`, `observation_id?` | Poll observed `Success` or `Failed`                       |
| `classify.lost`               | error  | `clip_id`, `classify_task_id`, `kind`, `status?` | 404/422, other terminal poll failure, or `kind=poll_timeout` once the finite poll budget is exhausted (PS-06) |

### Lifecycle

| `event`                       | Level  | Required fields beyond common         | Triggered when                                            |
| ----------------------------- | ------ | -------------------------------------- | --------------------------------------------------------- |
| `service.ready`               | info   | `pending_at_start`                    | After boot reconciliation, just before `sd_notify(READY)` |
| `service.shutdown`            | info   | `reason`                              | Clean shutdown (SIGTERM)                                  |
| `service.config_invalid`      | error  | `path`, `message`                     | Config load failed                                        |
| `service.credentials_reloaded`| info   | —                                     | SIGHUP hot-reloaded the mTLS credentials after re-enrollment (PS-18) |

## Field discipline

Producers MUST NOT log:

- `EnrollmentSessionMaterial.auth_token`
- `csr_pem`
- `station.key` (or any PEM body of the private key)
- Clip media bytes
- Any header named `Authorization`, `Cookie`, or `Set-Cookie`

Test obligation: `tests/integration/log_redaction.rs` injects known-secret
values into the materials and asserts that no log line emitted under any
event above contains them.

## Verbose mode

`RUST_LOG=trace` (or `--log-level trace`) enables additional `trace`-level
events under the `perchstation_core::*` targets, but never adds fields to
the events listed above and never emits any field listed in
"Field discipline". Verbose mode does NOT activate any HTTP wire-trace
that would risk leaking certificates or session tokens.

## Versioning

The event-code set above is the contract surface. Adding new codes is a
minor change; removing or renaming a code is a breaking change and
requires a constitution-level note in the next plan that touches the
delivery subsystem.
