# Feature Specification: perchstation Clip Delivery Subsystem

**Feature Branch**: `001-clip-delivery`

**Created**: 2026-05-26

**Status**: Draft

**Input**: User description: "Design the perchstation delivery subsystem that uploads bird-feeder clips to perchpub. The perchpub HTTP API is described in references/openapi.json — treat it as the source of truth for endpoint shape, auth, and response schemas."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - First-Time Setup and Happy-Path Delivery (Priority: P1)

A hobbyist mounts a brand-new perchstation on their bird feeder and powers it on for the first time. They open the perchpub UI on their phone or laptop and initiate an enrollment session for the new device, then hand the session credentials to the station. Once enrolled, the next clip the feeder records arrives in perchpub without further intervention, and the owner can see it appear in their station's clip list.

**Why this priority**: This is the minimum viable product. Until enrollment and a single happy-path upload work end-to-end, the device delivers no value at all. Enrollment is a hard prerequisite for upload, so they are bundled as one P1 journey.

**Independent Test**: Pair a brand-new station with a fresh perchpub enrollment session, hand a single test clip to the delivery subsystem, and verify the upload completes successfully and perchpub returns a classify-task identifier — all without operator intervention beyond the initial enrollment hand-off.

**Acceptance Scenarios**:

1. **Given** a brand-new station with no credentials and a perchpub enrollment session that has not yet expired, **When** the operator presents the perchpub-issued enrollment QR code to the station's camera, **Then** the station decodes the session credentials, generates its own keypair on-device, completes the enrollment exchange with perchpub, persists the issued certificate and CA chain durably, records the station identifier, and reports enrollment success.
2. **Given** an enrolled station with network connectivity and a captured clip waiting for delivery, **When** the delivery subsystem runs, **Then** the clip is uploaded to perchpub, perchpub returns a classify-task identifier, and the station records that identifier alongside the delivered clip.
3. **Given** an upload has completed, **When** the station inspects the corresponding classify task, **Then** it can determine that perchpub has at least received the clip (`Prepared`, `Queued`, `Processing`, or `Success`) and can observe terminal `Success` or `Failed` outcomes when they occur.
4. **Given** a station already holds enrollment credentials, **When** enrollment is attempted again, **Then** the station does not silently overwrite the existing identity and surfaces the conflict to the operator.

---

### User Story 2 - Robust Delivery Under Unreliable Conditions (Priority: P2)

The owner's home Wi-Fi or perchpub itself is intermittently unreachable — storms knock out the internet, the router reboots, perchpub gets a maintenance window. The station keeps capturing clips and reliably delivers them once connectivity returns, without the owner having to intervene.

**Why this priority**: Critical to the unattended-reliability promise, but only meaningful once the happy path of US1 works. Without this story, the device functions in lab conditions but fails the first time it goes outdoors.

**Independent Test**: With the station enrolled and a queue of clips on disk, take the network or perchpub offline. Verify clips queue without unbounded retry traffic and without loss. Restore connectivity and verify every queued clip uploads, oldest first, with no operator action.

**Acceptance Scenarios**:

1. **Given** perchpub is unreachable, **When** clips arrive at the delivery subsystem, **Then** they are persisted on the local queue and not lost, the station does not enter a hot retry loop, and retry traffic stays within bounded backoff.
2. **Given** perchpub becomes reachable after an outage, **When** the delivery subsystem next runs, **Then** all queued clips are uploaded in capture order (oldest first), without any operator action.
3. **Given** the local queue is at its configured bound, **When** a new clip arrives, **Then** the station applies an explicit, configured eviction-or-backpressure policy and logs the decision with enough context to diagnose later.
4. **Given** perchpub returns a permanent client-side error (for example, a 422 validation failure) for a specific clip, **When** the response is received, **Then** the station logs the failure with context, marks that clip undeliverable, and stops retrying it while continuing with the rest of the queue.
5. **Given** power is lost during an in-progress upload, **When** the device boots back up, **Then** the affected clip is either re-attempted or recognised as already-accepted by the server, with no permanent loss of captured footage and no corruption of local delivery state.

---

### User Story 3 - Operator Visibility Into Delivery State (Priority: P3)

The owner (or a friend with some technical skill) can SSH into the device or read the system journal and see at a glance: is the station healthy, how many clips are queued, when was the last successful upload, what was the most recent failure? They never need to install a companion app or consult an external dashboard to answer these questions.

