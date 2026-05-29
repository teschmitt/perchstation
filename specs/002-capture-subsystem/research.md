# Phase 0 Research: Capture Subsystem

**Feature**: 002-capture-subsystem
**Date**: 2026-05-28
**Status**: complete — no open `NEEDS CLARIFICATION`

This document records the load-bearing technical decisions for the
capture subsystem and the alternatives that were considered. The
constitution and spec already pinned down language (Rust 2024), the
single async runtime (Tokio), the privacy posture (no telemetry, no
new outbound channels), and the existing `Inbox` trait as the sole
hand-off to delivery (`crates/perchstation-core/src/queue/inbox.rs`).
Feature 001's research locked in the workspace layout and the
hardware-boundary discipline. Everything below is downstream of those
givens.

## R-1. Clip container and codec

**Decision**: Each clip is a complete MP4 container (ISO/IEC 14496-14)
with a single H.264 (AVC) video track and no audio. The supervisor
hands the staged `.mp4` file to `Inbox::submit`; the existing
`StoreInbox` renames it into `<data_dir>/queue/pending/<clip-id>.mp4`
exactly as if a previous run of the capture loop had produced it.
Tooling on perchpub treats the bytes as opaque (see
`references/openapi.json` — the upload endpoint accepts
`multipart/form-data` with a single `file` field whose schema is
`string, format: binary`).

**Rationale**:
- `libcamera-vid` on Bookworm has direct MP4 muxing
  (`--codec h264 -o foo.mp4`) using libavformat, with no extra Rust
  dependency on our side. We pay the muxing cost in a separate process
  whose lifetime is bounded by the supervisor's outer timeout.
- H.264 is hardware-accelerated on every supported Pi via the Pi's V4L2
  M2M encoder (`/dev/video11`); the keep-the-camera-off-otherwise
  principle (FR-002) makes encode CPU cost a non-issue during long idle
  periods.
- MP4 is the de-facto web container; perchpub's downstream classify
  pipeline almost certainly already accepts it, and changing container
  later does not require a queue schema change because the queue treats
  the bytes opaquely.
- No audio means no privacy footgun and no extra muxer state.

**Alternatives considered**:
- Raw H.264 elementary stream (`.h264`): smaller, no muxing cost, but
  no container = no duration metadata, awkward to play back without
  re-muxing, and would break the existing queue/delivery contract that
  the file living under `pending/*.mp4` plays in a browser.
- Matroska (`.mkv`): also supported by libcamera-vid's libavformat
  backend; rejected for marginally less ubiquitous browser support.
- WebM (`.webm`): would force VP8/VP9 encoding, which Pi 4 and Pi Zero
  2 W can do only in software at low resolution; rejects on Principle
  III grounds.

**Cross-project coordination**: The MP4-with-H.264 choice is recorded
here as a working default; if perchpub's classify pipeline turns out to
require a different container or codec, the change is local to
`perchstation-hw::camera_recorder` and the integration-test fakes
(which produce a fixed payload), not to the trait surface or to
delivery.

## R-2. GPIO motion-sensor adapter

**Decision**: `gpio-cdev` 0.6 for the production motion-sensor adapter.
The crate is a thin pure-Rust binding to the Linux kernel's gpiochip
character-device ABI (`/dev/gpiochip0`), the upstream-supported
replacement for the deprecated sysfs GPIO. The adapter requests an
**edge-event** descriptor on the configured BCM line in
`EdgeDetect::RisingEdge` mode and an additional read-only line handle
for the periodic level probe used by liveness tracking. The Linux-only
adapter file is gated on `#[cfg(target_os = "linux")]` matching the
existing `camera_qr` adapter.

**Rationale**:
- Character-device GPIO is the supported interface on Pi OS Bookworm
  (sysfs GPIO is deprecated and slated for removal upstream).
- Kernel-side edge detection means cancellation safety on the trait is
  trivial: the kernel buffers edges into the file descriptor, so a
  cancelled `next_trigger` future does not drop an edge — the next
  call resumes from the buffered FIFO.
- Pure Rust, MIT-licensed, no `unsafe` in our adapter (the crate
  itself is `unsafe`-free above the syscall layer).

**Alternatives considered**:
- `rppal` (Pi-only HAL): higher-level (offers GPIO + I²C + SPI), works
  on every Pi, but its async edge API is poll-based, not interrupt-
  driven, which makes the cancellation-safety contract on `next_trigger`
  harder to satisfy without an extra mpsc layer in our adapter. Also
  Pi-only, which makes it slightly harder to swap the device class
  later (e.g. to a Pi 5 or a different SBC).
