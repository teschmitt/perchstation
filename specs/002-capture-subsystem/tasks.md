# Tasks: Capture Subsystem

**Input**: Design documents from `/specs/002-capture-subsystem/`

**Prerequisites**: plan.md (required), spec.md (required for user stories),
research.md, data-model.md, contracts/

**Tests**: Included. The constitution's Principle V (Test-First, non-negotiable)
and the spec's Success Criteria mandate that every functional requirement maps
to at least one host-runnable integration test. Test tasks are listed first
within each user-story phase; per TDD, write them and watch them fail before
the implementation tasks land.

**Organization**: Tasks are grouped by user story to enable independent
implementation and testing of each story.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on
  incomplete tasks).
- **[Story]**: Which user story this task belongs to (US1, US2, US3).
- Each task includes the exact file path.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Project initialization — dependency addition and module
skeleton creation. After this phase the workspace compiles cleanly with
empty stubs in place.

- [ ] T001 [P] Add `gpio-cdev = "0.6"` to `[target.'cfg(target_os = "linux")'.dependencies]` in `crates/perchstation-hw/Cargo.toml` (see research.md R-2).
- [ ] T002 Create capture module skeleton — new directory `crates/perchstation-core/src/capture/` with `mod.rs` declaring `pub mod cooldown; pub mod liveness; pub mod recording; pub mod runner; pub mod staging; pub mod state;` plus empty placeholder files `cooldown.rs`, `liveness.rs`, `recording.rs`, `runner.rs`, `staging.rs`, `state.rs`; and add `pub mod capture;` to `crates/perchstation-core/src/lib.rs` so the workspace continues to compile (`cargo build -p perchstation-core` must pass after this task).

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Shared types, trait surfaces, log-event constants, status
projection scaffolding, and test fakes that every user story consumes.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

- [ ] T003 [P] Add `CaptureConfig` struct (fields from data-model.md §Configuration; all `#[serde(default)]`; defaults from research.md R-4 / R-7 / R-10) and `pub capture: CaptureConfig` with `#[serde(default)]` to the top-level `Config` struct in `crates/perchstation-core/src/config.rs`.
- [ ] T004 [P] Extend `crates/perchstation-core/src/hw_traits.rs` with `MotionSensor` (async trait, `next_trigger`, `level`), `Camera` (async trait, `record_clip`), `SensorLevel`, `RecordedClip`, `MotionSensorError`, and `CameraError` exactly as specified in `contracts/hw-traits.md`.
- [ ] T005 [P] Add capture-side event-code constants (`CAPTURE_READY`, `CAPTURE_SHUTDOWN`, `CAPTURE_STAGING_PURGED`, `CAPTURE_TRIGGER_OBSERVED`, `CAPTURE_RECORDING_STARTED`, `CAPTURE_RECORDING_COMPLETED`, `CAPTURE_RECORDING_FAILED`, `CAPTURE_RECORDING_HUNG`, `CAPTURE_COOLDOWN_SKIP`, `CAPTURE_DEGRADED_SKIP`, `CAPTURE_DISK_PRESSURE_SKIP`, `CAPTURE_QUEUE_REFUSED`, `CAPTURE_SENSOR_DEGRADED`, `CAPTURE_SENSOR_RECOVERED`) under the existing `events` module in `crates/perchstation-core/src/observability/tracing.rs` (string values exactly as in `contracts/log-events.md`).
- [ ] T006 [P] Add the `CaptureSnapshot`, `CaptureFailureSnapshot`, and `CaptureLivenessSnapshot` types (fields exactly as in `contracts/cli.md` §JSON output and data-model.md §CaptureStateSnapshot) to `crates/perchstation-core/src/observability/status.rs`. `CaptureLivenessSnapshot` has four variants: `NeverObserved`, `Healthy`, `StuckAsserted`, `Unavailable` (serde-renamed to lower snake_case for the JSON form). Do NOT yet wire `CaptureSnapshot` into `StatusSnapshot` — that join is a US3 task.
- [ ] T007 [P] Implement `FakeMotionSensor` (mpsc-backed `next_trigger`, `Arc<Mutex>`-backed `level`, `set_level`, `set_error`, `trigger(at)` helpers) in `tests/integration/support/fake_motion_sensor.rs` to match the obligations in `contracts/hw-traits.md` §Implementations.
- [ ] T008 [P] Implement `FakeCamera` (modes `Ok`, `FailMidway`, `Hang`, `EmptyOutput`; writes 1024 bytes of `0x42` by default to the staging path; cleans up the staging file on `FailMidway`/`Hang`/drop-cancellation) in `tests/integration/support/fake_camera.rs` to match `contracts/hw-traits.md` §Implementations.
- [ ] T009 Register the new fakes by adding `pub mod fake_motion_sensor;` and `pub mod fake_camera;` to `tests/integration/support/mod.rs` (depends on T007 + T008).