**Why this priority**: Diagnostics are quality-of-life. The system functions for an owner who never logs in — clips just flow. But when something does go wrong, the device must be inspectable from the device itself, because the constitution forbids a telemetry channel.

**Independent Test**: With US1 and US2 working, induce a few interesting states (idle, queue building, recent failure, recent recovery). Confirm that a human reading the local logs or CLI surface can identify each state within 30 seconds without leaving the device.

**Acceptance Scenarios**:

1. **Given** the station is operating normally, **When** the operator inspects the local log or CLI surface, **Then** they can see queue depth, last successful upload time, last failed upload context, and current enrollment status.
2. **Given** the classify task for a delivered clip transitions to a terminal state, **When** the station observes that transition, **Then** the outcome is reflected in the local log alongside the original delivery record.
3. **Given** the station is running, **When** outbound network activity is inspected, **Then** the device makes no calls beyond those required for enrollment, upload, and classify-task polling against the configured perchpub endpoint (plus the system-configured time and name-resolution services it relies on).

---

### Edge Cases

- **Power loss mid-upload**: Partial uploads must not corrupt local delivery state; on reboot the station retries or recognises an already-accepted upload without losing the clip.
- **Disk full or near-full**: When local storage approaches its limit, the configured queue policy (eviction or backpressure) applies before the device crashes or silently overwrites.
- **Perchpub returns 5xx for an extended period**: Backoff is bounded; the station never enters a tight retry loop.
- **Perchpub rejects clip as malformed (422)**: The station logs and abandons that clip rather than retrying forever.
- **Zero-length or unreadable clip on disk**: Detected before upload, logged, and skipped without crashing the delivery loop.
- **Clock skew or NTP unavailable**: Delivery continues; timestamps recorded by the station are treated as best-effort and the station does not refuse to upload purely because its clock looks wrong.
- **Enrollment certificate expires**: Delivery stops; the condition is logged prominently and the operator must re-enrol the station. A renewal flow is out of scope for this iteration.
- **Duplicate upload after retry**: A clip the server already accepted is uploaded again because the station's prior response was lost in transit; the resulting state is deterministic and the station does not corrupt its records.
- **Enrollment session expires before the station can confirm**: The station reports the failure and leaves local state uncorrupted; the operator can initiate a new session and try again.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST enrol an unprovisioned station against a perchpub-issued enrollment session, generating a fresh keypair on-device; the station's private key MUST never leave the device.
- **FR-002**: System MUST persist the issued certificate, CA chain, and station identifier such that they survive reboot, power loss, and process crash.
- **FR-003**: System MUST refuse to silently overwrite an existing station identity; re-enrollment MUST be an explicit, logged operator action.
- **FR-004**: System MUST accept captured clips for delivery via a bounded, on-disk queue that survives power loss.
- **FR-005**: System MUST upload queued clips to perchpub via the documented upload endpoint and MUST record the classify-task identifier returned by perchpub for each successful upload, associating it with the originating clip.
- **FR-006**: System MUST upload queued clips in capture order (oldest first) under normal conditions.
- **FR-007**: System MUST treat network failures, request timeouts, and perchpub 5xx responses as transient and retry with bounded exponential backoff.
- **FR-008**: System MUST treat permanent client-side responses (4xx other than recognised transient codes) as terminal for the affected clip: log with context, mark the clip undeliverable, and proceed with other queued clips.
- **FR-009**: System MUST enforce a configurable upper bound on the local queue (expressed in clip count and/or on-disk size) and apply an explicit, configured eviction-or-backpressure policy when the bound is reached.
- **FR-010**: System MUST be observable via structured local logs covering at least queue depth, last-success timestamp, last-failure context, and enrollment status, without emitting any telemetry beyond the perchpub endpoints documented in the API.
- **FR-011**: System MUST be able to query the disposition of a previously delivered clip via the classify-task identifier and reflect the result in the local log.
- **FR-012**: System MUST tolerate duplicate uploads without corrupting local state when a retry races with a prior already-accepted upload.
- **FR-013**: System MUST detect zero-length, unreadable, or otherwise malformed clip files before attempting upload, log them, and skip them.
- **FR-014**: System MUST stop attempting uploads when its enrollment credentials are expired or otherwise invalid, surface the condition prominently in the local log, and await operator-driven re-enrollment.
- **FR-015**: System MUST cap retry attempts and total retry traffic per clip such that, even under prolonged perchpub unavailability, the station's storage writes and outbound bandwidth remain bounded.
- **FR-016**: System MUST accept the enrollment session credentials (`session_id`, `auth_token`) from the operator by reading a QR code displayed in the perchpub UI through the station's own camera; no separate companion app, embedded web UI, or manual-entry channel is required for first-time enrollment.
- **FR-017**: System MUST present its enrollment-issued certificate as a TLS client certificate (mTLS) on every authenticated call to perchpub, including `POST /api/v1/upload/` and `GET /api/v1/classify-task/{id}`. The certificate, its on-device private key, and the CA chain returned by enrollment together constitute the station's persistent authentication material; perchpub authenticates the station server-side from the presented certificate and does not require the station to send any additional auth header.