- Sysfs GPIO via raw `std::fs`: deprecated upstream, no kernel-side
  buffering, race-prone under cancellation.
- `gpiocdev` (the newer namesake crate, separate from `gpio-cdev`):
  also pure Rust and arguably better designed, but less widely adopted
  in the Rust embedded ecosystem at the time of writing.

The line number, active-low/active-high polarity, and debounce window
(short kernel-side hardware debounce + a software hysteresis on top
for the liveness tracker) are configurable per R-6.

## R-3. Camera adapter

**Decision**: Production `Camera` impl in `perchstation-hw::camera_recorder`
shells out to `libcamera-vid` via `tokio::process::Command`, the same
pattern feature 001 chose for QR capture in
`perchstation_hw::camera_qr` (R-5 of feature 001). The invocation is
roughly:

```sh
libcamera-vid \
    --timeout <max_duration_ms> \
    --codec h264 \
    --inline \
    --width <res_w> --height <res_h> --framerate <fps> \
    --nopreview \
    -o <staging>/<recording-id>.mp4
```

The adapter's `Recording` handle owns the child `tokio::process::Child`
plus the staging file path. Drop / supervisor abort sends `SIGTERM` to
the child, then `SIGKILL` after a short grace period, and removes the
staging file. On clean completion the child's exit status is checked;
non-zero exit with bytes already on disk is treated as a failed
recording (the partial file is discarded).

**Rationale**:
- `libcamera-vid` is part of `rpicam-apps` (Bookworm's renamed
  `libcamera-apps`), already installed on the supported Pi OS images.
- Shell-out matches the pattern feature 001 already established for the
  camera; reusing the pattern means operators have one mental model
  for "the camera is invoked via the system's `libcamera` stack".
- Process-based isolation means a libcamera crash takes the child
  down, not the perchstation daemon. The supervisor reads child
  exit status as a recording outcome.
- The supervisor wraps the camera call in
  `tokio::time::timeout(clip_duration_secs + hang_margin_secs)` (default
  margin: 2 s), so a hung libcamera process cannot pin the loop. Drop
  cancellation triggers the SIGTERM/SIGKILL cleanup path.

**Alternatives considered**:
- Direct libcamera Rust bindings (`libcamera-rs`): API is still
  evolving and binding maintenance is sparse; deferred for the same
  reason feature 001 deferred them for QR.
- V4L2 via `nokhwa` / `v4l`: works on Pi 4 but the supported camera
  stack on Bookworm is libcamera; V4L2 compatibility shims are flaky.
- A C FFI shim into libcamera directly: would introduce `unsafe` and
  build-system complexity in a crate that today contains zero `unsafe`.

## R-4. Clip duration, cooldown, and liveness defaults

**Decision**:

| Knob                          | Default | Configurable | Rationale                                              |
| ----------------------------- | ------- | ------------ | ------------------------------------------------------ |
| `capture.clip_duration_secs`  | 8       | yes          | Long enough to see a bird arrive + pose; short enough that one visit produces a small handful of clips, not dozens. |
| `capture.hang_margin_secs`    | 2       | yes          | Outer timeout slack. The supervisor times out the camera call at `clip_duration + hang_margin`; the camera is supposed to stop itself at `clip_duration`. |
| `capture.cooldown_secs`       | 30      | yes          | Bounds the per-hour clip rate during a sustained visit at ~95 clips/h max (`3600 / (8 + 30)`), comfortably inside delivery's 500-clip/2 GiB ceiling. |
| `capture.liveness_stuck_secs` | 300     | yes          | A real visit is rarely 5 contiguous minutes; sustained-asserted longer than that suggests a wiring fault or sun-baked sensor housing. |
| `capture.liveness_poll_secs`  | 5       | yes          | Period of the level probe (`MotionSensor::level()`) that drives the liveness tracker. Five seconds gives a 60 s upper bound on the SC-004 / SC-005 detection latency with margin. |

These are the spec's "tuning knobs decided during planning"
(Assumptions §); the spec requires each bound exists and is enforced,
not that any particular value is chosen. Each appears in
`config.rs::CaptureConfig` with the values above as the
`#[serde(default)]`.

**Rationale**: All five are operator-tunable for unusual deployments
(a research feeder that wants 30 s clips, a high-traffic urban feeder
that wants a tight 60 s cooldown). The defaults match an unattended
backyard feeder watched from a hobbyist's phone.