**Checkpoint**: Foundation ready — `cargo build --workspace` and `cargo build --tests --workspace` succeed; user-story implementation can begin.

---

## Phase 3: User Story 1 - Motion Event Captured and Handed Off (Priority: P1) 🎯 MVP

**Goal**: A single motion-sensor edge produces exactly one bounded clip
file that arrives in `<data_dir>/queue/pending/` via `Inbox::submit`,
with `captured_at` reflecting the trigger time and the camera powered
down before and after. This is the minimum viable capture loop.

**Independent Test**: With `perchstation serve` running against an empty
queue and a `FakeMotionSensor`/`FakeCamera`, push one synthetic edge and
verify exactly one `.mp4` + sidecar pair appears in `pending/` with
correct `captured_at`, and `<data_dir>/capture-staging/` is empty
afterwards. See `quickstart.md` §2 for the canonical recipe.

### Tests for User Story 1 ⚠️

> Write these tests first and watch them fail before implementing.

- [ ] T010 [P] [US1] Add integration test asserting end-to-end happy path (single trigger → one clip in `pending/` with correct `captured_at`, staging empty, camera powered down) in `tests/integration/capture_happy.rs`. Spec mapping: US1 #1, #2 / SC-001.
- [ ] T011 [P] [US1] Add integration test asserting recording terminates at the configured `clip_duration_secs` even when the sensor stays asserted, and the resulting clip lands in `pending/` in `tests/integration/capture_bounded_clip.rs`. Spec mapping: US1 #3 / FR-005. Use `FakeCamera::Mode::Ok` together with `FakeMotionSensor::set_level(Asserted)`; the test asserts the **inner** clip-duration bound (the camera returns cleanly when `max_duration` elapses). Do NOT use `Mode::Hang` here — the camera-hang scenario covering the outer `tokio::time::timeout` is a separate test (T029a / `tests/integration/capture_camera_hang.rs`).
- [ ] T012 [P] [US1] Add integration test asserting that with `FakeMotionSensor` never firing, the capture loop performs zero camera invocations, leaves staging empty, produces zero `capture.recording_*` events, and `<data_dir>/queue/pending/` is empty after a multi-second idle window in `tests/integration/capture_idle.rs`. Spec mapping: US1 #4 / SC-008.

### Implementation for User Story 1

