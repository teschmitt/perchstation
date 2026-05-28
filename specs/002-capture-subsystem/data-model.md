# Phase 1 Data Model: Capture Subsystem

**Feature**: 002-capture-subsystem
**Date**: 2026-05-28

The capture subsystem holds almost all of its state in memory inside
the long-running `Capture` task. The only on-disk state it owns is the
staging directory `<data_dir>/capture-staging/`, which holds at most
one in-progress clip at a time and is purged before
`service.ready` on every boot (FR-017). All durable state (captured
clips, sidecars, queue history) lives under the existing
`<data_dir>/queue/` tree managed by feature 001's
`crates/perchstation-core/src/queue/` modules and is touched **only**
via `Inbox::submit` (FR-007).

```text
<data_dir>/
├── capture-staging/                  # NEW: in-progress recording staging
│   └── <recording-id>.mp4            # at most one at a time; purged on boot
└── queue/                            # unchanged from feature 001
    ├── pending/                      # capture's Inbox::submit lands here via StoreInbox::enqueue
    │   ├── <clip-id>.mp4
    │   └── <clip-id>.json            # ClipQueueEntry built from ClipMeta
    ├── inflight/
    └── delivered/
```

`<recording-id>` is `<capture_utc_basic>-cap` (e.g.
`20260528T142312Z-cap`). The `-cap` suffix is intentional: it is a
local-to-capture-subsystem identifier and never appears in the queue
or in delivery's logs, where the `<clip-id>` minted by
`QueueStore::enqueue` (`<capture_utc_basic>-<seq>`) is authoritative.

---

## Entity: MotionTriggerEvent

**Lifetime**: in-memory only, transient. Produced by
`MotionSensor::next_trigger`, consumed exactly once by
`Capture::handle_trigger`. Never written to disk.

**Fields**:

| Field         | Type                   | Notes                                                                                                                                |
| ------------- | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `at`          | RFC 3339 UTC timestamp | Wall-clock time of the quiescent-to-asserted edge. Forwarded into the resulting clip's `ClipMeta.captured_at` (SC-001).             |

**Invariants**:
- Only "fresh" edges produce trigger events. The
  `crates/perchstation-hw/src/motion_sensor.rs` adapter relies on the
  kernel-buffered gpiochip edge FIFO: an edge that arrived while
  `next_trigger` was not being awaited is still observed on the next
  call (research.md R-2). The fake `MotionSensor` for tests preserves
  this contract via an `mpsc::UnboundedSender<DateTime<Utc>>`.

**Dropped events** (per spec FR-004 / FR-006 / Edge Cases):
- Triggers observed during cooldown are visible to the supervisor but
  are not turned into recordings (they fall through `Capture::handle_trigger`'s
  gate).
- Triggers observed while the sensor liveness state is
  `StuckAsserted` or `Unavailable` are similarly ignored.
- Triggers that arrive during boot (before the capture loop reaches
  its `select!`) are observed on the first call to `next_trigger` if
  the adapter's gpiochip subscription was opened before they fired;
  the supervisor's startup order opens the subscription **after** the
  staging purge but **before** `service.ready`, so the worst case is
  one stale edge that may turn into a clip on the very first iteration —
  acceptable per the Edge Case "Sensor fires during boot or shutdown".

---

## Entity: Recording

**Lifetime**: in-memory only, transient. Represents a single bounded
attempt to capture a clip. Lives inside
`Capture::handle_trigger` from the moment a trigger is accepted until
either submission completes or the staging file is cleaned up.

**State machine**:

```text
                          ┌───────────────┐
   trigger accepted ─────►│   Starting    │
                          └──────┬────────┘
                                 │ Camera::record_clip resolved
                       ┌─────────┴────────┐
                       ▼                  ▼
                  ┌─────────┐        ┌─────────┐
                  │Completed│        │ Failed  │
                  └────┬────┘        └────┬────┘
                       │                  │
                       │                  │ remove staging
                       │ Inbox::submit    │ emit capture.recording_failed
                       │                  ▼
              ┌────────┴────┐         (back to idle)
              ▼             ▼
        ┌──────────┐  ┌────────────┐
        │Submitted │  │QueueRefused│
        └──────────┘  └─────┬──────┘
                            │ remove staging
                            │ emit capture.queue_refused
                            ▼
                       (back to idle)
```