**Alternatives considered**:
- Hard-coded constants: simpler today, but the spec explicitly calls
  out these as operator-tunable; making them config fields now costs
  ~30 lines and avoids a behaviour-changing config schema bump later.
- A single "sensitivity" knob: hides the bounds, makes it harder to
  diagnose "why did the device record so many clips today".

## R-5. Capture-loop process model

**Decision**: The capture loop is a single supervised tokio task
spawned by `perchstation::commands::serve::run` alongside the existing
`DeliveryRunner` and `ClassifyPoller` tasks. The three tasks share the
same `Arc<dyn Clock>` and the same `tracing` subscriber. The capture
task holds `Box<dyn MotionSensor>`, `Box<dyn Camera>`, an
`Arc<dyn Inbox>` (the `PolicyInbox<StoreInbox>` already wired up for
delivery), an `Arc<CaptureState>` (the read-side projection
`perchstation status` consumes), and a `CancellationToken` for clean
shutdown.

The loop's heart is a `tokio::select!` on three sources:

```rust
loop {
    tokio::select! {
        trigger = self.sensor.next_trigger() => { self.handle_trigger(trigger).await; }
        _ = liveness_tick.tick() => { self.update_liveness(self.clock.now()).await; }
        _ = self.shutdown.cancelled() => break,
    }
}
```

`handle_trigger` is the cooldown-gate → liveness-gate → record →
submit sequence. `update_liveness` does a non-blocking `level()` probe
and updates the `LivenessTracker`. The recording step blocks the loop
for the clip duration plus a small margin; during that window neither
new triggers nor liveness ticks are observed — that is intentional and
matches the spec's "no second concurrent recording" rule (US2 #5).

**Rationale**:
- Single task = single state machine, no inter-task locking on
  cooldown/liveness fields.
- Restart-as-recovery (Principle I): the capture task is supervised the
  same way as `DeliveryRunner` (`tokio::spawn` + `tokio::select!` on
  SIGTERM in `serve`); a panic in capture takes only the capture task
  down and is logged but does not stop delivery (FR-012). Future work
  may add an explicit restart wrapper around the capture task; today
  the supervisor logs the panic and the systemd `Restart=always`
  policy brings the whole process back if needed.