- [ ] T013 [P] [US1] Implement `StagingDir` newtype + `purge(staging_dir)` function (recursive `fs::remove_file` of every file under the directory; `fs::create_dir_all` first; emits `capture.staging_purged` event via the `tracing` constants from T005; returns `(removed_files, removed_bytes)`) in `crates/perchstation-core/src/capture/staging.rs`. Spec mapping: FR-017 / SC-003.
- [ ] T014 [P] [US1] Implement `CooldownState { until, last_outcome }` with `new`, `start_after(now, cooldown_secs)`, `is_active(now)`, and `last_outcome()` matching data-model.md §CooldownState in `crates/perchstation-core/src/capture/cooldown.rs`. Spec mapping: FR-006.
- [ ] T015 [P] [US1] Implement `CaptureState` (the `Arc<RwLock<CaptureStateInner>>` projection) with `new()`, `snapshot(&self) -> CaptureSnapshot`, `record_success(clip_id, at)`, `record_failure(at, kind, message)`, `set_liveness(state, since)` methods using the snapshot types from T006 in `crates/perchstation-core/src/capture/state.rs`. The struct holds last_recording_at, last_clip_id, last_failure, sensor_liveness, sensor_degraded_since (data-model.md §CaptureStateSnapshot). `CaptureState::new()` MUST default `sensor_liveness` to `NeverObserved` so a `status` invocation made before the supervisor has run its first liveness probe does not claim the sensor is healthy when nothing has been checked.
- [ ] T016 [US1] Implement the bounded record-and-stage helper `record_into_staging(camera, staging, max_duration, hang_margin) -> Result<RecordedClip, CaptureRecordError>` that wraps `camera.record_clip(max_duration)` in `tokio::time::timeout(max_duration + hang_margin)`, removes the staging file on any error path (including timeout drop-cancellation), and validates `byte_size > 0` before returning `Ok` in `crates/perchstation-core/src/capture/recording.rs` (depends on T013 for staging types). Spec mapping: FR-005, FR-008, Edge Case "Camera adapter hangs".
- [ ] T017 [US1] Implement the `Capture` supervisor in `crates/perchstation-core/src/capture/runner.rs`: the `tokio::select!` loop over `MotionSensor::next_trigger` + `tokio::time::interval(liveness_poll_secs)` + `CancellationToken::cancelled`; on a fresh trigger it gates on `CooldownState::is_active`, calls the recording helper from T016, then `inbox.submit(staging_path, ClipMeta { captured_at: triggered_at })`, then `CooldownState::start_after`, then `CaptureState::record_success`. Emits `capture.trigger_observed`, `capture.recording_started`, `capture.recording_completed`, `capture.cooldown_skip`, `capture.ready`, `capture.shutdown` via T005's constants. (Depends on T013, T014, T015, T016.) Spec mapping: US1 #1, #2, #3, FR-001, FR-002, FR-003, FR-004, FR-007.
- [ ] T018 [US1] Wire the public surface in `crates/perchstation-core/src/capture/mod.rs`: re-export `Capture`, `CaptureConfig` (from `crate::config::CaptureConfig`), `CaptureState`, and a `Capture::run(self, shutdown: CancellationToken)` async entry point that performs the startup `staging::purge` *then* emits `capture.ready` *then* enters the runner loop. (Depends on T013, T014, T015, T016, T017.) Spec mapping: FR-017.
- [ ] T019 [P] [US1] Implement `GpioMotionSensor` (constructor: `new(chip_path, line, active_high)`; subscribes to gpio-cdev rising-edge events on the configured line for `next_trigger`; reads the level via a separate line handle for `level`; gates the file on `#[cfg(target_os = "linux")]`) in `crates/perchstation-hw/src/motion_sensor.rs`. Spec mapping: FR-016, research.md R-2.
- [ ] T020 [P] [US1] Implement `LibcameraVidCamera` (constructor: `new(staging_dir, width, height, framerate, bitrate_bps)`; `record_clip` spawns `libcamera-vid --timeout <ms> --codec h264 --inline --width W --height H --framerate F --nopreview -o <staging>/<recording-id>.mp4` via `tokio::process::Command`; drop sends SIGTERM then SIGKILL after a short grace and removes the staging file; gates on `#[cfg(target_os = "linux")]`) in `crates/perchstation-hw/src/camera_recorder.rs`. Spec mapping: FR-016, research.md R-3.
- [ ] T021 [US1] Add `#[cfg(target_os = "linux")] pub mod motion_sensor;` and `#[cfg(target_os = "linux")] pub mod camera_recorder;` to `crates/perchstation-hw/src/lib.rs` (depends on T019, T020).
- [ ] T022 [US1] In `crates/perchstation/src/commands/serve.rs`, after the existing boot reconciliation but before `service.ready`: construct `LibcameraVidCamera` and `GpioMotionSensor` from `config.capture.*`, box them as `Box<dyn Camera>` / `Box<dyn MotionSensor>`, build a `Capture` instance sharing the existing `Arc<dyn Inbox>` + `Arc<dyn Clock>` + the shared `CancellationToken`, and `tokio::spawn(capture.run(shutdown))` alongside the existing `DeliveryRunner` and `ClassifyPoller` tasks. On shutdown, await the capture task's join handle. (Depends on T018, T021.) Spec mapping: FR-012, contracts/cli.md §`perchstation serve`.

