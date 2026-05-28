# Feature Specification: perchstation Capture Subsystem

**Feature Branch**: `002-capture-subsystem`

**Created**: 2026-05-28

**Status**: Draft

**Input**: User description: "Design the perchstation capture subsystem MVP: a motion-triggered recorder that produces complete video clip files and submits each one to the existing clip-delivery queue. The downstream contract is feature 001's Inbox trait at `crates/perchstation-core/src/queue/inbox.rs` (with `ClipMeta`) — treat that as the source of truth for the hand-off; the capture subsystem must not duplicate, bypass, or contort it."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Motion Event Captured and Handed Off (Priority: P1)

A bird lands on the feeder. The dedicated motion sensor mounted at the feeder fires. The station — which has been sitting idle with the camera powered down — opens the camera, records a single video clip of bounded duration, closes the camera, and hands the finished clip file to the delivery queue. From the delivery subsystem's perspective, a new clip simply appears via the same `Inbox::submit` call any other producer would use; from the operator's perspective, a clip eventually arrives in perchpub without their having to do anything.

**Why this priority**: This is the minimum viable product for capture. Until a single sensor fire turns into a single complete clip in the delivery queue, the station has nothing to deliver and the entire device is inert. Every other capture concern — resilience, observability, sustained-presence handling — is meaningless without this loop.

**Independent Test**: With the station running `perchstation serve` and the delivery queue empty, simulate a single motion-sensor edge through the hardware adapter's test surface. Verify that exactly one clip file appears in the delivery queue's `pending/` directory with a well-formed sidecar whose `captured_at` reflects the time of the trigger, and that the camera is powered down again afterwards.

**Acceptance Scenarios**:

1. **Given** the station is running, the queue is empty, and the camera is currently off, **When** a single motion-sensor event fires, **Then** the station opens the camera, records exactly one clip of the configured maximum duration (or shorter if the recording terminates cleanly earlier), closes the camera, and submits the finished clip to delivery via the existing `Inbox::submit` path with `captured_at` set to the time of the trigger.
2. **Given** a motion-sensor event triggers a recording, **When** the recording completes successfully, **Then** the resulting file is the only artefact handed to delivery — there is no separate notification, no side-channel write into the queue directory, and no bypass of the `Inbox` trait.
3. **Given** a motion-sensor event continues to assert past the configured clip duration, **When** the recording reaches that duration, **Then** the station stops recording at the bound, submits the bounded clip, and does not begin a follow-up recording until the cooldown has elapsed.
4. **Given** no motion-sensor events have fired, **When** the station is observed over an extended idle period, **Then** no clips are produced, the camera remains powered down, and the capture loop does not perform any video I/O.

---

### User Story 2 - Robust Capture Under Adverse Conditions (Priority: P2)

The device runs unattended outdoors for months. Power flickers in the middle of a recording. A bird sits on the feeder for half an hour. The motion sensor's wiring works loose and the GPIO line floats high forever. The SD card fills up. In all of these cases the station must fail safe: no corrupted queue entries, no runaway clip production, no silent loss of subsequent visits once the condition clears.

**Why this priority**: Critical to the unattended-reliability promise but only meaningful once US1 works. Without these guarantees the device performs well in lab conditions and fails the first time it encounters real-world weather, fauna, or hardware aging.

**Independent Test**: With US1 working, induce each adverse condition in turn — power loss mid-recording, a sustained sensor-asserted signal far longer than one clip duration, a sensor that never fires, and a disk full of stale capture-side staging files. After each, verify the queue is intact, capture-side disk footprint stays within its configured ceiling, and the station resumes normal operation without operator intervention once the condition clears.

**Acceptance Scenarios**:

