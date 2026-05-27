---

description: "Task list for Clip Delivery Subsystem (001-clip-delivery)"
---

# Tasks: Clip Delivery Subsystem

**Input**: Design documents from `/specs/001-clip-delivery/`

**Prerequisites**: plan.md (required), spec.md (required), research.md, data-model.md, contracts/{perchpub-api,cli,log-events}.md, quickstart.md

**Tests**: REQUIRED. Constitution Principle V (Test-First) is non-negotiable and the plan / contract documents enumerate explicit test obligations. Integration tests live at the workspace root under `tests/integration/`; contract-drift tests under `tests/contract/`.

**Organization**: Tasks are grouped by user story. Setup + Foundational phases must complete before any user story phase starts. Each user-story phase is an independently testable increment.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Different files, no dependency on an incomplete task — safe to run in parallel.
- **[US1] / [US2] / [US3]**: Maps to user stories in `spec.md` (P1, P2, P3 respectively).
- File paths are exact and relative to the repository root.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Stand up the empty Cargo workspace and shared toolchain configuration so every later task has a place to land. This is a greenfield repository, so the workspace itself is the first deliverable.

- [X] T001 Create the Cargo workspace manifest at `Cargo.toml` (workspace members `crates/perchstation-core`, `crates/perchstation-hw`, `crates/perchstation`; `[workspace.package]` with `edition = "2024"`, `rust-version = "1.95"`, `license = "AGPL-3.0-or-later"`; `[workspace.dependencies]` declaring shared versions of `tokio`, `reqwest`, `rustls`, `rustls-pemfile`, `rcgen`, `rqrr`, `image`, `serde`, `serde_json`, `toml`, `tracing`, `tracing-subscriber`, `clap`, `anyhow`, `thiserror`, `uuid`, `chrono`, `x509-parser`, dev-only `axum`, `assert_cmd`, `tempfile`, `wiremock`)
- [X] T002 [P] Create the `perchstation-core` crate skeleton at `crates/perchstation-core/Cargo.toml` and `crates/perchstation-core/src/lib.rs` (declare modules `config`, `enrollment`, `identity`, `perchpub`, `queue`, `delivery`, `observability`, `hw_traits`; add `#![deny(unsafe_code)]` and `#![deny(clippy::all)]` at crate root; depend on workspace shared deps; `thiserror`-typed public error)
- [X] T003 [P] Create the `perchstation-hw` crate skeleton at `crates/perchstation-hw/Cargo.toml` and `crates/perchstation-hw/src/lib.rs` (target-cfg gate the `camera_qr` module to `cfg(target_os = "linux")`; permit `unsafe` with the constitution's per-block invariant comment policy; depend on `perchstation-core` for trait definitions)
- [X] T004 [P] Create the `perchstation` binary crate skeleton at `crates/perchstation/Cargo.toml`, `crates/perchstation/src/main.rs`, `crates/perchstation/src/cli.rs` placeholder, and stubbed `crates/perchstation/src/commands/{enroll,serve,status}.rs` plus `crates/perchstation/src/bin/fakepub.rs` (all subcommands return `Err(unimplemented!())` until later phases wire them up; `#![deny(unsafe_code)]`)
- [X] T005 [P] Add `rustfmt.toml` and a workspace-level lint configuration in `Cargo.toml` (`[workspace.lints.clippy] all = "deny"`, `[workspace.lints.rust] unsafe_code = "deny"` overridden in `perchstation-hw` only) matching `quickstart.md` §1's `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` gates
- [X] T006 [P] Configure the workspace-root integration test directory: create `tests/integration/support/mod.rs` placeholder, `tests/integration/fixtures/.gitkeep`, and `tests/contract/.gitkeep`; attach the directory to the workspace by adding an `[[test]]` entry in `crates/perchstation/Cargo.toml` (or a dedicated `tests-runner` workspace member) so `cargo test --workspace` discovers files under `tests/integration/*.rs` and `tests/contract/*.rs`
- [X] T007 [P] Add `.gitignore` entries for `target/`, `**/*.rs.bk`, `/tmp/perchstation-*`, and `tests/integration/fixtures/*.tmp`; create an empty `references/` directory marker if missing (the OpenAPI doc lives at `references/openapi.json` from the prior commit)

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Modules and types that every user story consumes. CLI dispatch, config parsing, log formatter, perchpub schema mirrors, hardware traits, and the OpenAPI contract-drift test must all exist before any story-level work begins.

**CRITICAL**: No user story work begins until this phase is green (`cargo build --workspace` + `cargo test --workspace` clean, even though most tests will be stubs).

- [X] T008 Implement `Config` type and TOML loader in `crates/perchstation-core/src/config.rs` per `research.md` R-10 (top-level `perchpub_url`, `data_dir`; `[queue]` block with `max_clips`, `max_bytes`, `eviction` enum; `[retry]` block with `initial_delay_secs`, `max_attempt_delay_secs`, `per_clip_max_attempts`, `per_clip_max_wallclock_hours`; `serde::Deserialize` with field-level defaults; helpful error messages anchored on field names)
- [X] T009 [P] Implement `StationIdentity` type + `load()` in `crates/perchstation-core/src/identity.rs` per `data-model.md` (fields `station_id: Uuid`, `enrolled_at`, `perchpub_url`, `cert_not_after`; `load(data_dir)` reads `identity.json` and parses `cert_not_after` from `station.crt` via `x509-parser`; `save()` stub deferred to T026)
- [X] T010 [P] Mirror perchpub OpenAPI schemas in `crates/perchstation-core/src/perchpub/types.rs` (declare `EnrollmentRequest`, `EnrollmentResponse`, `ClassifyTaskPublic`, `ClassifyTaskStatus`, `UploadPublic`, `ObservationPublic`, `HTTPValidationError`, `ValidationError` as `serde::{Serialize, Deserialize}` types matching `references/openapi.json` v0.1.0 field-for-field per `contracts/perchpub-api.md` §Schemas)
- [X] T011 [P] Define hardware boundary traits in `crates/perchstation-core/src/hw_traits.rs` (`#[async_trait] trait QrFrameSource` returning `image::GrayImage` frames; `trait Clock` returning `chrono::DateTime<Utc>`; doc-comments naming the production and test implementations)
- [X] T012 [P] Implement `SystemClock` in `crates/perchstation-hw/src/clock.rs` (the sole production `Clock` impl, wraps `chrono::Utc::now`)
- [X] T013 [P] Implement structured-log infrastructure in `crates/perchstation-core/src/observability/tracing.rs` (init function selecting JSON formatter by default and a text formatter under `--log-format text`; helper functions / macros for each event code listed in `contracts/log-events.md`; redaction registry stub — fully populated in T059)
- [X] T014 Implement CLI scaffolding in `crates/perchstation/src/cli.rs` (clap-derive root with global flags `--config`, `--log-format`, `--log-level`; subcommands `enroll`, `serve`, `status`; exit-code constants `OK=0`, `USAGE=64`, `CONFIG=70`, `IO=74`, `TRANSIENT=75`, `UNRECOVERABLE=76` matching `contracts/cli.md`)
- [X] T015 Wire CLI dispatch in `crates/perchstation/src/main.rs` (parse args via T014, load config via T008, init tracing via T013, dispatch to `commands::{enroll,serve,status}::run`; commands themselves remain `unimplemented!()` stubs and the binary exits non-zero with a clear message — this is intentional, the stories fill them in)
- [X] T016 [P] Write OpenAPI contract-drift test in `tests/contract/openapi_sync.rs` (load `references/openapi.json`; for each schema listed in `contracts/perchpub-api.md` §Schemas, deserialise the OpenAPI `components/schemas/<Name>` definition and assert field set + type-kind matches the local mirror in `crates/perchstation-core/src/perchpub/types.rs`; test fails loudly with the diff on drift)

**Checkpoint**: workspace builds clean; foundational types compile; contract-drift test passes (or fails meaningfully if `references/openapi.json` and mirrors are out of sync — fix before proceeding).

---

## Phase 3: User Story 1 — First-Time Setup and Happy-Path Delivery (Priority: P1) — MVP

**Goal**: A brand-new station decodes an enrollment QR, completes mTLS provisioning with perchpub, persists its identity, and uploads its first captured clip — observing the resulting classify-task to terminal success — without operator intervention beyond the QR hand-off.

**Independent Test**: Pair a brand-new station (empty `data_dir`) with a fresh perchpub enrollment session against a fake perchpub (axum); hand a single `sample.mp4` to the delivery subsystem via `pending/`; verify the upload completes successfully, perchpub returns a classify-task identifier, the station records it alongside the delivered clip, and the classify-task poll observes `Success` — all without operator intervention beyond running `perchstation enroll` once.

### Test support for User Story 1 (prerequisite for tests in this phase)

- [X] T017 [US1] Build the fake perchpub axum server in `tests/integration/support/fakepub.rs` (per-test `Arc<Mutex<>>`-driven response state; routes `POST /api/v1/enrollment/confirm/{session_id}`, `POST /api/v1/upload/`, `GET /api/v1/classify-task/{id}`; serves over TLS using `tests/integration/fixtures/fakepub.{key,crt}`; records request counts and bodies for assertion)
- [X] T018 [P] [US1] Build the fake `QrFrameSource` in `tests/integration/support/fake_qr.rs` (in-memory `image::GrayImage` constructed from a PNG fixture path; implements `crates/perchstation-core/src/hw_traits.rs::QrFrameSource`)
- [X] T019 [P] [US1] Generate test fixtures at runtime via `tests/integration/support/fixtures.rs` — CA + leaf certs, station keypair, QR PNG, sample mp4 bytes are minted in-process per test rather than checked in (see `tests/integration/fixtures/README.md` for the rationale); `tests/integration/fixtures/` itself stays a `.gitkeep` directory ready for future binary blobs

### Tests for User Story 1 (RED — written before implementation)

- [X] T020 [P] [US1] Integration test `tests/integration/enrollment_happy.rs` (drives `perchstation enroll --qr-source file --qr-file tests/integration/fixtures/enroll-session.png` against the fake perchpub from T017; asserts `identity.json`, `station.crt`, `station.key` mode `0600`, and `ca_chain.pem` are all present and cross-consistent; captures stderr and asserts log events `enrollment.qr_decoded`, `enrollment.csr_generated`, `enrollment.sent`, `enrollment.persisted` fire in order) — covers spec.md US1 acceptance #1
- [X] T021 [P] [US1] Integration test `tests/integration/delivery_happy.rs` (with credentials prepopulated by a helper, drops `sample.mp4` into `pending/`, spawns `perchstation serve` for a bounded duration, asserts the file ends up in `delivered/` with sidecar only, `classify_task_id` is set, the mp4 is unlinked, and log events `delivery.attempt_started`, `delivery.upload_succeeded`, `classify.polled`, `classify.terminal` all fire) — covers spec.md US1 acceptance #2 and #3
- [X] T022 [P] [US1] Integration test `tests/integration/reenroll_conflict.rs` (with credentials prepopulated, invokes `perchstation enroll --qr-source file --qr-file …`; asserts exit code 76, no mutation of credentials, log event `enrollment.refused_overwrite` with `existing_station_id`; repeats with `--force` and asserts overwrite proceeds with a prominent log line) — covers spec.md US1 acceptance #4 and FR-003
- [X] T022a [P] [US1] Integration test `tests/integration/enrollment_session_expired.rs` (fake perchpub returns 422 with an `HTTPValidationError` body on `POST /api/v1/enrollment/confirm/{session_id}`; drives `perchstation enroll --qr-source file --qr-file …` against a brand-new station; asserts exit code 76, that `credentials/` is absent after the run, and that log event `enrollment.session_invalid` fires with `status=422`) — covers spec.md edge case "Enrollment session expires before the station can confirm" and the 4xx branch of `contracts/perchpub-api.md` §1

### Enrollment implementation for User Story 1

- [X] T023 [US1] Implement CSR + Ed25519 keypair generation in `crates/perchstation-core/src/enrollment/csr.rs` (use `rcgen` to mint an Ed25519 keypair and a PKCS#10 CSR; return `(SigningKey, csr_pem: String)`; in-memory key only)
- [X] T024 [US1] Implement QR decoding in `crates/perchstation-core/src/enrollment/mod.rs` (`decode_enrollment_session(image: &GrayImage) -> Result<EnrollmentSessionMaterial>` using `rqrr`; parses the JSON payload to `{session_id: Uuid, auth_token: String}`; ignores `expires_at` if present)
- [X] T025 [US1] Implement the enrollment confirm exchange in `crates/perchstation-core/src/enrollment/confirm.rs` (build a plain-TLS reqwest client with `rustls` configured against the QR-bound CA pin material — note this is the pre-enrollment client, distinct from the post-enrollment mTLS client; POST `/api/v1/enrollment/confirm/{session_id}` with `EnrollmentRequest`; on 200 with `success=true` validate the returned `certificate_pem` chains to `ca_chain_pem` and matches the held private key via SPKI comparison; map 5xx/network to the enrollment-tier retry schedule from `contracts/perchpub-api.md`)
- [X] T026 [US1] Implement atomic identity persistence in `crates/perchstation-core/src/identity.rs::save()` (stage all four files in `credentials.tmp/`; write `station.key` with mode `0600` via `OpenOptions::mode`; serialise `StationIdentity` to `identity.json`; `renameat2` the directory into place; idempotent against partial prior writes) — refuses to clobber an existing `credentials/` unless an explicit `overwrite: true` flag is set, satisfying FR-003
- [X] T027 [US1] Implement `perchstation enroll` command in `crates/perchstation/src/commands/enroll.rs` (refuse if `credentials/identity.json` exists unless `--force`, exiting 76 with `enrollment.refused_overwrite`; acquire a QR frame from the configured `QrFrameSource`; call into T023→T024→T025→T026 in order; emit `enrollment.qr_decoded`, `enrollment.csr_generated`, `enrollment.sent`, `enrollment.persisted` at the appropriate steps; map errors to exit codes 74/75/76 per `contracts/cli.md`)
- [X] T028 [P] [US1] Add file-based `QrFrameSource` implementation in `crates/perchstation-core/src/enrollment/file_source.rs` (loads a PNG/JPEG via the `image` crate, converts to `GrayImage`, returns a single frame; selected via `--qr-source file --qr-file <path>`)
- [X] T029 [P] [US1] Add libcamera-still shell-out adapter in `crates/perchstation-hw/src/camera_qr.rs` (spawns `libcamera-still --immediate --width 800 --height 600 --output <tmp>.jpg`, reads the JPEG, decodes to grayscale, returns the frame; behind `cfg(target_os = "linux")`; selected via `--qr-source camera`)

### Queue and delivery happy path for User Story 1

- [X] T030 [US1] Implement queue store directory layout + atomic state transitions in `crates/perchstation-core/src/queue/store.rs` (create `pending/`, `inflight/`, `delivered/` on first use; `enqueue(clip_path, ClipMeta)` constructs the destination filename stem as `<capture_utc_rfc3339_basic>-<seq>` per `data-model.md` so that lexicographic ordering equals capture order — FR-006 depends on this — then writes the mp4 then the sidecar via tmp + rename; `pick_oldest_pending()` scans `pending/` and returns the lexicographically smallest entry whose `next_attempt_after` has elapsed; `transition_inflight(entry)` and `transition_delivered(entry, outcome)` use tmp + rename; on delivered transitions, unlink the mp4 before the sidecar rename — boot reconciliation is deferred to T048)
- [X] T031 [P] [US1] Implement `ClipQueueEntry` serde type in `crates/perchstation-core/src/queue/mod.rs` (all fields from `data-model.md` §Entity: ClipQueueEntry; `serde::{Serialize, Deserialize}` with `chrono::DateTime<Utc>` for RFC 3339 timestamps; `Outcome` enum nested as an `Option`)
- [X] T032 [P] [US1] Implement `Inbox` trait + default impl in `crates/perchstation-core/src/queue/inbox.rs` (`async fn submit(clip_path, ClipMeta) -> Result<()>` — the capture subsystem's entry point; default impl delegates to `queue::store::enqueue`; eviction policy interception is wired in T047)
- [X] T033 [US1] Implement the mTLS perchpub HTTP client in `crates/perchstation-core/src/perchpub/client.rs` (construct `reqwest::Client` with `rustls-tls`, `RootCertStore` containing only the enrolled CA, `Identity::from_pem` built from station cert+key; refuses every request whose host authority differs from the configured `perchpub_url` authority, emitting an error event without making the network call — implements the SC-007 invariant; exposes methods `upload_clip` and `get_classify_task` per T034/T035)
- [X] T034 [US1] Implement streaming upload in `crates/perchstation-core/src/perchpub/client.rs::upload_clip` (open the clip with `tokio::fs::File`, wrap in `tokio_util::io::ReaderStream`, build a `reqwest::multipart::Part::stream` with content-type `video/mp4`, send as a single-part `multipart/form-data` body to `POST /api/v1/upload/`; return `ClassifyTaskPublic` on 200; classification of non-200 codes is layered in T046)
- [X] T035 [US1] Implement classify-task GET in `crates/perchstation-core/src/perchpub/client.rs::get_classify_task` (typed call to `GET /api/v1/classify-task/{id}`; returns `ClassifyTaskPublic` on 200; non-200 handling layered in T052)
- [X] T036 [US1] Implement the happy-path delivery loop in `crates/perchstation-core/src/delivery/mod.rs` and `crates/perchstation-core/src/delivery/runner.rs` (loop body: pick oldest pending → transition to `inflight/` and emit `delivery.attempt_started` with `attempt` → call `upload_clip` → on 200 atomically update the in-flight sidecar via tmp + rename with `classify_task_id`, `delivered_at`, `outcome = Delivered`, then call `transition_delivered` (which unlinks the mp4 before renaming the sidecar into `delivered/`) and emit `delivery.upload_succeeded`; error-path classification is layered in T046)
- [X] T037 [US1] Implement the classify-task poller in `crates/perchstation-core/src/delivery/classify.rs` (scan `delivered/` for entries whose `last_classify_status` is non-terminal; poll at 30 s cadence per `contracts/perchpub-api.md`; update the sidecar with the latest status and `observation_id`; emit `classify.polled` on non-terminal and `classify.terminal` on `Success`/`Failed`; full transient/terminal classification of poll responses is layered in T052)
- [X] T038 [US1] Implement `perchstation serve` command in `crates/perchstation/src/commands/serve.rs` (load identity, exit 76 with a clear message if missing; spawn the delivery loop (T036) and the classify poller (T037) as tokio tasks; emit `service.ready` with `pending_at_start`; install a SIGTERM handler that drains the in-flight upload for up to 30 s and emits `service.shutdown`)
- [X] T039 [P] [US1] Wire `sd_notify(READY=1)` in `crates/perchstation/src/commands/serve.rs` using the `sd-notify` crate, fired immediately after the `service.ready` log event so systemd `Type=notify` observes a truthful resume timestamp (SC-003)

**Checkpoint — MVP**: Tests T020/T021/T022 are green. Quickstart §1–§5 runs end-to-end on a dev host (no retry edge cases, no eviction, no operator status surface yet).

---

## Phase 4: User Story 2 — Robust Delivery Under Unreliable Conditions (Priority: P2)

**Goal**: The station survives intermittent perchpub unavailability with bounded retry, applies a configured eviction policy when the queue is full, marks permanently failed clips undeliverable without blocking the queue, and recovers cleanly from mid-upload crashes.

**Independent Test**: With the station enrolled (US1), take perchpub offline and verify clips queue without unbounded retry traffic and without loss; restore perchpub and verify every queued clip uploads oldest-first with no operator action; fill the queue to its configured bound and observe the documented eviction; return 422 once and observe one clip mark-undeliverable while others continue; kill `serve` mid-upload and observe boot reconciliation on next start.

### Tests for User Story 2 (RED)

- [ ] T040 [P] [US2] Integration test `tests/integration/outage_recovery.rs` (fake perchpub returns 503 for the first N requests then 200; inject a test `Clock` to verify exponential backoff sleep durations stay within the documented schedule from `research.md` R-7; assert clips upload oldest-first when perchpub recovers; assert no clip is lost; assert `delivery.upload_transient` events fire with correct `next_attempt_after`) — covers spec.md US2 acceptance #1 and #2
- [ ] T041 [P] [US2] Integration test `tests/integration/queue_eviction.rs` (fill queue to `max_clips` and submit one more clip; assert oldest entry in `delivered/` with `outcome: Undeliverable` is dropped first, then oldest in `pending/` if needed, with `queue.evicted` carrying `clip_id`, `reason`, `policy`, `remaining_clips`, `remaining_bytes`; switch policy to `refuse_new` and assert `Inbox::submit` returns the documented error and no eviction occurs) — covers spec.md US2 acceptance #3
- [ ] T042 [P] [US2] Integration test `tests/integration/permanent_failure.rs` (fake perchpub returns 422 for one specific clip and 200 for the rest; assert that one clip transitions to `delivered/` with `outcome: Undeliverable`, `delivery.upload_terminal` fires with `status=422`, and remaining clips upload normally; cap test wall-clock at 30 s) — covers spec.md US2 acceptance #4 and FR-008
- [ ] T043 [P] [US2] Integration test `tests/integration/crash_recovery.rs` (start `serve`, drop a clip, simulate a crash by leaving a file pair in `inflight/` and starting a fresh process; assert boot reconciliation re-queues the clip with `queue.recovered_inflight`; clip ultimately delivers; assert no orphan `.mp4` remains in `inflight/`) — covers spec.md US2 acceptance #5
- [ ] T044 [P] [US2] Unit test in `crates/perchstation-core/src/delivery/retry.rs::tests` (drive the backoff scheduler against a fake `Clock`; assert exponential growth, jitter stays inside ±20 %, attempt ceiling is respected, wall-clock ceiling triggers `attempts_exhausted`)
- [ ] T044a [P] [US2] Integration test `tests/integration/clock_skew_tolerance.rs` (inject a `Clock` whose `now()` returns a value far in the past then far in the future; drop a sample clip into `pending/`; spawn `perchstation serve` briefly; assert delivery still completes successfully and the recorded `delivered_at` reflects the injected clock without the loop aborting or refusing to upload) — covers spec.md edge case "Clock skew or NTP unavailable"

### Implementation for User Story 2

- [ ] T045 [US2] Implement retry/backoff scheduler in `crates/perchstation-core/src/delivery/retry.rs` (exponential backoff with ±20 % jitter; configurable `initial_delay`, `max_attempt_delay`, multiplier 2.0, per-clip attempt ceiling, wall-clock budget; consumes a `Clock` for testability; pure functions returning the next `next_attempt_after`)
- [ ] T046 [US2] Extend `crates/perchstation-core/src/delivery/runner.rs` to classify upload responses per the `contracts/perchpub-api.md` retry table (transient: 408/425/429/500/502/503/504 + network errors + 200-with-malformed-body → schedule via T045 and emit `delivery.upload_transient`; terminal: 4xx other than the transient list → emit `delivery.upload_terminal`, transition the entry to `delivered/` with `outcome: Undeliverable`)
- [ ] T047 [US2] Implement queue policy in `crates/perchstation-core/src/queue/policy.rs` (enforce `max_clips` and `max_bytes`; apply configured eviction `drop_oldest_undelivered` — delete entries from `delivered/` whose `outcome == Undeliverable` first, oldest by `captured_at`, until under bound; alternate `refuse_new` returns an `InboxError::QueueFull`; emit `queue.evicted` with the full set of required fields from `contracts/log-events.md`)
- [ ] T048 [US2] Implement boot reconciliation in `crates/perchstation-core/src/queue/store.rs::reconcile()` (enumerate `inflight/`, rename each mp4+sidecar pair back to `pending/`, emit `queue.recovered_inflight` per entry; reset `next_attempt_after` to `None`; called once from `commands::serve` immediately before `service.ready`)
- [ ] T049 [US2] Implement zero-length / unreadable pre-flight check in `crates/perchstation-core/src/delivery/runner.rs` (stat the clip file before the upload attempt; on failure, emit `queue.zero_length_skipped`, transition the entry to `delivered/` with `outcome: Undeliverable` and a recorded `last_error.kind = "zero_length"` or `"unreadable"`; never reaches the wire) — covers FR-013
- [ ] T049a [P] [US2] Implement ENOSPC handling across queue writes in `crates/perchstation-core/src/queue/store.rs` and `crates/perchstation-core/src/delivery/runner.rs` (surface `io::ErrorKind::StorageFull` from sidecar and clip writes as a typed `QueueError::DiskFull`; on detection emit `queue.disk_full` with current pending/inflight/delivered byte totals, do NOT enter a tight retry loop, sleep for the retry-tier backoff window, and re-attempt on the next loop iteration; if disk remains full beyond the per-clip wall-clock budget, transition the affected entry to `delivered/` with `outcome: Undeliverable` and `last_error.kind = "disk_full"`; add `queue.disk_full` to `contracts/log-events.md`) — covers spec.md edge case "Disk full or near-full" when the global filesystem fills outside `queue.max_bytes`
- [ ] T050 [P] [US2] Honour `Retry-After` on 429 responses in `crates/perchstation-core/src/perchpub/client.rs` (parse the header and surface it to T046, which uses it as a floor for `next_attempt_after`)
- [ ] T051 [P] [US2] Implement attempts-exhausted handling in `crates/perchstation-core/src/delivery/runner.rs` (when T045 reports the per-clip attempt count or wall-clock budget exhausted, emit `delivery.attempts_exhausted` with `attempts` + `wallclock_secs`, transition to `delivered/` with `outcome: Undeliverable`)
- [ ] T052 [US2] Implement classify-task poll classification in `crates/perchstation-core/src/delivery/classify.rs` per the poll table in `contracts/perchpub-api.md` (transient 5xx/network → backoff via T045 with poll-tier ceilings; terminal 404/422 → emit `classify.lost`, stop polling that entry but leave `outcome: Delivered` intact — the clip was uploaded successfully, only its post-upload disposition is lost)

**Checkpoint**: Tests T040–T044 pass. The station now survives outages, queue saturation, terminal failures, and crashes. Together with US1, this is the constitution's "unattended reliability" promise discharged.

---

## Phase 5: User Story 3 — Operator Visibility Into Delivery State (Priority: P3)

**Goal**: An operator with only local shell access can determine delivery health in under 30 s, the station emits no outbound traffic beyond the configured perchpub endpoint, and no log line carries enrollment or key material.

**Independent Test**: With US1 + US2 working, induce idle / queue-building / recent-failure / recent-recovery states and confirm a human reading `perchstation status` or the journal identifies each state inside 30 s; verify a 5-minute simulated workload produces zero connection attempts to any host other than perchpub from the station's PID; inject known-secret values into enrollment and delivery and assert no log line contains them.

### Tests for User Story 3 (RED)

- [ ] T053 [P] [US3] Integration test `tests/integration/status_surface.rs` (prepopulate four data dirs reflecting idle / queue-building / recent-failure / recent-recovery; run `perchstation status` and `perchstation status --json`; assert text output matches the example in `contracts/cli.md` §status default output; assert JSON matches the schema in `contracts/cli.md`; assert `enrollment.state` reflects `missing`/`ok`/`expired` transitions including a fixture where `cert_not_after` is in the past) — covers spec.md US3 acceptance #1 and FR-014's status surfacing
- [ ] T054 [P] [US3] Integration test `tests/integration/outbound_allowlist.rs` (run `perchstation serve` under a userspace network namespace with `iptables`-style counting via a transparent local proxy that logs every connection attempt; drive a 5-minute simulated workload via accelerated `Clock`; assert zero connection attempts from the station PID to any host other than the fake perchpub authority; allow loopback DNS and `systemd-timesyncd` from other PIDs) — covers spec.md US3 acceptance #3 and SC-007
- [ ] T055 [P] [US3] Integration test `tests/integration/log_redaction.rs` (inject distinctive marker strings into the enrollment `auth_token`, the generated CSR PEM, and the station private key; run full enrollment + delivery against fake perchpub; capture all stderr lines AND every HTTP request body recorded by the fake perchpub; assert no stderr line contains any marker — even under `--log-level trace`; assert the private-key marker appears in zero captured request bodies — the `auth_token` and `csr_pem` markers are expected only inside the `/enrollment/confirm` request body, the private-key marker must appear nowhere on the wire) — covers `contracts/log-events.md` §Field discipline and FR-001's "private key MUST never leave the device"

### Implementation for User Story 3

- [ ] T056 [US3] Implement the status snapshot in `crates/perchstation-core/src/observability/status.rs` (read `credentials/identity.json` to compute `enrollment.state` — `missing` if absent, `expired` if `cert_not_after < now`, `ok` otherwise; enumerate `pending/`, `inflight/`, `delivered/` for counts and on-disk byte totals; project `last_success` from the most recent `delivered/` sidecar with `outcome: Delivered`; project `last_failure` from the most recent sidecar with a non-empty `last_error` or `outcome: Undeliverable`; gather the three most recent deliveries; pure read — never mutates `data_dir`)
- [ ] T057 [US3] Implement `perchstation status` command in `crates/perchstation/src/commands/status.rs` (call `observability::status::snapshot`; render the human-readable text format from `contracts/cli.md` by default; render the JSON object under `--json`; exit 0 always except on filesystem error → exit 74; safe to run concurrently with `serve`)
- [ ] T058 [US3] Implement cert-expired pre-flight check in `crates/perchstation-core/src/delivery/runner.rs` (before each upload attempt, compare `StationIdentity.cert_not_after` to the injected `Clock`; on expiry, emit `delivery.cert_expired` with `cert_not_after`, halt the delivery loop without exiting the process so `status` continues to report `expired`; the operator must re-enroll to resume) — covers FR-014
- [ ] T059 [P] [US3] Realise the secret-redaction layer in `crates/perchstation-core/src/observability/tracing.rs::redact` (registry installed at process start with `auth_token`, `csr_pem`, and the station private-key PEM body once each is materialised; a `tracing` `Layer` that scans every event's field values and rejects fields containing any registered marker; covers every event code in `contracts/log-events.md` and works under `RUST_LOG=trace`) — builds on T013's stub
- [ ] T060 [P] [US3] Verify the outbound URL allowlist gate in `crates/perchstation-core/src/perchpub/client.rs` matches what `outbound_allowlist.rs` (T054) asserts (pre-flight check on every request: the parsed URL's host authority must equal the configured `perchpub_url` authority; on mismatch return an error before the connection is opened; emit an error log event with the offending URL) — strengthens the T033 gate to a test-verified invariant per `research.md` R-12

**Checkpoint**: Tests T053–T055 pass. All three user stories are independently functional. Spec success criteria SC-006 and SC-007 are testable invariants.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Production deployment artefacts, cross-build, CI, and the end-to-end quickstart validation.

- [ ] T061 [P] Implement the `fakepub` dev binary in `crates/perchstation/src/bin/fakepub.rs` (thin CLI over the `tests/integration/support/fakepub.rs` module from T017; flags `--listen`, `--tls-key`, `--tls-cert`, `--ca`; used by `quickstart.md` §2 — note this is a dev-only entry point and is excluded from release artefacts)
- [ ] T062 [P] Add the systemd unit at `deploy/systemd/perchstation.service` per `quickstart.md` §7 (`Type=notify`, `Restart=always` with restart-rate-limit, `Environment="RUST_LOG=info"`, `ExecStart=/usr/local/bin/perchstation --config /etc/perchstation/config.toml serve`, `User=perchstation`, `Group=perchstation`, `StateDirectory=perchstation`)
- [ ] T063 [P] Add the example operator config at `deploy/config.example.toml` (every field from `research.md` R-10 with commented defaults and a one-line explanation per field)
- [ ] T064 [P] Add aarch64 cross-build configuration at `.cargo/config.toml` (`target.aarch64-unknown-linux-gnu` linker hints for `cargo zigbuild`; documented in a comment block referencing `quickstart.md` §6)
- [ ] T065 [P] Add CI workflow at `.github/workflows/ci.yml` (`cargo fmt --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`, conditional `cargo zigbuild --target aarch64-unknown-linux-gnu` on pushes to `main`; runs on Linux + macOS for the host triple)
- [ ] T066 Run the full quickstart end-to-end as documented in `specs/001-clip-delivery/quickstart.md` §1–§5 on a dev host (record the actual output; reconcile any drift between the doc and the implementation by amending the doc or filing follow-up tasks; do not edit the doc to paper over real bugs)
- [ ] T067 [P] Document the on-device release smoke test in `deploy/RELEASE-CHECKLIST.md` (real camera QR capture, real perchpub interop against staging, journald log inspection, 7-day soak referenced by SC-005 — the items quickstart §"What this quickstart does not prove" defers to a hardware presence)
- [ ] T068 [P] Refresh `CLAUDE.md` so its SPECKIT marker points at `specs/001-clip-delivery/plan.md` and any new top-level entry points (e.g., `cargo run -p perchstation -- ...`) are reflected
- [ ] T069 Final cross-cutting sweep: run `cargo test --workspace` clean; run `cargo clippy --all-targets --workspace -- -D warnings` clean; grep `crates/perchstation-core/` for `unsafe` and confirm zero matches; confirm `license = "AGPL-3.0-or-later"` is set at the workspace level and inherited by every crate; confirm the OpenAPI contract test (T016) still passes against the current mirrors

---

## Dependencies & Execution Order

### Phase dependencies

- **Setup (Phase 1)**: No prerequisites — can start immediately.
- **Foundational (Phase 2)**: Depends on Setup. **Blocks every user-story phase.**
- **User Stories (Phases 3, 4, 5)**: All depend on Foundational completion.
  - US1 (P1) is the MVP and is implemented first.
  - US2 (P2) and US3 (P3) both depend on US1's delivery loop, mTLS client, queue store, and `serve` skeleton being in place. They can run in parallel after US1 lands.
- **Polish (Phase 6)**: Depends on all user-story phases being complete.

### User-story dependencies (this feature specifically)

- **US1 → US2**: US2 extends the same `delivery::loop`, `queue::store`, `delivery::classify`, and `perchpub::client` modules that US1 establishes. US2 cannot precede US1 in any meaningful sense.
- **US1 → US3**: US3's `status` snapshot reads the queue layout US1 defines and the `StationIdentity` US1 persists; the `delivery.cert_expired` event US3 adds (T058) hooks into US1's loop; the outbound-allowlist invariant US3 verifies (T054, T060) tests behaviour US1's `perchpub::client` (T033) already enforces.
- **US2 ⟂ US3** (independent after US1): The two stories touch largely disjoint code paths and can be developed in parallel by two engineers once US1 is green.

### Within each user story

- Tests are written first (RED) and **MUST fail meaningfully** before implementation — TDD per Constitution Principle V.
- Within a phase, follow: test support → tests → models / types → core services → endpoints / loop → CLI wiring.
- Each story is complete only when its integration tests are green.

### Parallel opportunities (highlights)

- All Setup tasks marked **[P]** (T002, T003, T004, T005, T006, T007) can run in parallel — different files, no cross-deps; T001 (workspace Cargo.toml) must land first because each crate skeleton inherits from it.
- All Foundational tasks marked **[P]** (T009, T010, T011, T012, T013, T016) can run in parallel after T001/T008; T014 + T015 are sequential because T015 imports T014.
- Within US1: T017, T018, T019 (test support) are largely parallel; T020, T021, T022 (RED tests) can be drafted in parallel; T023, T024, T028, T029, T031, T032 are independent files. T025 depends on T023+T024; T036 depends on T030+T033+T034.
- Within US2: T040–T044 (all RED tests + the retry unit test) can be drafted in parallel; T050, T051 are independent files relative to T046; T045 must precede T046.
- Within US3: T053, T054, T055 are independent test files; T059, T060 are independent files relative to T056/T057/T058.

---

## Parallel Example: User Story 1 RED Tests

```bash
# Once T017, T018, T019 are in place, these three tests can be written in parallel:
Task: "Write enrollment_happy.rs in tests/integration/"
Task: "Write delivery_happy.rs in tests/integration/"
Task: "Write reenroll_conflict.rs in tests/integration/"

# And the independent-file implementation tasks within US1:
Task: "Implement CSR generation in crates/perchstation-core/src/enrollment/csr.rs"
Task: "Implement QR decoding in crates/perchstation-core/src/enrollment/mod.rs"
Task: "Implement file-based QrFrameSource in crates/perchstation-core/src/enrollment/file_source.rs"
Task: "Implement libcamera-still shell-out in crates/perchstation-hw/src/camera_qr.rs"
Task: "Implement ClipQueueEntry serde type in crates/perchstation-core/src/queue/mod.rs"
Task: "Implement Inbox trait in crates/perchstation-core/src/queue/inbox.rs"
```

---

## Implementation Strategy

### MVP first (User Story 1 only)

1. Complete Phase 1: Setup.
2. Complete Phase 2: Foundational — workspace compiles, all stubs in place, T016 contract-drift test passes.
3. Complete Phase 3: User Story 1 — enrollment + happy-path delivery + classify polling end-to-end.
4. **Stop and validate**: T020 / T021 / T022 green; quickstart §1–§5 runs on a dev host.
5. The MVP is shippable: a station can be enrolled and deliver clips under happy-path conditions.

### Incremental delivery

1. Setup + Foundational → workspace ready.
2. Add US1 → demo the MVP (enrol + first upload).
3. Add US2 → demo unattended-reliability scenarios (outage, eviction, terminal failure, crash).
4. Add US3 → demo operator visibility (`status`, allowlist invariant, log redaction).
5. Polish → CI, cross-build, systemd, release docs.

Each story adds standalone value without breaking previous stories.

### Parallel team strategy

With two engineers after Phase 2:

- **Engineer A**: US1 → US2.
- **Engineer B**: blocked until US1 lands (US3 needs US1's queue + identity + client), then US3 in parallel with US2.

With three engineers after US1:

- **A**: US2.
- **B**: US3 status + allowlist (T053, T054, T056, T057, T058, T060).
- **C**: US3 log redaction + polish prep (T055, T059, T061, T062, T063).

---

## Notes

- **[P] tasks** = different files, no dependency on an incomplete task in this list.
- **[Story] label** ties a task to a spec.md user story for traceability; setup, foundational, and polish phases carry no story label.
- **Tests must fail before implementation** — verify each RED test fails with a meaningful message (not a compile error) before writing the corresponding implementation.
- **Commit cadence**: one commit per task or per logical group; the `speckit.git.commit` post-hook will offer to commit after this command.
- **Stop at any checkpoint** to validate the story independently; do not stack unfinished stories.
- **Avoid**: vague tasks ("improve delivery"), same-file conflicts inside a [P] block, cross-story dependencies that would prevent US2/US3 from being demoed without each other.