**Checkpoint**: `cargo test --test capture_happy --test capture_bounded_clip --test capture_idle --workspace` passes. User Story 1 is fully functional and testable end-to-end with fakes.

---

## Phase 4: User Story 2 - Robust Capture Under Adverse Conditions (Priority: P2)

**Goal**: The capture loop survives stuck sensors, unavailable sensors,
camera hangs, recording failures, queue-full responses, disk-pressure
conditions, and power loss mid-recording. After each induced failure
the loop returns to normal operation without operator intervention.

**Independent Test**: With US1 working, drive each failure mode in turn
via the fake sensor/camera modes and assert (a) no partial clips ever
enter the queue, (b) staging is cleaned up, (c) the capture loop keeps
running, (d) the delivery loop is unaffected.

### Tests for User Story 2 ⚠️

> Write these tests first and watch them fail before implementing.

- [ ] T023 [P] [US2] Add integration test in `tests/integration/capture_cooldown.rs` with two scenarios: (1) two trigger edges within `cooldown_secs`, asserting exactly one clip in `pending/` and one `capture.cooldown_skip` event; (2) one trigger edge followed by the fake sensor remaining `Asserted` past `cooldown_secs`, asserting no second recording fires until the fake sensor transitions Quiescent → Asserted and pushes a fresh edge (US2 #2 second clause: "further recordings only occur after the sensor first returns to its quiescent state and then re-asserts"). Spec mapping: US2 #2, FR-004 (fresh quiescent-to-asserted transition), FR-006.
- [ ] T024 [P] [US2] Add integration test driving `FakeMotionSensor::set_level(Asserted)` continuously past `liveness_stuck_secs`, asserting `capture.sensor_degraded { kind: "stuck_asserted" }` is emitted, a subsequent trigger produces `capture.degraded_skip` and no clip, and `set_level(Quiescent)` produces `capture.sensor_recovered`, in `tests/integration/capture_stuck_sensor.rs`. Spec mapping: US2 #3, FR-010, SC-004.
- [ ] T025 [P] [US2] Add integration test driving `FakeMotionSensor::set_error(Unavailable)` on `level` and `next_trigger`, asserting `capture.sensor_degraded { kind: "unavailable" }` is emitted, the loop continues running, and recovery once the error is cleared produces `capture.sensor_recovered`, in `tests/integration/capture_unavailable_sensor.rs`. Spec mapping: US2 #4, FR-011, SC-005.
- [ ] T026 [P] [US2] Add integration test using `FakeCamera::Mode::FailMidway`, asserting no clip in `pending/`, staging removed, `capture.recording_failed` event emitted, and the loop accepts a subsequent successful trigger, in `tests/integration/capture_recording_failure.rs`. Spec mapping: US2 #8, FR-008.
- [ ] T027 [P] [US2] Add integration test pre-populating `<data_dir>/capture-staging/` with garbage to exceed `max_staging_bytes`, firing a trigger, and asserting `capture.disk_pressure_skip` is emitted, no recording is attempted, and the loop enters cooldown so it does not tight-loop, in `tests/integration/capture_disk_pressure.rs`. Spec mapping: US2 #6, FR-013.
- [ ] T028 [P] [US2] Add integration test using `PolicyInbox<StoreInbox>` configured with `EvictionPolicy::RefuseNew` + `max_clips=1`, pre-loading one clip, firing a trigger, and asserting `capture.queue_refused { kind: "queue_full" }` is emitted, the staging file is removed, the loop enters cooldown, and the loop continues running, in `tests/integration/capture_queue_full.rs`. Spec mapping: US2 #7, FR-018.
- [ ] T029 [P] [US2] Add integration test in `tests/integration/capture_resilience.rs` covering crash-restart **and** the boot/shutdown sensor-edge edge case. Scenarios: (1) spawns `Capture`, (2) fires a trigger and waits until recording is in progress, (3) drops the task to simulate power loss leaving a partial staging file, (4) opens a fresh `Capture` against the same `data_dir`, (5) asserts the boot-time staging purge removed the partial file, the queue is intact, and a new trigger after restart records normally. Add two further assertions exercising the spec's "Sensor fires during boot or shutdown" edge case: (a) an edge fired before `Capture::run` resolves the staging purge is observed on the first iteration after readiness and still produces a clip in `pending/` (no corruption); (b) an edge that arrives after the `CancellationToken` has been signalled is dropped cleanly — no new staging file, no partial queue entry. Spec mapping: US2 #1, FR-009, FR-017, SC-003, Edge Case "Sensor fires during boot or shutdown".
- [ ] T029a [P] [US2] Add integration test in `tests/integration/capture_camera_hang.rs` that drives `FakeCamera::Mode::Hang`, fires a trigger, and asserts: (1) the supervisor's outer `tokio::time::timeout` fires within ~`hang_margin_secs` of `clip_duration_secs` elapsing; (2) a single `capture.recording_hung { recording_id, max_duration_ms }` event is emitted; (3) the staging file is removed by the drop-cancellation path (`FakeCamera::Mode::Hang`'s cleanup branch); (4) no clip ever appears in `<data_dir>/queue/pending/`; (5) `CaptureState::record_failure` is updated with `kind = "camera_hang"` (matching the `last_failure.kind` enum in `contracts/cli.md`); (6) after the hang fault clears, the loop accepts a subsequent normal trigger (with `Mode::Ok`) and records cleanly. Spec mapping: Edge Case "Camera adapter hangs", FR-005 (outer bound), `capture.recording_hung`.
- [ ] T029b [P] [US2] Add integration test in `tests/integration/capture_concurrent_event.rs` that fires two synthetic motion edges within a single recording's duration and asserts exactly one `Camera::record_clip` invocation occurred and exactly one clip lands in `pending/`. Simplest implementation: configure `CaptureConfig.clip_duration_secs` to a small value (e.g. 1 s) so a `Mode::Ok` recording is still in progress when trigger #2 arrives. If finer control is desired, add a `Mode::Slow(Duration)` variant to `FakeCamera` (and in that case also extend T008 and `contracts/hw-traits.md` §Implementations to list the new mode). Spec mapping: US2 #5 ("a motion event fires while a previous recording is still being staged — no second concurrent recording"), R-5 (supervisor's `select!` does not advance until `handle_trigger` returns).
- [ ] T029c [P] [US2] Add integration test in `tests/integration/capture_isolation.rs` that spawns the delivery loop and the capture loop against a shared `data_dir`, pre-loads a few pending clips into the queue, and verifies failure-isolation in both directions: (1) inject a panic into the capture task (via a new `FakeMotionSensor::panic_on_next_trigger()` helper, or extend the existing fake with a `Mode::Panic` variant — note this in the task description and update T007 if so); assert delivery continues to drain the pre-loaded clips after the capture panic. (2) symmetrically, inject a panic into delivery (re-using existing 001 test seams) and assert capture continues to record from fresh triggers. Spec mapping: SC-009, FR-012, `contracts/cli.md` §Failure isolation.