1. **Given** a recording is in progress, **When** power is lost and then restored, **Then** the boot-time state of the delivery queue contains no partial clip and no corrupted sidecar — at worst the in-progress clip is gone — and the capture loop resumes accepting new motion events within the same restart window as the delivery loop.
2. **Given** the motion sensor remains continuously asserted for longer than one clip duration plus cooldown, **When** the cooldown expires, **Then** the station does NOT immediately re-record the same sustained presence as a second clip; further recordings only occur after the sensor first returns to its quiescent state and then re-asserts.
3. **Given** the motion sensor has been continuously asserted beyond a configured liveness threshold (i.e., the signal looks stuck rather than reflecting a real plausible visit), **When** the threshold is crossed, **Then** the station logs the degraded state, surfaces it through the existing status surface, and refuses to record again until the sensor returns to quiescent.
4. **Given** the hardware adapter for the motion sensor reports an I/O or connectivity failure, **When** the failure is detected, **Then** the station logs the degraded state and surfaces it through the existing status surface; the capture loop continues running so it can recover automatically once the sensor returns.
5. **Given** a motion event fires while a previous recording is still being staged or handed to delivery, **When** the new event arrives, **Then** the station does not begin a second concurrent recording; the new event is either folded into the active recording (if still within the configured duration) or ignored under cooldown.
6. **Given** the device's storage is at or near its configured capture-side ceiling, **When** a new motion event would otherwise start a recording, **Then** the station declines to record, logs the decision with enough context to diagnose later, and does not crash the capture loop.
7. **Given** the delivery queue refuses an incoming clip (e.g., `Inbox` returns a queue-full error under the configured eviction policy), **When** the capture loop observes the refusal, **Then** the captured file is cleaned up locally, the decision is logged, and the capture loop continues; the queue's policy decision is not duplicated or second-guessed inside capture.
8. **Given** a recording attempt fails partway through (camera adapter error, write error, etc.), **When** the failure is observed, **Then** no partial clip is submitted to delivery, any temporary staging files are removed, the failure is logged with context, and the capture loop remains running and ready for the next motion event.

---

### User Story 3 - Capture-Side Visibility Through Existing Surfaces (Priority: P3)

The owner (or a friend with some technical skill) wants to confirm that the capture half of the station is doing its job. They SSH into the device or read the system journal and answer three questions: "Did the station record anything recently?", "Has the camera or sensor failed?", and "Is the sensor currently healthy?" They never install a companion app, never consult an external dashboard, and the station never emits telemetry to any outside service.

**Why this priority**: Diagnostics are quality-of-life. The station functions for an owner who never logs in — clips just flow into perchpub. But when something does go wrong, the device must be inspectable from the device itself, by the same person who already inspects delivery health.

**Independent Test**: With US1 and US2 working, induce a few capture-side states (idle, recent successful recording, recent camera failure, sensor degraded). Confirm a human reading the existing structured log stream and the existing `perchstation status` output can identify each state within 30 seconds, without any new surface or external service.

**Acceptance Scenarios**:

1. **Given** the station has recorded at least one clip recently, **When** the operator runs the existing `perchstation status` surface, **Then** they see the time of the most recent successful recording and any most-recent capture failure (with reason) alongside the existing delivery and enrollment summaries.
2. **Given** the motion sensor is reporting a degraded or unresponsive state, **When** the operator runs `perchstation status` or reads the JSON log stream, **Then** the degraded state is visible in both surfaces and clearly attributed to the sensor, not to delivery or enrollment.
3. **Given** the station is running, **When** outbound network activity is inspected, **Then** the capture subsystem itself contributes no network traffic — all networking remains delivery's responsibility and the existing privacy posture is preserved.

---

### Edge Cases