- Cancellation safety: `MotionSensor::next_trigger` is contractually
  cancellation-safe (R-2's kernel-buffered FIFO), so the `select!`
  pattern is correct.

**Alternatives considered**:
- Two tasks (trigger handler + liveness checker) sharing
  `Arc<Mutex<...>>`: more lock surface area, no real benefit.
- A separate process / IPC: rejected by the spec assumption that
  capture and delivery share the `serve` process and supervision tree.
- A second tokio runtime: rejected by the constitution's single-
  runtime constraint.

## R-6. Capture-side staging layout and startup purge

**Decision**: One new directory per `data_dir`:

```text
<data_dir>/capture-staging/
└── <recording-id>.mp4   # in-progress; renamed/copied into the queue on Inbox::submit
```

`<recording-id>` is `<capture_utc_basic>-cap` (the suffix
distinguishes a capture-side staging file from a queue clip-id without
clashing with the queue's per-process atomic counter). The directory
itself is `mkdir -p`-created on first use by the capture supervisor and
is purged before `service.ready` by deleting every file underneath it.

The supervisor never writes anywhere else: the only side-effect of a
successful recording is `Inbox::submit(staging_path, ClipMeta { … })`,
after which the staging file has been consumed by the queue's enqueue
(rename + sidecar write).

**Rationale**:
- A separate directory keeps the queue's invariant ("`pending/`
  contains only complete, ready-to-deliver clips with sidecars")
  intact — partial clips never appear there.
- Startup purge satisfies FR-017 (no accumulated junk across reboots).
- Putting staging under `data_dir` keeps everything operator-facing in
  one tree; the systemd `ReadWritePaths=` line already covers it.
- Rename-into-queue is the same atomic operation `StoreInbox::enqueue`
  already implements; cross-filesystem fallback (copy + remove) is
  also already handled.

**Alternatives considered**:
- Stage into `/tmp`: `/tmp` lives on tmpfs in many distros, which
  makes the capture-side disk ceiling impossible to enforce (`tmpfs`
  exhausts RAM, not SD card). Rejected.
- Stage directly into `queue/pending/` with a `.tmp` suffix and rename
  on completion: would couple the capture state machine to queue's
  internal layout, exactly what FR-007 forbids.
- A per-recording subdirectory: pointless complexity for a single
  in-flight clip at a time (US2 #5).

## R-7. Capture-side disk-footprint ceiling

**Decision**: `capture.max_staging_bytes` defaults to 256 MiB. Enforced
in two places:

1. Before each `Camera::record_clip` call, the supervisor scans
   `capture-staging/`, sums file sizes, and refuses to record if the
   sum would exceed the ceiling (`capture.disk_pressure_skip` event,
   `cooldown` starts so we don't tight-loop).
2. The supervisor also enforces a much smaller in-flight ceiling
   implicitly via the bounded clip duration and the camera's bitrate
   ceiling — a single 8 s 1080p clip at the libcamera-vid default
   bitrate (~8 Mbps) is under 10 MiB, so a single ongoing recording
   cannot blow the ceiling.

**Rationale**:
- 256 MiB is comfortably larger than any single clip and small enough
  that an SD card with ~16 GiB free does not need extra capacity
  reserved for capture.
- The ceiling is configurable so operators with smaller cards (and
  longer expected delivery outages) can lower it; the trade-off is
  more "capture skipped due to disk pressure" events during the
  outage.

The check is filesystem-side (`fs::read_dir` + `metadata`) rather than
in-memory accounting, so a crash that left stale staging files behind
is reflected in the next decision — and the startup purge zeroes it
anyway.

**Alternatives considered**:
- A statvfs-based "% free" check on `data_dir`: also useful but
  conflates capture pressure with queue pressure (which has its own
  ceilings); we keep them separate.
- No ceiling: violates FR-013 and Principle III.

## R-8. Sensor-liveness state machine

**Decision**: Three explicit states — `Healthy`, `StuckAsserted`,
`Unavailable` — tracked by a `SensorLivenessTracker` in
`crates/perchstation-core/src/capture/liveness.rs`. Transitions:

```text
                        ┌───────────────────────────────┐
                        │                               │
                        ▼                               │
                  ┌──────────┐  level=Asserted        │
   start ───────► │ Healthy  │ ──── for ≥ stuck_secs ─┐│
                  └──────────┘                        ││
                       ▲                              ▼│
                       │ level=Quiescent      ┌───────────────┐
                       │                      │ StuckAsserted │
                       │                      └───────────────┘
                       │                              ▲│
                       │           sensor errored     ││ level returns to Quiescent
                       │ ────────────────────────────┐│▼
                       │                             │┌───────────────┐
                       └──── adapter recovers ───────┤│ Unavailable   │
                                                     ││               │
                                                     │└───────────────┘
                                                     │
                                                     └ adapter level() returned Err
```

`Healthy → StuckAsserted` when the periodic level probe observes
`Asserted` continuously for `liveness_stuck_secs`. Transition emits
`capture.sensor_degraded { kind: "stuck_asserted" }`.

`StuckAsserted → Healthy` when the level probe observes `Quiescent`.
Transition emits `capture.sensor_recovered { kind: "stuck_asserted" }`.

`* → Unavailable` when `MotionSensor::level()` returns `Err` or
`MotionSensor::next_trigger` returns `Err`. Transition emits
`capture.sensor_degraded { kind: "unavailable" }`.

`Unavailable → Healthy` when a subsequent probe returns `Ok`.
Transition emits `capture.sensor_recovered { kind: "unavailable" }`.

While in `StuckAsserted` or `Unavailable`, the supervisor refuses to
start a recording even on a fresh trigger (`capture.degraded_skip`
event). The states are exposed through the read-side `CaptureSnapshot`
(`sensor_liveness` field) so `perchstation status` shows them
directly.

**Rationale**:
- Matches FR-010, FR-011, US2 #3, US2 #4, SC-004, SC-005 1-for-1.
- Three explicit states avoid the "is degraded a Bool with a tag?"
  ambiguity that would otherwise come up in tests.

**Alternatives considered**:
- Conflating stuck and unavailable into one degraded state: harder to
  diagnose, fails the spec's "clearly attributed to the sensor, not to
  delivery or enrollment" requirement (US3 #2).
- Putting the state machine in `perchstation-hw`: would make the
  trait surface impure and is impossible to test without hardware.

## R-9. Queue refusal handling

**Decision**: The capture loop treats `InboxError::QueueFull` and
`InboxError::Queue(...)` as the queue's authoritative refusals
(FR-018). The supervisor:

1. Removes the staging file (`fs::remove_file`; ignore `NotFound`).
2. Emits `capture.queue_refused { kind: "queue_full" | "queue_io", … }`.
3. Starts the cooldown deadline anyway, so a queue-full state does not
   tight-loop on the next trigger.
4. Returns to the supervisor loop.

The capture loop does **not**:
- Retry the submission (delivery's retry logic owns that).
- Implement its own queueing or pending-clip directory.
- Write into `queue/pending/` directly.

**Rationale**: matches FR-018 verbatim. Delivery's queue-full policy
(`drop_oldest_undelivered` by default, `refuse_new` opt-in) is the
single source of truth; capture's only responsibility is to not make
the situation worse.

## R-10. Configuration schema additions

**Decision**: `Config` gains a `[capture]` section, added via
`#[serde(default)] pub capture: CaptureConfig` so existing 001-only
config files continue to parse:

```toml
[capture]
clip_duration_secs   = 8
cooldown_secs        = 30
hang_margin_secs     = 2
liveness_stuck_secs  = 300
liveness_poll_secs   = 5
max_staging_bytes    = 268_435_456   # 256 MiB
sensor_gpiochip      = "/dev/gpiochip0"
sensor_line          = 17            # BCM line number
sensor_active_high   = true
camera_width         = 1280
camera_height        = 720
camera_framerate     = 30
camera_bitrate_bps   = 4_000_000
```

The hardware-specific fields (`sensor_gpiochip`, `sensor_line`,
`sensor_active_high`, the camera resolution/bitrate) are only consumed
in `perchstation-hw` when constructing the production adapters;
`perchstation-core`'s capture loop never sees them.

**Rationale**: TOML matches the existing 001 schema. `[serde(default)]`
on every field plus `[serde(default)]` on the section makes the
section omitable in dev configs.

**Alternatives considered**:
- Environment variables: less discoverable than a file the operator
  can read with `cat`; would diverge from 001's pattern.
- A separate `/etc/perchstation/capture.toml`: extra file the operator
  has to keep in sync, no value.

## R-11. Subagent-driven implementation readiness

**Decision**: The constitution requires (per the 2026-05-28 amendment)
that "all shared types MUST be in `data-model.md` and all interfaces
in `contracts/` before implementation begins. Each task MUST be
self-contained (context, file paths, acceptance criteria) and touch
1–2 files." Phase 1's `data-model.md` and `contracts/hw-traits.md`
explicitly enumerate every shared type (the trait surfaces, the
configuration struct, the status projection, the staging-file
recording-id format, the sidecar `ClipMeta` already established by
001) so subagent-driven tasks generated by `/speckit-tasks` have a
single source of truth.

**Rationale**: The capture subsystem touches three crates and the
existing serve command; without the data-model + contracts being the
spine, sub-agents implementing one trait would otherwise have to
re-derive the shape from prose.

## R-12. Re-evaluation against existing observability and privacy invariants

**Decision**: The capture subsystem adds **zero** new outbound
destinations and **zero** new log channels. All new events are
`capture.*` codes flowing through the existing `tracing` JSON layer
(`contracts/log-events.md` extension). The
`tests/integration/outbound_allowlist.rs` test from 001 is extended
(`capture_no_network.rs`) to keep the capture loop running for the
duration of its assertion window; a passing test gives the spec's US3
#3 / FR-014 "no network traffic" guarantee.

**Rationale**: Closes the loop on the spec's "the capture subsystem
itself contributes no network traffic" requirement as a *tested
invariant* rather than a code comment, matching the discipline 001
applied to delivery.

## Constitution recheck

After this round of decisions, the Constitution Check section in
`plan.md` remains green: no principle requires an exception, no entry
needs to be added to Complexity Tracking.

- Principle I — Unattended Reliability: every failure mode (camera
  hung, sensor stuck, sensor unavailable, queue full, disk full,
  staging crash) maps to an explicit handler above with no
  silent-data-loss path.
- Principle II — Hardware at the Boundary: R-2 and R-3 keep
  hardware-touching code strictly inside `perchstation-hw` behind the
  two new traits added to `perchstation-core::hw_traits`.
- Principle III — Resource Discipline: bounded duration, bounded
  cooldown, bounded staging footprint, single new pure-Rust dep, no
  second runtime.
- Principle IV — Observable, Not Chatty: capture events use the
  existing structured-log channel; no telemetry; status surface
  extends the existing one.
- Principle V — Test-First (non-negotiable): every functional
  requirement maps to at least one host-runnable integration test (per
  the matrix in `plan.md`'s Testing section), and the few genuinely
  hardware-only paths (real GPIO edge, real `libcamera-vid`) are
  covered by an addition to the release smoke test, not by mocks.