### Implementation for User Story 2

- [ ] T030 [P] [US2] Implement `SensorLivenessTracker` with `new(now, stuck_secs)`, `observe_level(now, Result<SensorLevel, Err>)`, `observe_trigger_error(now, Err)`, `state() -> SensorLiveness`, and `is_degraded() -> bool`. Transitions exactly per data-model.md §SensorLivenessTracker / research.md R-8; each transition returns an enum describing the event the supervisor should emit (`Degraded { kind, since, reason? }`, `Recovered { kind }`, `NoChange`). In `crates/perchstation-core/src/capture/liveness.rs`. Spec mapping: FR-010, FR-011.
- [ ] T031 [P] [US2] Add `staging_bytes(staging_dir) -> io::Result<u64>` helper that sums file sizes under the staging directory (used for the pre-record disk-pressure gate) to `crates/perchstation-core/src/capture/staging.rs`. Spec mapping: FR-013.
- [ ] T032 [US2] Extend `Capture` supervisor in `crates/perchstation-core/src/capture/runner.rs` with: (a) the `SensorLivenessTracker` field driven by the existing liveness tick, emitting `capture.sensor_degraded`/`capture.sensor_recovered` and updating `CaptureState::set_liveness`; (b) the liveness gate (refuse to record when `is_degraded()`, emit `capture.degraded_skip`, start cooldown); (c) the disk-pressure gate (call `staging_bytes`, if it would exceed `max_staging_bytes` emit `capture.disk_pressure_skip`, update `CaptureState::record_failure(kind="disk_pressure", message=<bytes vs ceiling>)`, start cooldown); (d) queue-refusal handling — on `InboxError::QueueFull` / `InboxError::Queue(_)` remove the staging file, emit `capture.queue_refused { kind }`, update `CaptureState::record_failure(kind="queue_full"|"queue_io", …)`, start cooldown; (e) recording-error paths — on `CaptureRecordError::Failed(_)` emit `capture.recording_failed { kind }` and update `CaptureState::record_failure(kind="recording_failed", message=<adapter error>)`; on `CaptureRecordError::Timeout` (the outer `tokio::time::timeout` fired) emit `capture.recording_hung { recording_id, max_duration_ms }` and update `CaptureState::record_failure(kind="camera_hang", message=<max_duration_ms>)`, then start cooldown so the loop does not tight-loop on a sustained hung adapter. (Depends on T030, T031.) Spec mapping: FR-010, FR-011, FR-013, FR-018, US2 #3, #4, #6, #7, #8, Edge Case "Camera adapter hangs".
- [ ] T033 [US2] In `crates/perchstation/src/commands/serve.rs`, wrap the capture task's `JoinHandle` so a panic or error return is caught at the join boundary, logged via `service.task_panicked { task: "capture" }`, and does NOT abort the delivery or classify-poller tasks (and vice versa for delivery). Spec mapping: FR-012, SC-009, contracts/cli.md §Failure isolation.