- **Power loss mid-recording**: The partially-written clip and its temporary staging file MUST be discarded on the next boot before the capture loop begins accepting motion events. The delivery queue MUST NOT contain a partial sidecar pointing at a half-written media file. At worst, one in-progress clip is lost; nothing else.
- **Stuck-on sensor (signal asserted indefinitely)**: Detected via a configured liveness threshold and surfaced as a degraded state via the existing log + status surfaces; the station does not produce clips while the sensor is in this state.
- **Stuck-off / disconnected sensor**: Detected via the hardware adapter's own error/connectivity signal (e.g., a failed read) and surfaced as a degraded state; the capture loop continues running so it can recover automatically when the sensor returns.
- **Sustained real presence (a bird sits on the feeder for many minutes)**: Bounded clip duration plus cooldown ensures the visit produces a bounded number of clips, not an unbounded series. The owner accepts that some footage of the same visit may not be captured.
- **Disk near or at the configured capture-side ceiling**: The station declines to start a new recording rather than crashing or letting capture-side files balloon to consume all storage; the decision is logged.
- **Delivery queue refuses the clip (queue full)**: The capture subsystem respects the queue's configured policy — it does not retry the submission with its own queueing, does not write the file into the queue directory by hand, and cleans up the local file before returning to idle.
- **Clock skew / NTP correction during a recording**: `captured_at` is best-effort wall-clock time at trigger; the capture loop does not refuse to record because the system clock looks wrong.
- **Camera adapter hangs**: A recording cannot run forever — the configured clip duration is an upper bound the loop enforces from outside the adapter, independent of the adapter's own internal state.
- **Sensor fires during boot or shutdown**: An edge that fires during the boot window — between the hardware adapter opening its kernel-buffered edge FIFO and the capture loop entering its main `select!` — MUST NOT corrupt queue state. Such an edge MAY be observed on the first iteration of the loop after readiness and produce a single bounded clip via the normal trigger path; the staging purge that precedes the loop guarantees no half-state. Producing one clip from a pre-readiness edge is preferable to silently dropping a legitimate sensor event during a window the operator has no control over. An edge that arrives after the shutdown signal has been raised MUST be dropped cleanly: no new staging file, no partial queue entry.
- **Stale capture-side staging files from a previous crash**: On startup the capture loop purges any incomplete staging artefacts before entering its main loop.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST treat the dedicated motion sensor at the feeder as the sole trigger for recording. The capture subsystem MUST NOT initiate recordings based on camera-frame analysis, schedule, weight, or any other signal in this iteration.
- **FR-002**: System MUST keep the camera powered down whenever no recording is in progress.
- **FR-003**: System MUST begin recording at the moment a motion-sensor event is observed, with no pre-roll, no buffered look-back, and no post-roll buffering.
- **FR-004**: System MUST produce exactly one clip per qualifying motion-sensor event, where a "qualifying event" is a fresh quiescent-to-asserted transition observed outside the cooldown window and outside any degraded-sensor state.
- **FR-005**: System MUST bound each recording's duration at a configured maximum; once the bound is reached the recording MUST terminate cleanly regardless of sensor state.
- **FR-006**: System MUST honour a configured cooldown between consecutive recordings, during which further motion-sensor events do NOT start new recordings.
- **FR-007**: System MUST submit each successfully recorded clip to the delivery subsystem via the `Inbox::submit` trait method defined in `crates/perchstation-core/src/queue/inbox.rs`, passing a `ClipMeta` whose `captured_at` reflects the wall-clock time of the trigger. The capture subsystem MUST NOT duplicate, bypass, or contort this interface — for example, it MUST NOT write directly into the queue's `pending/` directory.
- **FR-008**: System MUST NOT submit a partial, truncated-due-to-error, or unverified-write clip to delivery. A recording that did not complete cleanly MUST be discarded locally with its temporary staging files removed.
- **FR-009**: System MUST keep the delivery queue uncorrupted across power loss during a recording. After power is restored the queue contents MUST be the same as if the in-progress recording had never been attempted.
- **FR-010**: System MUST detect a motion-sensor signal that has remained continuously asserted beyond a configured liveness threshold and treat it as a degraded state: no further recordings are produced and the degraded state is surfaced through the existing log and status surfaces. Normal operation MUST resume automatically once the sensor returns to its quiescent state.
- **FR-011**: System MUST detect a motion-sensor adapter that fails to read or reports unavailability and treat it as a degraded state surfaced through the existing log and status surfaces. The capture loop MUST continue running so it can recover automatically once the adapter returns.
- **FR-012**: System MUST run the capture loop inside the `perchstation serve` daemon, supervised alongside the delivery loop. A fault, panic, or non-fatal error in capture MUST NOT terminate delivery, and vice versa.
- **FR-013**: System MUST enforce a configured upper bound on capture-side on-disk footprint (temporary staging files and any intermediate buffers). When the bound is reached the station MUST decline to start new recordings rather than exhaust device storage.
- **FR-014**: System MUST emit capture-side events (recording started, recording completed, recording failed, sensor degraded, sensor recovered, cooldown skip, capture-side disk pressure) into the same structured JSON log stream the delivery subsystem already uses. The capture subsystem MUST NOT introduce a new telemetry channel.
- **FR-015**: `perchstation status` MUST expose capture-side state including at minimum: the time of the most recent successful recording, the time and reason of the most recent capture failure, and the current sensor liveness value. The liveness value MUST distinguish at least four cases: (a) `healthy` — the sensor has been probed and is in normal operating range; (b) `stuck-asserted` — the configured liveness threshold has been crossed; (c) `unavailable` — the adapter is failing reads; and (d) `never-observed` — no liveness probe has been performed in this process (for example when `status` is invoked outside of a running `serve` process). The `never-observed` value is explicitly distinct from `healthy` so that an operator running `status` standalone is not misled into thinking the sensor has been checked when it has not.
- **FR-016**: Hardware-touching code (motion-sensor adapter, camera adapter) MUST live exclusively in the `perchstation-hw` crate. The capture loop in `perchstation-core` MUST depend only on narrow trait abstractions for the sensor and the camera, with fakes available for host tests.
- **FR-017**: System MUST purge incomplete capture-side staging artefacts on startup before the capture loop begins accepting motion events. Stale temporary files MUST NOT accumulate across reboots.
- **FR-018**: System MUST treat the delivery subsystem's queue-full / refused-submission outcome as authoritative: the capture loop logs the decision, removes the local clip file, and continues. The capture subsystem MUST NOT introduce its own queueing, retry, or eviction policy.

### Key Entities

