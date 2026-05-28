# Contract: `perchstation` operator CLI (capture additions)

**Direction**: operator → station (local, via SSH or console).

This document **extends** the CLI contract defined in
`specs/001-clip-delivery/contracts/cli.md`. The capture subsystem
introduces **no new subcommand**, **no new global flag**, and **no new
exit code**. It extends two surfaces only:

1. `perchstation status` gains capture-side fields on the existing
   text and JSON outputs (FR-015, US3 #1, US3 #2, SC-007).
2. `perchstation serve` spawns one additional supervised task (the
   capture loop) in addition to the existing `DeliveryRunner` and
   `ClassifyPoller`.

Every behaviour from `001/contracts/cli.md` not mentioned here is
unchanged.

---

## `perchstation status` (capture additions)

The capture-side fields are joined into the existing `StatusSnapshot`
defined by 001's `crates/perchstation-core/src/observability/status.rs`.

### Text output (additions)

The default human-readable rendering gains a `Capture:` section after
the existing `Last 3 deliveries:` block. Example:

```text
Enrollment:    OK (station 7f3e..., cert expires 2027-04-12)
Queue depth:   3 clips (12.4 MB on disk)
Last success:  2026-05-27 06:31:08 UTC  20260527T063108Z-001  classify=Success
Last failure:  2026-05-26 22:14:55 UTC  perchpub 503
Last 3 deliveries:
  2026-05-27 06:31  20260527T063108Z-001  classify=Success
  2026-05-27 06:28  20260527T062800Z-001  classify=Processing
  2026-05-27 06:25  20260527T062500Z-001  classify=Queued
Capture:
  Last recording:  2026-05-27 06:31:00 UTC  20260527T063100Z-001
  Last failure:    (none)
  Sensor:          healthy
```

Variants:

- When the device has never recorded: `Last recording:  (none)`.
- When the most recent capture attempt failed:
  `Last failure:    2026-05-27 06:30:12 UTC  recording_failed: io error reading from camera`.
- When the sensor is degraded:
  `Sensor:          stuck_asserted (since 2026-05-27 06:25:00 UTC)`
  or
  `Sensor:          unavailable (since 2026-05-27 06:25:00 UTC)`.

The capture section MUST be emitted even when every field is `None`
(so the operator can confirm the capture half is up). When the capture
task has not run in this process (e.g. `status` is invoked
outside of `serve`), all capture fields are `None`, which renders as
the three lines above with `(none)` / `(unknown)` defaults.

### JSON output (additions)

The JSON schema gains a top-level `capture` object:

```json
{
  "enrollment": { … },
  "queue":      { … },
  "last_success": { … },
  "last_failure": { … },
  "recent":     [ … ],
  "capture": {
    "last_recording_at": "2026-05-27T06:31:00Z",
    "last_clip_id":      "20260527T063100Z-001",
    "last_failure": {
      "at":      "2026-05-27T06:30:12Z",
      "kind":    "recording_failed",
      "message": "io error reading from camera"
    },
    "sensor_liveness":       "healthy",
    "sensor_degraded_since": null
  }
}
```

Field reference:

| Field                              | Type                                              | Notes                                                                |
| ---------------------------------- | ------------------------------------------------- | -------------------------------------------------------------------- |
| `capture.last_recording_at`        | RFC 3339 UTC timestamp ∣ null                     | Time of the most recent **successful** clip submission. Mirrors the wall-clock `triggered_at` of the recording (i.e., the `captured_at` field of the resulting `ClipQueueEntry`). |
| `capture.last_clip_id`             | string ∣ null                                     | Queue-side `clip_id` of the most recent successful recording.        |
| `capture.last_failure.at`          | RFC 3339 UTC timestamp                            | Present iff `last_failure != null`.                                  |
| `capture.last_failure.kind`        | string                                            | One of `"recording_failed"`, `"camera_hang"`, `"queue_full"`, `"queue_io"`, `"disk_pressure"`. |
| `capture.last_failure.message`     | string                                            |                                                                       |
| `capture.sensor_liveness`          | enum `"healthy" \| "stuck_asserted" \| "unavailable"` | Mirrors `SensorLivenessTracker`'s current state.                 |
| `capture.sensor_degraded_since`    | RFC 3339 UTC timestamp ∣ null                     | Present when `sensor_liveness != "healthy"`.                          |

### Behaviour

- `status` remains read-only with respect to `data_dir`; the capture
  fields are read from the in-process `Arc<CaptureStateSnapshot>` when
  `status` is invoked from the same process as `serve` (e.g. in
  integration tests), and default to `None` / `"healthy"` /
  `"never recorded"` otherwise.
- The existing exit-code contract is unchanged; capture state cannot
  produce an exit code other than 0 from `status` (the loop's
  degradation is reflected in the snapshot, not in the exit code).

---

## `perchstation serve` (capture additions)

Behavioural changes to the `serve` subcommand:

1. After boot reconciliation (existing 001 step) but before
   `service.ready`, `serve` calls `capture::staging::purge(<data_dir>/capture-staging/)`.
   This satisfies FR-017 (no stale staging across reboots).
2. After staging purge but before `service.ready`, `serve` constructs
   the production `MotionSensor` and `Camera` adapters from the
   `[capture]` config section, boxes them, and wires them into a
   `Capture` instance alongside the existing `DeliveryRunner` /
   `ClassifyPoller`.
3. `serve` spawns the `Capture::run` future as an additional supervised
   `tokio::spawn` task alongside the delivery and classify-poller
   tasks.
4. On SIGTERM / SIGINT, the existing `service.shutdown` flow aborts
   *all three* worker tasks before returning.

Failure isolation (FR-012):

- A panic in the capture task does NOT terminate the delivery loop and
  vice versa. Today this means the panic is caught at the
  `tokio::spawn` join boundary and logged; the panicked task is not
  automatically restarted — systemd's `Restart=always` is the recovery
  mechanism, as for delivery.
- A panic, error return, or graceful exit from `Capture::run` is
  reflected by the absence of further `capture.*` events in journald;
  the delivery half continues to emit `delivery.*` events normally.

Exit codes:

- `70` (Configuration error) is now also returned when
  `config.capture.*` fails to parse — the existing
  `service.config_invalid` event is emitted with `path` and `message`
  in the same shape.
- The capture subsystem cannot introduce a new top-level exit code; a
  capture-side fatal error (e.g. `GpioMotionSensor::new` fails because
  `/dev/gpiochip0` doesn't exist) maps onto `70` (configuration /
  hardware) with a clear message.

---

## Test obligations

- `tests/integration/capture_status_surface.rs` invokes `perchstation
  status` (via `assert_cmd`) against a `data_dir` whose in-process
  `CaptureStateSnapshot` was primed by the test, and asserts each of
  the four states ("never recorded", "recorded recently", "recent
  failure", "sensor degraded") renders correctly in both text and
  `--json` modes.
- `tests/integration/capture_resilience.rs` invokes `perchstation
  serve` with a fake sensor + fake camera, kills the process mid-
  recording, restarts it, and asserts that
  `<data_dir>/capture-staging/` is empty post-restart and that the
  queue contains no partial clip — the integration-level check that
  combines FR-017, FR-009, and SC-003.

---

## Versioning

These additions are a **minor** change to the CLI contract surface:
the existing fields and exit codes are preserved bit-for-bit; only new
fields are added under `status`'s JSON schema, and only an additional
section is added to the text rendering. External consumers parsing
the JSON output with a permissive `serde` `deny_unknown_fields = false`
client continue to work; the perchstation project's own consumers
update their fixtures.