**Checkpoint**: `cargo test --test capture_cooldown --test capture_stuck_sensor --test capture_unavailable_sensor --test capture_recording_failure --test capture_disk_pressure --test capture_queue_full --test capture_resilience --test capture_camera_hang --test capture_concurrent_event --test capture_isolation --workspace` passes. User Story 2 is fully functional alongside US1.

---

## Phase 5: User Story 3 - Capture-Side Visibility Through Existing Surfaces (Priority: P3)

**Goal**: An operator with shell access can answer "did the station
record anything recently?", "did capture fail?", and "is the sensor
healthy?" from `perchstation status` and the JSON log stream alone,
within 30 seconds, without any new surface or external service. The
capture subsystem contributes zero outbound traffic.

**Independent Test**: With US1 + US2 working, prime the in-process
`CaptureState` to one of four scenarios (never recorded, recorded
recently, recent failure, sensor degraded) and assert each renders
correctly in both text and `--json` modes of `perchstation status`.
Separately, run the capture loop in the outbound-allowlist test
fixture and assert zero outbound traffic from any capture-side code.

### Tests for User Story 3 ⚠️

> Write these tests first and watch them fail before implementing.

- [ ] T034 [P] [US3] Add integration test priming the in-process `CaptureState` to each of these scenarios (never recorded with `sensor_liveness = NeverObserved` — the "status invoked outside of `serve`" case; never recorded with `sensor_liveness = Healthy` — the "supervisor just started, no triggers yet" case; recently recorded; recent failure with kind+message; sensor degraded with `stuck_asserted`; sensor degraded with `unavailable`) and asserting both text rendering (per `contracts/cli.md` §Text output, including the `(never observed)` rendering of `NeverObserved`) and JSON rendering (per `contracts/cli.md` §JSON output, including `"sensor_liveness": "never_observed"`) of `perchstation status`, in `tests/integration/capture_status_surface.rs`. Spec mapping: US3 #1, #2, SC-007, FR-015.
- [ ] T035 [P] [US3] Add integration test that runs a `Capture` task with `FakeMotionSensor` firing periodically alongside the existing `outbound_allowlist`-style sniffer, asserting zero outbound traffic from any capture-side code path during the assertion window, in `tests/integration/capture_no_network.rs`. Spec mapping: US3 #3, FR-014.

### Implementation for User Story 3