- **Motion Trigger Event**: A single quiescent-to-asserted transition observed from the motion-sensor adapter, with the wall-clock time of the transition. Events observed during cooldown, during a degraded-sensor state, or while a recording is already in progress are dropped rather than queued.
- **Recording**: A single bounded attempt to capture a clip, starting at a trigger event and ending at the configured maximum duration or earlier on a clean stop. Carries a state (in-progress, completed, failed) and, on completion, the path of the resulting clip file plus its `captured_at`.
- **Cooldown State**: The interval after a recording during which further motion-trigger events do not start new recordings, expressed as a deadline relative to the wall clock.
- **Sensor Liveness State**: The capture loop's current runtime view of the motion sensor — healthy (quiescent or briefly asserted within normal bounds), stuck-asserted (continuously asserted beyond the liveness threshold), or unavailable (adapter reports failure). Drives both the decision to record and the value surfaced through the status surface. When projected for the status surface the value also distinguishes never-observed — the case where the snapshot is constructed outside of a running supervisor and no probe has been performed.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A single motion-sensor edge at the feeder produces exactly one clip in the delivery queue with `captured_at` within one second of the trigger.
- **SC-002**: Over any 7-day period of normal operation, the number of clips produced does not exceed the upper bound implied by the configured clip duration and cooldown — i.e., capture cannot generate unbounded clips even under continuous sensor assertion.
- **SC-003**: After power loss during a recording, on the next boot the delivery queue contains zero partial clips and zero orphaned sidecars attributable to the interrupted recording, and the capture loop resumes accepting motion events within 60 seconds of boot.
- **SC-004**: A sensor that has been continuously asserted beyond the configured liveness threshold is reflected as a degraded state in both the JSON log and `perchstation status` within 60 seconds of crossing the threshold; no further clips are produced while the state persists.
- **SC-005**: A sensor adapter that fails (e.g., the underlying device disappears) is reflected as a degraded state in both surfaces within 60 seconds; once the adapter recovers, normal capture resumes without operator intervention.
- **SC-006**: Capture-side on-disk footprint (temporary staging plus any intermediate buffers) remains within its configured ceiling across any 7-day soak under normal conditions, with or without delivery being able to drain the queue.
- **SC-007**: An operator with only local shell access to the device can determine, from `perchstation status` and the JSON log alone and within 30 seconds, when the last clip was captured, whether the most recent capture attempt failed (and why), and whether the sensor is currently healthy.
- **SC-008**: With the motion sensor disconnected (or, in tests, never firing) the capture loop produces no clips, performs no camera I/O, and does not emit log entries at a rate higher than the configured periodic-health cadence of the delivery loop.
- **SC-009**: A fault in capture (panic in the capture task, repeated sensor errors, repeated camera errors) does NOT stop the delivery loop from continuing to drain the existing queue, and vice versa.

## Assumptions

- The motion sensor exposes a discrete digital signal (rising edge / sustained-high) via the `perchstation-hw` adapter; multi-signal trigger fusion, analog thresholding, motion-zone masking, and owner-tunable sensitivity are explicitly out of scope for this iteration.
- The camera adapter in `perchstation-hw` can be started and stopped on demand and produces a complete container-formatted clip file when stopped cleanly. The specific codec, container, resolution, and bitrate are planning-level choices documented in `research.md`, not requirements of this spec; the delivery queue treats the clip as opaque bytes.
- Concrete numeric values for clip duration, cooldown, the stuck-sensor liveness threshold, and the capture-side disk-footprint ceiling are tuning knobs decided during planning. The spec requires that each bound exists and is enforced, not that any particular value is chosen.
- The capture subsystem is the sole producer of clips for the delivery queue in normal operation. No second entry point (sneakernet, manual file drop, companion app) is in scope.
- `captured_at` in `ClipMeta` is best-effort wall-clock time at the trigger. The capture loop does not compensate for arbitrary clock skew and does not refuse to record because the system clock looks wrong; this aligns with the clock-skew posture already taken by the delivery subsystem.
- Audio is not captured. The clip is video-only.
- Pre-roll, post-roll, weight sensing, multi-signal fusion, motion-zone masking, owner-tunable sensitivity, time-of-day or seasonal scheduling, and a companion UI are deferred to follow-up features.
- Certificate renewal and operator install/provisioning UX remain deferred, as noted by feature 001.
- The capture loop and delivery loop share the `perchstation serve` process and supervision tree; their only data-plane interface is the `Inbox` trait. Process isolation, separate daemons, and IPC are out of scope.
- "Sensor liveness" is observed at the trait boundary the capture loop uses; the specific mechanism (polling, edge interrupts, watchdog) is an implementation detail belonging to `perchstation-hw` and planning.
