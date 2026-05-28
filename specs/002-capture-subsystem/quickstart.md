# Quickstart: Capture Subsystem (dev host)

**Audience**: a developer with a clean clone of `perchstation`, no Pi
attached, no camera, no GPIO. The constitution requires that "the bulk
of the codebase MUST be testable without a Pi"; this quickstart is how
you verify the capture half of that on day one.

End-to-end goal: with feature 001's enrollment + delivery already
working against a fake perchpub locally, run the `perchstation serve`
process with a fake motion sensor + fake camera, drive a single
synthetic motion edge from a test harness, and watch exactly one
clip land in `<data_dir>/queue/pending/` via the existing
`Inbox::submit` path — with no clip-delivery code touched.

## Prerequisites

- Everything from `specs/001-clip-delivery/quickstart.md` §Prerequisites.
- Familiarity with `tests/integration/` from feature 001 (the capture
  tests live alongside the delivery tests in the same directory).

No Pi, no camera, no GPIO line. Real perchpub not required.

## 1. Build and test

```sh
cargo fmt --check
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
```

`cargo test --workspace` now also runs the capture integration tests
under `tests/integration/capture_*.rs`. Each spins up an in-process
`Capture` task with a `FakeMotionSensor` and a `FakeCamera`, drives a
controlled sequence of motion edges, and asserts on the resulting
`<data_dir>/queue/pending/` state plus the structured-log stream.

Expected: green. If anything is red, **stop** and fix before moving on.

## 2. The validated reference: `capture_happy.rs`

The single test that proves the capture half end-to-end against the
delivery half is `tests/integration/capture_happy.rs`. Read it before
you write your own scratch driver — it is the canonical recipe:

```text
tests/integration/capture_happy.rs   # US1 acceptance #1 + #2
```

What it does (paraphrased):

1. Create a `TempDir` for `data_dir`. Open a `QueueStore`. Wrap it in
   `PolicyInbox<StoreInbox>` with a default `QueuePolicy`.
2. Create a `FakeMotionSensor` (mpsc-backed) and a `FakeCamera`
   (writes 1024 bytes of `0x42` into `<data_dir>/capture-staging/`).
3. Construct a `Capture` with the fakes, the inbox, a `Clock` fake,
   and a `CaptureConfig` with short `clip_duration_secs` (e.g. 1) for
   test speed.
4. Spawn `Capture::run`. Push one synthetic edge through
   `FakeMotionSensor::trigger(at)`.
5. Wait for the structured log event `capture.recording_completed`
   (via a `tracing_subscriber` capture layer installed by the test).
6. Assert exactly one `.mp4` + sidecar pair lives in
   `<data_dir>/queue/pending/`. Read the sidecar JSON, assert
   `captured_at == at`. Assert `<data_dir>/capture-staging/` is empty.
7. Stop the supervisor via the `CancellationToken`.

This test exercises the same `Inbox::submit` path delivery uses, so a
green `capture_happy.rs` is the integration-level proof that "from
delivery's perspective, the capture subsystem is just another `Inbox`
caller."

## 3. Exercising specific acceptance scenarios

The capture test matrix from `plan.md` §Testing maps spec acceptance
scenarios to test files. To exercise one in isolation:

```sh
cargo test --workspace --test capture_cooldown
cargo test --workspace --test capture_stuck_sensor
cargo test --workspace --test capture_queue_full
```

Each test follows the same pattern as `capture_happy.rs` but tunes
the fakes:

| Test                                  | Fake configuration                                                                                          |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `capture_idle.rs`                     | `FakeMotionSensor` never fires. Assert zero clips, zero camera invocations, and the clip count from the structured log is exactly zero. |
| `capture_cooldown.rs`                 | Fire two edges within `cooldown_secs`. Assert one clip in `pending/`, one `capture.cooldown_skip` event.   |
| `capture_stuck_sensor.rs`             | `FakeMotionSensor::set_level(Asserted)` continuously past `liveness_stuck_secs`. Assert `capture.sensor_degraded { kind: "stuck_asserted" }`, then a follow-up edge produces a `capture.degraded_skip` event and no clip. Drive `FakeMotionSensor::set_level(Quiescent)`, assert `capture.sensor_recovered`. |
| `capture_unavailable_sensor.rs`       | `FakeMotionSensor::set_error(Unavailable)` from both `next_trigger` and `level`. Assert recovery once the error is cleared. |
| `capture_recording_failure.rs`        | `FakeCamera::Mode::FailMidway`. Assert no clip in `pending/`, staging file removed, `capture.recording_failed` event. |
| `capture_bounded_clip.rs`             | `FakeCamera::Mode::Hang`. Assert outer timeout fires, `capture.recording_hung` event, staging removed.     |
| `capture_disk_pressure.rs`            | Pre-populate `<data_dir>/capture-staging/` with garbage to exceed `max_staging_bytes`; assert `capture.disk_pressure_skip`. |
| `capture_queue_full.rs`               | `PolicyInbox` configured with `EvictionPolicy::RefuseNew` and `max_clips=1`. Pre-load one clip, fire trigger, assert `capture.queue_refused { kind: "queue_full" }` and staging removed. |
| `capture_resilience.rs`               | Spawn `Capture`, fire trigger, drop the task during recording (simulates power loss), restart `Capture` against the same `data_dir`. Assert staging-purge cleans up partial files, queue is intact, supervisor accepts a new trigger. |