- [ ] T036 [US3] In `crates/perchstation-core/src/observability/status.rs`, add a `pub capture: CaptureSnapshot` field to `StatusSnapshot` (using the types added in T006), make `StatusSnapshot::snapshot(...)` accept the optional `Arc<CaptureState>` from `serve` and clone its `snapshot()` into the new field (when the capture task is not running, fall back to a default `CaptureSnapshot` with every timestamp and failure field `None` and `sensor_liveness = NeverObserved`, so `status` remains safe to invoke standalone and never claims a sensor is healthy when nothing has been checked), and pass that handle through from the binary's `status` command wiring. Spec mapping: FR-015, contracts/cli.md §JSON output.
- [ ] T037 [US3] In `crates/perchstation/src/commands/status.rs`, render the new `Capture:` block in the text output (per `contracts/cli.md` §Text output — emit the block even when every field is `None`, using `(none)` defaults; format the `Sensor:` line according to the liveness enum: `(never observed)` for `NeverObserved`, `healthy` for `Healthy`, `stuck_asserted (since <ts>)` / `unavailable (since <ts>)` for the two degraded variants) and ensure the JSON output serializes the `capture` object with the exact field shape and casing in `contracts/cli.md` §JSON output (including the `"never_observed"` variant of `sensor_liveness`). (Depends on T036.) Spec mapping: FR-015, US3 #1, #2.

**Checkpoint**: `cargo test --test capture_status_surface --test capture_no_network --workspace` passes. All three user stories now work independently and together.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Operator-facing artefacts, on-device verification checklist,
and final workspace-wide gates.

- [ ] T038 [P] Add a commented `[capture]` section to `deploy/config.example.toml` documenting every field from `CaptureConfig` (defaults from research.md R-4 / R-7 / R-10) — `clip_duration_secs`, `hang_margin_secs`, `cooldown_secs`, `liveness_stuck_secs`, `liveness_poll_secs`, `max_staging_bytes`, `sensor_gpiochip`, `sensor_line`, `sensor_active_high`, `camera_width`, `camera_height`, `camera_framerate`, `camera_bitrate_bps`.
- [ ] T039 [P] Append capture-side smoke-test items to `deploy/RELEASE-CHECKLIST.md` covering: real GPIO edge from the wired motion sensor produces a clip; `libcamera-vid` produces a playable MP4; `perchstation status` reflects the new recording; sensor disconnected → status shows `unavailable` within ~60 s; sensor held asserted → status shows `stuck_asserted` after `liveness_stuck_secs`; staging directory size stays below `capture.max_staging_bytes` across a 30-minute trigger loop (spot-check of SC-006's structural property).
- [ ] T040 Run final workspace gates: `cargo fmt --check`, `cargo clippy --all-targets --workspace -- -D warnings`, and `cargo test --workspace`. All three MUST pass clean before PR.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies; can start immediately.
- **Foundational (Phase 2)**: depends on Setup; BLOCKS all user-story phases.
- **User Story 1 (Phase 3)**: depends on Foundational.
- **User Story 2 (Phase 4)**: depends on Foundational; in practice also
  needs the runner.rs / staging.rs files from US1 to exist, because US2
  extends them with the liveness gate, disk-pressure gate, and
  queue-refusal handling.
- **User Story 3 (Phase 5)**: depends on Foundational; in practice also
  needs US1's `CaptureState` to be wired into the runner (T017) so the
  status surface has data to project.
- **Polish (Phase 6)**: depends on all targeted user stories being complete.

### User Story Dependencies

- **US1**: depends on Foundational only — fully independent MVP.
- **US2**: extends US1's runner.rs / staging.rs / serve.rs. Not parallelizable
  with US1 in practice; runs after US1's checkpoint passes.
- **US3**: extends US1's runner.rs (via `CaptureState` writes) and the
  binary's status command. Runs after US1's checkpoint passes; can run
  in parallel with US2 since the surfaces touched (observability/status.rs,
  commands/status.rs) are disjoint from US2's touch set (runner.rs,
  liveness.rs, staging.rs, serve.rs).

### Within Each User Story

- Tests are written and made to fail before the matching implementation
  tasks land (Principle V).
- Within US1: the three "new file" tasks T013 (staging), T014
  (cooldown), T015 (state) can run in parallel; T016 (recording)
  depends on T013; T017 (runner) depends on T013/T014/T015/T016; T018
  (mod.rs public surface) depends on T013-T017; T019 (GpioMotionSensor)
  and T020 (LibcameraVidCamera) are independent of the supervisor and
  can run in parallel from the moment T004 is done; T021 (hw lib.rs)
  depends on T019+T020; T022 (serve.rs wiring) depends on T018+T021.
