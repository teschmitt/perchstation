# Implementation Plan: Capture Subsystem

**Branch**: `002-capture-subsystem` | **Date**: 2026-05-28 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/002-capture-subsystem/spec.md`

## Summary

The capture subsystem is the perchstation's sensor-facing half: a
motion-triggered recorder that converts a single fresh quiescent-to-asserted
edge on the dedicated motion sensor into exactly one bounded video clip
file and hands it to the existing clip-delivery queue via the `Inbox`
trait established by feature 001
(`crates/perchstation-core/src/queue/inbox.rs`). The capture loop runs
in-process alongside the delivery loop under `perchstation serve`,
supervised by the same tokio runtime; the only data-plane contact between
the two halves is `Inbox::submit(clip_path, ClipMeta { captured_at })`.

The technical approach is deliberately small. Two new narrow hardware
traits (`MotionSensor`, `Camera`) extend `crates/perchstation-core/src/hw_traits.rs`
without touching delivery. Production adapters live exclusively in
`crates/perchstation-hw`: `motion_sensor.rs` reads a GPIO line via the
Linux character-device ABI (`gpio-cdev`); `camera_recorder.rs` shells out
to `libcamera-vid` (the same shell-out pattern feature 001 chose for QR
capture, R-5) and writes a complete `.mp4` file. A new
`crates/perchstation-core/src/capture/` module owns the platform-agnostic
state machine: trigger → cooldown gate → liveness gate → bounded record
(outer `tokio::time::timeout` guards against camera hangs) → `Inbox::submit`.
A separate startup-time staging-purge runs before `service.ready` so a
crash mid-record can never accumulate junk across reboots; the capture
loop's status (`last_recording_at`, `last_failure`, sensor liveness) is
projected into `perchstation status` by extending the existing
`StatusSnapshot` rather than by introducing a second surface.

Phase 0 resolves the open clip-format / GPIO / threshold-default
questions raised below. Phase 1 produces a data-model document, three
contract artifacts (`hw-traits.md`, `log-events.md` extensions, `cli.md`
extensions), and a host-runnable quickstart that drives a fake sensor
and verifies a clip appears in the delivery queue's `pending/` via the
exact same `Inbox::submit` path delivery already implements.

## Technical Context

**Language/Version**: Rust, stable toolchain, edition 2024, MSRV 1.95 —
same as feature 001; no MSRV bump required.

**Primary Dependencies** (additions on top of 001's set):

- `tokio` 1.x — already the project-wide runtime; the capture loop is
  one more supervised task in the existing `serve` process.
- `tokio-util` `CancellationToken` — used by `serve` to give the capture
  task a cooperative shutdown signal alongside the existing delivery
  shutdown.
- `gpio-cdev` 0.6 — pure-Rust binding to the Linux gpiochip character
  device ABI (`/dev/gpiochip0`); production-only, lives in
  `perchstation-hw`. (Decision rationale and the `rppal` alternative are
  in `research.md` R-2.)
- `async-trait` — already in workspace; used for the new `MotionSensor`
  and `Camera` traits.
- Camera path: `libcamera-vid` (shell-out, Linux-only, runtime
  dependency from the OS package `libcamera-apps` / `rpicam-apps`). The
  binary is already part of the supported Pi OS Bookworm base image; no
  Cargo dependency.
- Dev only: existing test scaffolding (`tempfile`, `assert_cmd`,
  `chrono`). The integration tests gain a `FakeMotionSensor` (driven by
  `mpsc::UnboundedSender<MotionEdge>`) and a `FakeCamera` (writes a
  fixed-size byte payload to a staging file) under
  `tests/integration/support/`.

No `unsafe`. No new C dependencies. No `openssl-sys`. The dependency
addition footprint is exactly `gpio-cdev` (pure Rust, MIT-licensed) plus
re-use of crates already in the workspace.

**Storage**: Local filesystem. The capture subsystem adds one new
on-disk subtree under `data_dir`:

```text
<data_dir>/capture-staging/
└── <recording-id>.mp4   # in-progress clip; renamed/copied into the queue on Inbox::submit
```

`capture-staging/` is purged at startup before the capture loop becomes
ready (FR-017). It never contains terminal state — completed clips move
into `<data_dir>/queue/pending/` via the existing
`StoreInbox::submit`. See `data-model.md` for the layout and lifecycle.

The delivery queue layout (`<data_dir>/queue/{pending,inflight,delivered}/`)
is unchanged. The capture subsystem does **not** read or write any
queue subdirectory directly (FR-007); every clip enters via
`Inbox::submit`.

**Testing**: `cargo test --workspace` continues to be the single entry
point. New host-runnable integration tests under `tests/integration/`:

| Test                                  | Spec mapping              |
| ------------------------------------- | ------------------------- |
| `capture_happy.rs`                    | US1 acceptance #1, #2     |
| `capture_bounded_clip.rs`             | US1 #3 (clip duration bound) |
| `capture_idle.rs`                     | US1 #4 / SC-008 (no I/O without trigger) |
| `capture_cooldown.rs`                 | US2 #2, FR-006            |
| `capture_stuck_sensor.rs`             | US2 #3, FR-010, SC-004    |
| `capture_unavailable_sensor.rs`       | US2 #4, FR-011, SC-005    |
| `capture_recording_failure.rs`        | US2 #8, FR-008            |
| `capture_disk_pressure.rs`            | US2 #6, FR-013            |
| `capture_queue_full.rs`               | US2 #7, FR-018            |
| `capture_status_surface.rs`           | US3 #1, #2, SC-007        |
| `capture_resilience.rs`               | US2 #1, FR-009, FR-017, SC-003 (crash-restart with staging purge; queue intact); Edge Case "Sensor fires during boot or shutdown" (edge before readiness still produces a clip; edge after shutdown signal dropped cleanly) |
| `capture_camera_hang.rs`              | Edge Case "Camera adapter hangs", FR-005 (outer `tokio::time::timeout` bound), `capture.recording_hung` event |
| `capture_concurrent_event.rs`         | US2 #5 (no second concurrent recording while a previous recording is in progress) |
| `capture_isolation.rs`                | SC-009, FR-012 (capture panic does not stop delivery, and vice versa) |
| `capture_no_network.rs`               | US3 #3 (extends 001's outbound_allowlist with capture loop running) |

On-device verification that genuinely needs a Pi (real GPIO edges, real
`libcamera-vid`, real motion sensor wired to a feeder) is covered by an
addition to `deploy/RELEASE-CHECKLIST.md`, not by mocks.

**Target Platform**: Same as feature 001 — 64-bit Raspberry Pi OS
Bookworm on Pi 4 and Pi Zero 2 W (`aarch64-unknown-linux-gnu`).
Cross-compiled from x86_64 dev hosts via `cargo-zigbuild`. The
production hardware adapters compile only on `target_os = "linux"`
(matching the existing `perchstation-hw::camera_qr` pattern).

**Project Type**: Cargo workspace, no structural change. The existing
three crates absorb the new modules and adapters. See "Project
Structure" below.

**Performance Goals** (derived from spec Success Criteria):

- SC-001 — clip arrives in `pending/` with `captured_at` within 1 s of
  the sensor edge.
- SC-003 — capture loop accepts new motion events within 60 s of boot,
  including the staging-purge.
- SC-004 / SC-005 — stuck / unavailable sensor is reflected in
  `perchstation status` within 60 s of the threshold crossing.
- SC-008 — idle device performs no camera I/O; the only log emissions
  are the existing periodic-health cadence from the delivery loop.

**Derived success criteria** (no separate test built):

- SC-002 (bounded clip count over 7 days) is a structural consequence
  of FR-005 + FR-006: the worst-case rate is
  `3600 / (clip_duration_secs + cooldown_secs)` per hour, ~95 clips/h
  with the defaults (already noted in Scale/Scope below). The
  cooldown enforcement itself is covered by `capture_cooldown.rs`
  (T023); no 7-day soak is built.
- SC-006 (capture-side disk footprint within ceiling over 7 days) is
  a structural consequence of FR-013: the pre-record disk-pressure
  gate refuses to record before the ceiling is breached. The gate is
  covered by `capture_disk_pressure.rs` (T027); a 30-minute on-device
  spot-check is added to `deploy/RELEASE-CHECKLIST.md` (T039) to
  validate the structural property on real hardware.

**Constraints**:

- Memory ceiling: the capture loop's steady-state RSS contribution is
  budgeted at < 5 MB on a Pi Zero 2 W (Principle III). `libcamera-vid`
  runs as a child process with its own RSS, ~50 MB during a recording;
  this is the cost of bounded-duration recording and is acceptable
  because the camera is powered down at all other times (FR-002).
- Capture-side on-disk footprint: bounded by a configured
  `capture.max_staging_bytes` ceiling (default 256 MiB; see research.md
  R-7). Enforced before `Camera::record_clip` is called (FR-013).
- Bounded clip duration: configured `capture.clip_duration_secs`
  (default 8 s) is the inner bound; the supervisor wraps the camera
  call in `tokio::time::timeout(clip_duration + HANG_MARGIN)` so a hung
  adapter cannot pin the loop (edge case "Camera adapter hangs"). See
  research.md R-3.
- Bounded cooldown: `capture.cooldown_secs` (default 30 s).
- Sensor liveness threshold: `capture.liveness_stuck_secs` (default
  300 s). See research.md R-4.
- Sensor liveness poll cadence: `capture.liveness_poll_secs` (default
  5 s). Bounds detection latency for SC-004/SC-005: a stuck or
  unavailable sensor is reflected within ≤ `liveness_poll_secs` of the
  threshold crossing (or the first failed adapter read), comfortably
  inside the 60 s budget.
- The capture loop produces no network traffic (US3 #3); this remains
  a tested invariant via the existing `outbound_allowlist.rs` test,
  extended to keep the capture loop running for its duration. (FR-014
  forbids a new telemetry channel — a related but distinct guarantee
  enforced by reusing the existing `tracing` JSON channel.)
- `unsafe` forbidden in `perchstation-core` (Principle II + workspace
  lint); confined to `perchstation-hw`, where any `unsafe` block (the
  `gpio-cdev` crate is itself safe Rust; the camera shell-out is too)
  carries an invariant comment per the constitution.

**Scale/Scope**: A single sensor + single camera at the feeder.
Realistic trigger rate: a handful per hour at peak (a busy feeder),
typically << 1/min. The cooldown + duration bound caps the worst-case
clip rate at `3600 / (clip_duration + cooldown)` per hour — with the
defaults, ~95 clips/h upper bound, well inside the delivery queue's
500-clip / 2 GiB ceiling.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle / Gate                   | Result | Where it shows up                                                                                                                                                                                              |
| ---------------------------------- | ------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| I. Unattended Reliability          | ✅     | Capture loop fails safe: a failed recording is discarded locally (FR-008), staging-purge on startup leaves no orphaned partial files (FR-017), a queue-full refusal is logged and the clip is cleaned up (FR-018), and a panic in the capture task does not stop delivery (FR-012). The duration bound is enforced *from outside* the camera adapter so a hung adapter cannot wedge the loop (edge case "Camera adapter hangs"). |
| II. Hardware at the Boundary       | ✅     | New traits `MotionSensor` and `Camera` extend `perchstation-core/src/hw_traits.rs`; production impls live exclusively in `perchstation-hw`. The capture loop in `perchstation-core` holds only `Box<dyn MotionSensor>` and `Box<dyn Camera>` and is fully host-runnable against fakes. (FR-016.)                                                                  |
| III. Resource Discipline           | ✅     | Camera off whenever no recording is active (FR-002). Bounded clip duration (FR-005), bounded cooldown (FR-006), bounded capture-side disk footprint with explicit ceiling (FR-013). Single new pure-Rust dependency (`gpio-cdev`); no new C deps, no second runtime, no second log channel.                                                                                                                                  |
| IV. Observable, Not Chatty         | ✅     | All capture-side events flow through the existing `tracing` JSON channel as new `capture.*` event codes (`contracts/log-events.md`); the existing redaction layer and outbound allowlist apply unchanged (US3 #3 / FR-014). `perchstation status` gains capture fields on the existing snapshot — no new surface, no telemetry endpoint (FR-015).                                                                              |
| V. Test-First (non-negotiable)     | ✅     | Every functional requirement maps to at least one host-runnable integration test (matrix above). Hardware-bound paths (real GPIO edge, real `libcamera-vid`) are covered only by the release smoke test (`deploy/RELEASE-CHECKLIST.md` addition), not by mocks. The capture loop's algorithm (cooldown gating, liveness state machine, queue-refusal handling, staging purge) is TDD-eligible inside `perchstation-core`.                                  |
| Technology & resource constraints  | ✅     | Rust 2024, MSRV 1.95, single Tokio runtime, no `openssl-sys`, AGPL-3.0+ at the workspace level. `unsafe` remains confined to `perchstation-hw` (which the new adapters do not need to introduce — both `gpio-cdev` and `Command` are safe Rust). The new dep (`gpio-cdev`, MIT) is reviewed in research.md R-2.                                                                                                                                  |
| Development workflow               | ✅     | Spec-driven via speckit (this command). The constitution's subagent-driven implementation principle is honoured by placing every shared type in `data-model.md` and every interface in `contracts/` before tasks generation. Cross-project coordination with perchpub: the clip is uploaded via the existing `POST /api/v1/upload/` contract (`references/openapi.json`), which treats the body as opaque bytes; the format choice (research.md R-1) is informational. |

No violations to justify. Complexity Tracking below is empty.

## Project Structure

### Documentation (this feature)

```text
specs/002-capture-subsystem/
├── plan.md                  # This file
├── research.md              # Phase 0 — decisions, alternatives, constitution recheck
├── data-model.md            # Phase 1 — entities, state machines, on-disk staging layout
├── quickstart.md            # Phase 1 — host-runnable capture smoke
├── contracts/               # Phase 1
│   ├── hw-traits.md         # MotionSensor + Camera trait surfaces (the hardware boundary)
│   ├── log-events.md        # `capture.*` event codes (extends 001/contracts/log-events.md)
│   └── cli.md               # Status-surface additions (extends 001/contracts/cli.md)
├── checklists/              # (created by /speckit-checklist if used; not by /speckit-plan)
├── spec.md                  # Feature spec (unchanged)
└── tasks.md                 # Phase 2 — created by /speckit-tasks, NOT this command
```

### Source Code (repository root)

Additions only — no existing files are removed or renamed.

```text
crates/
├── perchstation-core/
│   ├── Cargo.toml                       # + gpio-cdev? NO — pure-trait crate.
│   └── src/
│       ├── lib.rs                       # + pub mod capture;
│       ├── config.rs                    # + #[serde(default)] pub capture: CaptureConfig
│       ├── hw_traits.rs                 # + MotionSensor, SensorLevel, Camera, RecordedClip, errors
│       ├── observability/
│       │   ├── tracing.rs               # + events::CAPTURE_* constants
│       │   └── status.rs                # + CaptureSnapshot, joined into StatusSnapshot
│       └── capture/                     # NEW: platform-agnostic capture loop
│           ├── mod.rs                   # CaptureConfig + public entry point (Capture::run)
│           ├── runner.rs                # the supervised tokio task: trigger → record → submit
│           ├── recording.rs             # bound-duration record-and-stage helper
│           ├── staging.rs               # staging dir layout + startup purge (FR-017)
│           ├── liveness.rs              # SensorLivenessTracker state machine (FR-010/FR-011)
│           ├── cooldown.rs              # CooldownState (FR-006)
│           └── state.rs                 # CaptureStateSnapshot read-side projection (FR-015)
│
├── perchstation-hw/
│   ├── Cargo.toml                       # + gpio-cdev (Linux-only feature gate)
│   └── src/
│       ├── lib.rs                       # + #[cfg(target_os = "linux")] pub mod motion_sensor; + camera_recorder;
│       ├── motion_sensor.rs             # NEW: GpioMotionSensor — gpio-cdev edge + level reader
│       └── camera_recorder.rs           # NEW: LibcameraVidCamera — shells out to libcamera-vid
│
└── perchstation/                        # binary; one wiring change in commands/serve.rs
    └── src/commands/serve.rs            # spawn Capture::run alongside DeliveryRunner + ClassifyPoller