### Key Entities

- **Station Identity**: The persistent identity of this perchstation — a station identifier issued by perchpub, the certificate and CA chain returned by enrollment, and the station's private key (generated on-device and never transmitted).
- **Clip Queue Entry**: A captured clip awaiting or undergoing delivery, with associated metadata: clip handle, capture time, queue-arrival time, retry count, most recent error, and — once upload succeeds — the perchpub classify-task identifier.
- **Delivery Outcome**: The current or terminal state of a delivered clip, reflecting perchpub's classify-task status (`Prepared`, `Queued`, `Processing`, `Success`, `Failed`) and, on `Success`, the associated observation reference.
- **Enrollment Session Material**: Transient secrets supplied by the operator (`session_id`, `auth_token`) used exactly once to complete enrollment and discarded from station memory immediately afterwards.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Under normal home network conditions, 99% of clips captured by an enrolled station reach perchpub within 5 minutes of capture, measured over any rolling 30-day window.
- **SC-002**: For any 7-day window in which the station remains powered and connectivity is restored at least once, 100% of clips captured during the window (and within the configured queue bound) reach perchpub by the end of the window.
- **SC-003**: After power is restored following a power loss, the station resumes delivery without manual intervention within 60 seconds.
- **SC-004**: A fresh station completes first-time enrollment, end to end, in under 3 minutes of operator interaction, measured from the moment the operator initiates an enrollment session in perchpub.
- **SC-005**: Over a 7-day soak test under normal home conditions, the device's local storage writes and outbound network traffic both remain within their configured ceilings; no runaway retry pattern or unbounded queue growth is observed.
- **SC-006**: An operator with only local shell access to the device can determine current delivery health (queue depth, last success, last failure cause, enrollment status) in under 30 seconds without a companion app or telemetry.
- **SC-007**: The station emits no network traffic to any host other than the configured perchpub deployment and the system-configured time and name-resolution services it depends on.

## Assumptions

- The station's camera, used during normal capture, doubles as the input channel for enrollment QR codes; no additional input device, screen, or embedded UI is required on the station.
- The capture/recording subsystem produces complete clip files on disk before invoking the delivery subsystem. Clip integrity beyond detecting zero-length or unreadable files is the capture subsystem's responsibility, not delivery's.
- Classification and observation creation are perchpub's responsibility; the station's job ends once perchpub acknowledges the upload and returns a classify-task identifier. The station does not POST observations directly.
- Perchpub is the system of record for stored clips. Local clip storage is a buffer, not an archive — clips may be removed locally once delivery is confirmed, or evicted earlier under the configured queue policy.
- The station's geographic location is established during the perchpub-side enrollment session and persists server-side bound to the station identity. The station itself does not stamp clips with coordinates.
- The host operating system keeps the system clock reasonably accurate (for example, via NTP). The station may continue best-effort delivery during clock drift and is not required to compensate for arbitrary clock skew.
- Certificate renewal is out of scope for this iteration. When the enrollment certificate is invalid or expired, the station ceases uploads and the operator must re-enrol. A renewal flow is expected to be added in a later iteration.
- Network is the only delivery channel. Sneakernet, USB exports, and other out-of-band transports are explicitly out of scope.
- Each station communicates with a single perchpub deployment, fixed at enrollment time. Multi-deployment failover is out of scope.
- Each station is independent. Station-to-station coordination is out of scope.
- Specific numeric defaults for queue size, retry ceilings, and log-rotation cadence are tuned during planning; this specification requires that the bounds exist and are enforced, not that they take any particular value.