- Within US2: T030 (liveness.rs) and T031 (staging.rs helper) are
  independent and can run in parallel; T032 (runner.rs extension)
  depends on T030+T031; T033 (serve.rs panic isolation) is independent
  of T030/T031/T032 and can run in parallel with all of them.

### Parallel Opportunities

- All Setup tasks marked [P] run in parallel.
- All Foundational tasks marked [P] (T003-T008) run in parallel; T009
  is sequential after T007+T008.
- All US1 test files marked [P] (T010-T012) can be written in parallel.
- US1 file-creation tasks marked [P] (T013-T015 in core; T019-T020 in
  hw) can run in parallel.
- All US2 test files marked [P] (T023-T029, T029a-T029c) can be written in parallel.
- All US3 test files marked [P] (T034-T035) can be written in parallel.
- Polish tasks T038, T039 are independent.

---

## Parallel Example: User Story 1 implementation

```bash
# Phase 3 file-creation tasks that touch disjoint files can run together:
Task T013: Implement crates/perchstation-core/src/capture/staging.rs
Task T014: Implement crates/perchstation-core/src/capture/cooldown.rs
Task T015: Implement crates/perchstation-core/src/capture/state.rs
Task T019: Implement crates/perchstation-hw/src/motion_sensor.rs
Task T020: Implement crates/perchstation-hw/src/camera_recorder.rs

# Once T013 is done, T016 can begin (depends on staging types):
Task T016: Implement crates/perchstation-core/src/capture/recording.rs

# Once T013, T014, T015, T016 are done, T017 can begin:
Task T017: Implement crates/perchstation-core/src/capture/runner.rs

# Once T017, T019, T020 are done, T018 + T021 + T022 close out US1:
Task T018: Wire crates/perchstation-core/src/capture/mod.rs public surface
Task T021: Wire crates/perchstation-hw/src/lib.rs adapter exports
Task T022: Wire crates/perchstation/src/commands/serve.rs capture spawn
```

---

## Implementation Strategy

### MVP First (User Story 1 only)

1. Complete Phase 1 (Setup): T001, T002.
2. Complete Phase 2 (Foundational): T003-T009.
3. Complete Phase 3 (US1): T010-T022.
4. **STOP and VALIDATE**: run `cargo test --workspace`, confirm
   `capture_happy`, `capture_bounded_clip`, `capture_idle` are green.
5. Deploy/demo if the on-device path is desired (real GPIO + real
   `libcamera-vid` are smoke-tested per `deploy/RELEASE-CHECKLIST.md`).

### Incremental Delivery

1. Setup + Foundational → foundation ready.
2. Add US1 → test → deploy MVP.
3. Add US2 → test → deploy (capture now survives adverse conditions).
4. Add US3 → test → deploy (capture is now operator-inspectable via
   the existing status surface).
5. Polish (Phase 6) before opening the PR.

### Parallel Team Strategy

With multiple implementers (or subagents):

1. Team finishes Setup + Foundational together.
2. After US1's runner.rs is in place, US2 and US3 can be worked in
   parallel because their touch sets are disjoint (US2 → runner.rs,
   liveness.rs, staging.rs, serve.rs; US3 → observability/status.rs,
   commands/status.rs).
3. Each story carries its own integration tests; the workspace test
   suite gates the merge.

---

## Notes

- [P] tasks = different files, no dependencies on incomplete tasks.
- [Story] label maps each task to a user story for traceability.
- Every functional requirement maps to at least one host-runnable
  integration test (the matrix in plan.md §Testing).
- Hardware-only paths (real GPIO edge, real `libcamera-vid`) are
  verified by the addition to `deploy/RELEASE-CHECKLIST.md`, not by
  mocks (Principle V).
- Each test file's docstring `//!` MUST include a `Spec mapping:` line
  referencing the spec items it covers (US/FR/SC), as quickstart.md §8
  documents.
- Commit after each task or logical group; `cargo fmt` + `cargo clippy`
  must remain green between commits.