**Fields** (mostly derived from the `Camera::record_clip` return):

| Field             | Type                   | Notes                                                                                       |
| ----------------- | ---------------------- | ------------------------------------------------------------------------------------------- |
| `triggered_at`    | RFC 3339 UTC timestamp | The `MotionTriggerEvent.at` that started this recording.                                    |
| `started_at`      | RFC 3339 UTC timestamp | Wall-clock at the moment `Camera::record_clip` was awaited.                                 |
| `ended_at`        | RFC 3339 UTC timestamp | Wall-clock at completion (clean or failed).                                                 |
| `staging_path`    | `PathBuf`              | `<data_dir>/capture-staging/<recording-id>.mp4`. Owned by the supervisor for the recording's lifetime. |
| `byte_size`       | u64                    | Bytes the camera wrote. Validated as nonzero before `Inbox::submit`.                        |
| `outcome`         | enum                   | `Completed` / `Failed { kind, message }` / `Submitted { clip_id }` / `QueueRefused { kind }`. |

**Invariants**:
- At most one `Recording` exists at a time per `Capture` instance.
  US2 #5 ("no second concurrent recording") is structurally true
  because the supervisor's `select!` does not advance until
  `handle_trigger` returns.
- The supervisor MUST remove the staging file on any non-`Submitted`
  outcome (FR-008). The fake `Camera` in tests verifies this by
  asserting `staging_path` does not exist after a failed recording.
- `triggered_at`, when handed to `Inbox::submit`, becomes the
  `ClipMeta.captured_at` field — i.e., the resulting `ClipQueueEntry`'s
  `captured_at` reflects the time of the trigger (FR-007 / SC-001).
  This is **not** `started_at` (which is slightly later) and **not**
  `ended_at` (which is much later, after the clip duration).

---

## Entity: CooldownState

**Lifetime**: in-memory, lives inside the `Capture` task for its
lifetime. Holds at most one outstanding cooldown deadline.

**Fields**:

| Field           | Type                                  | Notes                                                              |
| --------------- | ------------------------------------- | ------------------------------------------------------------------ |
| `until`         | `Option<DateTime<Utc>>`               | `Some(t)` while cooldown is active; `None` when idle.              |
| `last_outcome`  | enum `{ Submitted, Failed, QueueRefused, DegradedSkip, DiskPressureSkip }` | Decides which event is emitted on the next trigger skip; informational. |

**Lifecycle**:
- `start_after(now, cooldown_secs)` sets `until = Some(now + cooldown_secs)`.
- `is_active(now)` returns `until.map_or(false, |t| t > now)`.
- Cleared by elapsing wall-clock time; no explicit `clear`.

**Invariants**:
- A cooldown is started after *every* completed `handle_trigger` call,
  regardless of outcome (success, failure, queue-refused, degraded
  skip, disk-pressure skip). Without this, a sustained-asserted
  sensor would produce back-to-back attempts the moment recording
  failed (violates FR-006 in spirit and US2 #2 in letter).
- Cooldown does not stop the supervisor from *observing* fresh trigger
  edges (the kernel still buffers them); it only stops them from
  turning into recordings.

---

## Entity: SensorLivenessTracker

**Lifetime**: in-memory, lives inside the `Capture` task for its
lifetime. Reads inputs from `MotionSensor::level()` (called by the
liveness tick) and `MotionSensor::next_trigger`'s error case (called
from `handle_trigger`).

**State**:

```rust
pub enum SensorLiveness {
    Healthy,
    StuckAsserted { since: DateTime<Utc> },
    Unavailable  { since: DateTime<Utc>, reason: String },
}
```

**Inputs**:

| Input                        | Source                                                       |
| ---------------------------- | ------------------------------------------------------------ |
| Level probe `Ok(Asserted)`   | `MotionSensor::level()` from the periodic liveness tick.     |
| Level probe `Ok(Quiescent)`  | `MotionSensor::level()`.                                     |
| Level probe `Err(_)`         | `MotionSensor::level()` — triggers the `Unavailable` branch. |
| Trigger error                | `MotionSensor::next_trigger()` returning `Err`.              |