tests/
└── integration/
    ├── capture_happy.rs                 # US1 #1, #2
    ├── capture_bounded_clip.rs          # US1 #3 / FR-005
    ├── capture_idle.rs                  # US1 #4 / SC-008
    ├── capture_cooldown.rs              # US2 #2 / FR-006
    ├── capture_stuck_sensor.rs          # US2 #3 / FR-010 / SC-004
    ├── capture_unavailable_sensor.rs    # US2 #4 / FR-011 / SC-005
    ├── capture_recording_failure.rs     # US2 #8 / FR-008
    ├── capture_disk_pressure.rs         # US2 #6 / FR-013
    ├── capture_queue_full.rs            # US2 #7 / FR-018
    ├── capture_status_surface.rs        # US3 #1, #2 / SC-007
    ├── capture_resilience.rs            # US2 #1 / FR-009 / FR-017 / SC-003 / Edge Case "Sensor fires during boot or shutdown"
    ├── capture_camera_hang.rs           # Edge Case "Camera adapter hangs" / FR-005 outer bound / capture.recording_hung
    ├── capture_concurrent_event.rs      # US2 #5 (no second concurrent recording)
    ├── capture_isolation.rs             # SC-009 / FR-012 (capture↔delivery panic isolation)
    ├── capture_no_network.rs            # US3 #3 (extends outbound_allowlist with capture running)
    └── support/
        ├── fake_motion_sensor.rs        # mpsc-backed FakeMotionSensor
        └── fake_camera.rs               # writes a fixed payload + adjustable failure modes

deploy/
├── config.example.toml                  # + [capture] section with annotated defaults
└── RELEASE-CHECKLIST.md                 # + capture smoke item: real sensor edge, real libcamera-vid
```

**Structure Decision**: No new crate. The capture subsystem cleanly
fits into the existing workspace split because the constitution's
hardware boundary (Principle II) already drew the line in the right
place. Adding `MotionSensor` and `Camera` to `perchstation-core::hw_traits`
mirrors the existing `QrFrameSource` + `Clock` pattern; the production
impls land in `perchstation-hw` next to `camera_qr.rs` and `clock.rs`;
the capture loop is a new sibling module to `delivery/` and `enrollment/`
inside `perchstation-core`. The binary's only change is one extra task
spawn in `commands/serve.rs`. The integration-test layout extends the
existing `tests/integration/` directory by name without restructuring.

## Complexity Tracking

> No constitution violations. This table is intentionally empty.

| Violation | Why Needed | Simpler Alternative Rejected Because |
| --------- | ---------- | ------------------------------------- |
| —         | —          | —                                     |
