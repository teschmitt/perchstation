# Contract: Capture-side structured log events

**Direction**: station → operator (via stderr → journald).
**Format**: one JSON object per line, UTF-8, no embedded newlines.
**Producer**: `tracing` + `tracing-subscriber` JSON formatter, via the
same `RedactingMakeWriter` feature 001 installed.

This document **extends** the event-code contract defined in
`specs/001-clip-delivery/contracts/log-events.md`. All field-discipline
rules ("MUST NOT log auth_token, station.key body, …"), all common
fields (`timestamp`, `level`, `target`, `message`, `event`, …), and all
verbose-mode rules from 001 apply unchanged to the new codes below.

The capture subsystem introduces **no new log channel** and **no new
outbound destination**. Every event below uses `event = "capture.…"`
on the existing channel, consumable by the same journald reader an
operator already uses to read delivery and enrollment events.

## New target

| Target                                | Notes                                                  |
| ------------------------------------- | ------------------------------------------------------ |
| `perchstation_core::capture`          | All capture-loop events emit under this `tracing` target. |

## New event codes

### Lifecycle (capture loop)

| `event`                          | Level  | Required fields beyond common              | Triggered when                                                          |
| -------------------------------- | ------ | ------------------------------------------- | ----------------------------------------------------------------------- |
| `capture.ready`                  | info   | `staging_purged_files`                      | After staging-purge completes; just before the supervisor enters its `select!` loop. Mirrors `service.ready` but only for the capture half. |
| `capture.shutdown`               | info   | `reason`                                    | The supervisor's `select!` selected the shutdown branch.                |
| `capture.staging_purged`         | debug  | `removed_files`, `removed_bytes`            | The staging-purge step at boot. Emitted even if 0 files; `removed_files=0` is a legal value. |

### Recording

| `event`                          | Level  | Required fields beyond common              | Triggered when                                                          |
| -------------------------------- | ------ | ------------------------------------------- | ----------------------------------------------------------------------- |
| `capture.trigger_observed`       | debug  | `triggered_at`                              | `MotionSensor::next_trigger` resolved with `Ok`.                       |
| `capture.recording_started`      | info   | `recording_id`, `triggered_at`              | Just before `Camera::record_clip` is awaited.                           |
| `capture.recording_completed`    | info   | `recording_id`, `clip_id`, `byte_size`, `duration_ms` | The clip was handed to `Inbox::submit` and the inbox returned `Ok`. `clip_id` is the queue-side id minted by the `StoreInbox::enqueue`. |
| `capture.recording_failed`       | warn   | `recording_id`, `kind`, `message`           | `Camera::record_clip` returned `Err` (`kind` ∈ `"open_failed"`, `"io"`, `"aborted"`, `"empty_output"`). |
| `capture.recording_hung`         | error  | `recording_id`, `max_duration_ms`           | The supervisor's outer `tokio::time::timeout` fired (camera adapter exceeded `clip_duration + hang_margin`). The drop-cancellation of the future cleans up the staging file. |

### Trigger gating (reasons we observed a fresh edge but did not record)

| `event`                          | Level  | Required fields beyond common              | Triggered when                                                          |
| -------------------------------- | ------ | ------------------------------------------- | ----------------------------------------------------------------------- |
| `capture.cooldown_skip`          | debug  | `cooldown_until`                            | Trigger arrived while `CooldownState::is_active`.                       |
| `capture.degraded_skip`          | warn   | `sensor_liveness`                           | Trigger arrived while sensor liveness ∈ `{StuckAsserted, Unavailable}`. |
| `capture.disk_pressure_skip`     | warn   | `staging_bytes`, `max_staging_bytes`        | Trigger arrived but pre-record disk-pressure check refused to record.   |

### Queue refusal handoff

| `event`                          | Level  | Required fields beyond common              | Triggered when                                                          |
| -------------------------------- | ------ | ------------------------------------------- | ----------------------------------------------------------------------- |
| `capture.queue_refused`          | warn   | `recording_id`, `kind`, `current_clips?`, `max_clips?`, `current_bytes?`, `max_bytes?` | `Inbox::submit` returned `InboxError::QueueFull` (`kind: "queue_full"`) or `InboxError::Queue(_)` (`kind: "queue_io"`). The supervisor removed the staging file before emitting. (FR-018.) |

### Sensor liveness

| `event`                          | Level  | Required fields beyond common              | Triggered when                                                          |
| -------------------------------- | ------ | ------------------------------------------- | ----------------------------------------------------------------------- |
| `capture.sensor_degraded`        | warn   | `kind`, `since`, `reason?`                  | Transition `Healthy → StuckAsserted` (`kind: "stuck_asserted"`) or `* → Unavailable` (`kind: "unavailable"`, `reason: <error message>`). |
| `capture.sensor_recovered`       | info   | `kind`                                      | Transition `StuckAsserted → Healthy` or `Unavailable → Healthy`. `kind` matches the prior `capture.sensor_degraded` to make pairing trivial. |

## Field discipline (re-affirmed)

The capture subsystem MUST NOT log:

- Any field listed in `001/contracts/log-events.md` §"Field discipline".
- Raw clip bytes. (Logging the staging path is fine; logging the byte
  content is not.)
- The contents of `MotionSensor::next_trigger`'s buffered FIFO (event
  count is fine; per-edge metadata other than `triggered_at` is not
  needed and is not logged).

The test obligation from 001 (`tests/integration/log_redaction.rs`)
remains in force. Capture-side codes do not add any new
secret-bearing fields, so no new redaction sites are required.

## `kind` on `capture.recording_failed` versus `last_failure.kind`

The field name `kind` is used in two contracts with two different
vocabularies:

- **`capture.recording_failed { kind, … }`** (this document, log
  event): values are the underlying `CameraError` variant — one of
  `"open_failed"`, `"io"`, `"aborted"`, `"empty_output"`. This is the
  *cause* of the failure, observed at the camera-adapter boundary.
- **`capture.last_failure.kind`** (see `contracts/cli.md` §JSON
  output): values are the supervisor's higher-level failure category
  — one of `"recording_failed"`, `"camera_hang"`, `"queue_full"`,
  `"queue_io"`, `"disk_pressure"`. This is the *event the operator
  cares about*, surfaced through the status snapshot.

A single failure may surface in both: a camera I/O error during a
recording emits `capture.recording_failed { kind: "io", … }` in the
log and updates the snapshot to `last_failure.kind = "recording_failed"`.
Implementers MUST keep these vocabularies separate.

## `recording_id` versus `clip_id`

- `recording_id` is the **capture-side staging-file id**
  (`<capture_utc_basic>-cap`). It identifies the in-progress recording
  inside the capture subsystem.
- `clip_id` is the **queue-side id** minted by `StoreInbox::enqueue`
  (`<capture_utc_basic>-<seq>`). It identifies the entry once it lives
  in the queue.

Both appear on `capture.recording_completed` so an operator can pivot
from a `capture.*` event to a `delivery.*` / `queue.*` event with one
grep. Failure events carry only `recording_id` (because no queue entry
was ever created); queue refusals carry only `recording_id` for the
same reason.

## Verbose mode

`RUST_LOG=perchstation_core::capture=trace` enables additional
`trace`-level events under the `perchstation_core::capture` target
(per-tick liveness probe results, per-trigger arrival timing). These
are **debugging aids only** and remain subject to the field discipline
above.

## Versioning

The event-code set above is the capture subsystem's contract surface.
Adding new codes is a minor change. Removing or renaming a code is a
breaking change and requires a constitution-level note in the next
plan that touches the capture subsystem, identically to 001's policy.