## 4. Driving the binary manually

For ad-hoc debugging it is often useful to run `perchstation serve`
manually rather than via a test. To do this without real hardware,
the binary respects two dev-only feature flags off by default:

```sh
# Dev wiring: --capture-source fake [--capture-fixture <path>]
# (Tracked as a follow-up; see contracts/hw-traits.md "Constructor surfaces".)
```

The follow-up will allow:

```sh
cargo run -p perchstation -- \
    --config "$PERCHSTATION_DATA/config.toml" \
    --log-format text \
    serve --capture-source fake --capture-fixture "$FIXTURES/clip-stub.mp4"
```

Until that flag exists, the validated path for manual exploration is
to add a new integration test alongside `capture_happy.rs` and run it
with `RUST_LOG=perchstation_core::capture=trace` to see the supervisor's
internals on stderr.

## 5. Inspecting capture status

Once `perchstation serve` is running with the capture loop wired up,
`perchstation status` shows the new capture section:

```sh
cargo run -p perchstation -- --config "$PERCHSTATION_DATA/config.toml" status
```

Expected output appends the `Capture:` block documented in
`contracts/cli.md`:

```text
Capture:
  Last recording:  2026-05-28 14:23:12 UTC  20260528T142312Z-001
  Last failure:    (none)
  Sensor:          healthy
```

JSON form:

```sh
cargo run -p perchstation -- --config "$PERCHSTATION_DATA/config.toml" status --json | jq .capture
```

```json
{
  "last_recording_at": "2026-05-28T14:23:12Z",
  "last_clip_id":      "20260528T142312Z-001",
  "last_failure":      null,
  "sensor_liveness":   "healthy",
  "sensor_degraded_since": null
}
```

## 6. Verifying no network traffic

The capture subsystem MUST NOT produce any outbound traffic of its
own (spec US3 #3 / FR-014). This is a tested invariant via the
existing `outbound_allowlist.rs` test, extended with a capture loop
running its full select-loop for the test's window:

```sh
cargo test --workspace --test capture_no_network
```

A green run is the integration-level proof of US3 #3.

## 7. What this quickstart does *not* prove

- Real GPIO motion-sensor edges. The `gpio-cdev`-backed adapter is
  exercised only on a real Pi via the addition to
  `deploy/RELEASE-CHECKLIST.md`.
- Real `libcamera-vid` invocation. Same: release-only.
- Sun-heated sensor housings, weather, long-run wiring drift, and
  other physical failure modes. None of these are observable on the
  dev host; the spec's "robust under adverse conditions" promise (US2)
  is upheld by the documented release smoke test and the on-device
  7-day soak from feature 001's SC-005.

## 8. Adding a new capture test

The path is the same as adding a delivery integration test, plus one
extra step:

1. Create `tests/integration/capture_<scenario>.rs`.
2. Build a `Capture` with `FakeMotionSensor` + `FakeCamera` from
   `tests/integration/support/`.
3. Drive the scenario via the fakes' inputs.
4. Assert on:
   - The on-disk state of `<data_dir>/queue/pending|inflight|delivered/`.
   - The on-disk state of `<data_dir>/capture-staging/`.
   - The captured structured-log events (via the same
     `tracing_subscriber::test::capture` machinery the delivery tests
     use).
5. **Map the test back to a spec requirement** in the file-level
   docstring (`//! Spec mapping: US2 acceptance #3 / FR-010 / SC-004.`).
   This is the discipline that keeps the spec ↔ test ↔ code chain
   readable.