**Transitions** (full diagram in `research.md` R-8):

| From            | Input                                        | To                       | Side-effects                                        |
| --------------- | -------------------------------------------- | ------------------------ | --------------------------------------------------- |
| Healthy         | Asserted for ≥ `liveness_stuck_secs` continuously | StuckAsserted        | emit `capture.sensor_degraded { kind: "stuck_asserted", since }` |
| StuckAsserted   | Quiescent observed                           | Healthy                  | emit `capture.sensor_recovered { kind: "stuck_asserted" }` |
| *               | level()/next_trigger() returned Err          | Unavailable              | emit `capture.sensor_degraded { kind: "unavailable", reason }` |
| Unavailable     | level() returned Ok                          | Healthy                  | emit `capture.sensor_recovered { kind: "unavailable" }` |

**Invariants**:
- `is_degraded()` returns true iff state is `StuckAsserted` or
  `Unavailable`. The supervisor consults this *before* starting a
  recording even when a trigger arrives (US2 #3, US2 #4).
- The transition to `Healthy` is *automatic* once the adapter
  recovers; no operator action is required (FR-010, FR-011, SC-005).

---

## Entity: CaptureSnapshot (read-side projection)

**Naming**: `CaptureState` is the mutable in-process holder (an
`Arc<RwLock<CaptureStateInner>>`, owned by the supervisor and updated
on each handled trigger). `CaptureSnapshot` is the immutable projection
produced by `CaptureState::snapshot()` and joined into the existing
`StatusSnapshot` as a new `capture: CaptureSnapshot` field. The two
names are intentionally distinct: writers see `CaptureState`, readers
see `CaptureSnapshot`.

**Lifetime**: a single read-only snapshot, rebuilt on demand by
`perchstation status`. Not persisted. Joined into the existing
`StatusSnapshot` defined in
`crates/perchstation-core/src/observability/status.rs` as a new
`capture: CaptureSnapshot` field.

**Fields**:

| Field                  | Type                                        | Notes                                                     |
| ---------------------- | ------------------------------------------- | --------------------------------------------------------- |
| `last_recording_at`    | `Option<DateTime<Utc>>`                     | Wall-clock time of the most recent **successful** clip submission. None if the device has never recorded. |
| `last_clip_id`         | `Option<String>`                            | The `ClipQueueEntry.clip_id` returned by the latest successful `Inbox::submit`. Mirrors the queue clip-id so an operator can grep it. |
| `last_failure`         | `Option<CaptureFailureSnapshot>`            | Most recent capture failure (recording failed, queue refused, disk pressure, etc.).                       |
| `sensor_liveness`      | `enum CaptureLivenessSnapshot`              | `never_observed` / `healthy` / `stuck_asserted` / `unavailable`. The first three of the latter three are projected from `SensorLivenessTracker`'s current state; `never_observed` is the default before the supervisor's first liveness probe (e.g. when `status` is invoked outside of `serve`). |
| `sensor_degraded_since`| `Option<DateTime<Utc>>`                     | Present (`Some`) when `sensor_liveness` is `stuck_asserted` or `unavailable`; `None` for `healthy` and `never_observed`. |

**`CaptureFailureSnapshot`**:

| Field        | Type           | Notes                                                                       |
| ------------ | -------------- | --------------------------------------------------------------------------- |
| `at`         | RFC 3339 UTC   | When the failure was observed.                                              |
| `kind`       | string         | `"recording_failed"`, `"camera_hang"`, `"queue_full"`, `"queue_io"`, `"disk_pressure"`. |
| `message`    | string         | Human-readable summary (no PEM bodies; no secrets).                          |

**Construction**: the supervisor task owns an `Arc<RwLock<CaptureStateInner>>`
that it updates on each handled trigger (start, success, failure,
liveness transition). `status::snapshot(data_dir, now)` clones the
inner data via a read-lock acquisition — read-only with respect to
on-disk state (so it is still safe to run alongside `serve`).

**Why a read-side projection rather than reading the queue**: the
spec asks `perchstation status` to report **capture-side** state —
specifically the last *recording* time, the last *capture* failure,
and sensor liveness. These are not derivable from the queue (the queue
knows last *upload* success/failure). The projection bridges the two
halves cleanly inside the existing `StatusSnapshot`. When the capture
task is not running (e.g. a tool invokes `status` outside of `serve`),
the projection defaults to `None` for every timestamp / failure field
and to `CaptureLivenessSnapshot::NeverObserved` for `sensor_liveness`
— consistent with the "status is safe to run anywhere" promise from
`001/contracts/cli.md`, and explicitly distinct from the `Healthy`
value the supervisor publishes only after a successful liveness probe.

---

## Configuration (parsed view)

Loaded once at process start by the existing `Config::load`. Adds the
`[capture]` section documented in `research.md` R-10. All fields have
`#[serde(default)]` so the section may be omitted.

| Field                                | Type   | Default      | Consumed by                              |
| ------------------------------------ | ------ | ------------ | ---------------------------------------- |
| `capture.clip_duration_secs`         | u64    | 8            | supervisor (passed to `Camera::record_clip`) |
| `capture.hang_margin_secs`           | u64    | 2            | supervisor (outer `tokio::time::timeout`) |
| `capture.cooldown_secs`              | u64    | 30           | `CooldownState::start_after`             |
| `capture.liveness_stuck_secs`        | u64    | 300          | `SensorLivenessTracker`                  |
| `capture.liveness_poll_secs`         | u64    | 5            | `tokio::time::interval` in the supervisor loop |
| `capture.max_staging_bytes`          | u64    | 268_435_456  | supervisor (pre-record disk-pressure gate) |
| `capture.sensor_gpiochip`            | path   | `/dev/gpiochip0` | `perchstation-hw::motion_sensor` only |
| `capture.sensor_line`                | u32    | 17           | `perchstation-hw::motion_sensor` only    |
| `capture.sensor_active_high`         | bool   | true         | `perchstation-hw::motion_sensor` only    |
| `capture.camera_width`               | u32    | 1280         | `perchstation-hw::camera_recorder` only  |
| `capture.camera_height`              | u32    | 720          | `perchstation-hw::camera_recorder` only  |
| `capture.camera_framerate`           | u32    | 30           | `perchstation-hw::camera_recorder` only  |
| `capture.camera_bitrate_bps`         | u64    | 4_000_000    | `perchstation-hw::camera_recorder` only  |

The hardware-specific fields (`sensor_*`, `camera_*`) are intentionally
absent from the supervisor's view of `CaptureConfig` — they are
constructor parameters for the production adapters in
`perchstation-hw` and do not appear in the capture loop or in any
fake. Splitting them out keeps the capture-loop tests free of
hardware knobs they have no use for.

---

## Mapping to spec requirements

| Spec entity             | Maps to                                                           |
| ----------------------- | ----------------------------------------------------------------- |
| Motion Trigger Event    | `MotionTriggerEvent` (in-memory)                                  |
| Recording               | `Recording` (in-memory, single instance per `Capture` task)       |
| Cooldown State          | `CooldownState` (in-memory)                                       |
| Sensor Liveness State   | `SensorLivenessTracker` + `SensorLiveness` enum (in-memory)       |

---

## Mapping to delivery's existing on-disk types

| Delivery type (001)        | Capture's use                                                              |
| -------------------------- | -------------------------------------------------------------------------- |
| `Inbox` (trait)            | Sole hand-off surface. Capture holds an `Arc<dyn Inbox>` cloned from the wiring in `serve`. (FR-007.) |
| `ClipMeta`                 | Constructed per recording with `captured_at = MotionTriggerEvent.at`. No new fields needed (the spec's `captured_at` requirement maps 1:1 onto the existing field). |
| `InboxError::QueueFull`    | Handled in `Capture::handle_trigger` by removing the staging file and emitting `capture.queue_refused { kind: "queue_full" }` (FR-018). |
| `InboxError::Queue(_)`     | Handled identically with `kind: "queue_io"`.                               |
| `ClipQueueEntry.clip_id`   | Mirrored into `CaptureSnapshot.last_clip_id` for operator visibility (FR-015). |

No changes to delivery's types are required for the capture
subsystem; the existing `ClipMeta { captured_at }` is sufficient.
