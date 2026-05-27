# Implementation Plan: Clip Delivery Subsystem

**Branch**: `001-clip-delivery` | **Date**: 2026-05-27 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/001-clip-delivery/spec.md`

## Summary

The delivery subsystem is the perchstation's network-facing half: it owns
station enrollment (a one-shot QR-driven mTLS provisioning exchange with
perchpub), the on-disk queue of captured clips waiting to upload, the
upload loop itself, and the operator's view into all of the above. Because
this is the first feature in a greenfield repository, the plan also
establishes the project skeleton: a Cargo workspace with three crates —
`perchstation-core` (platform-agnostic delivery, enrollment, queue,
perchpub client, observability), `perchstation-hw` (the only place
hardware lives, cfg-gated), and `perchstation` (the binary, with `clap`
subcommands `enroll`, `serve`, `status`). The technical approach is
deliberately small: streaming `reqwest`+`rustls` mTLS calls; a
directory-of-files queue with `rename(2)`-atomic state transitions; an
Ed25519 keypair generated on-device at enrollment whose private half never
leaves; structured-JSON `tracing` logs to journald with an enforced
outbound allowlist; bounded retries with exponential backoff capped in
both attempts and wall-clock. The Phase 0 research has resolved every
candidate `NEEDS CLARIFICATION`; Phase 1 produced a data model, three
contract documents, and a host-runnable quickstart.

## Technical Context

**Language/Version**: Rust, stable toolchain, edition 2024, MSRV 1.95.

**Primary Dependencies**:
- `tokio` 1.x — single project-wide async runtime (constitution-mandated).
- `reqwest` 0.12 (`rustls-tls`, `stream`, `multipart`) — HTTP + mTLS + streaming uploads.
- `rustls` + `rustls-pemfile` — TLS material loading; CA pinning to the enrollment-issued chain only.
- `rcgen` — on-device Ed25519 keypair + PKCS#10 CSR generation at enrollment time.
- `rqrr` — QR decoding from grayscale frames (consumes images; platform-agnostic).
- `serde` + `serde_json` + `toml` — config and on-disk metadata.
- `tracing` + `tracing-subscriber` (JSON formatter) — structured logs to stderr/journald.
- `clap` 4 — operator CLI surface.
- `anyhow` (binary) / `thiserror` (library APIs).
- Dev only: `axum` (fake perchpub for integration tests), `assert_cmd`, `tempfile`, `wiremock`.

**Storage**: Local filesystem, default `data_dir = /var/lib/perchstation`. Subtree:
`credentials/` (PEMs + `identity.json`) and `queue/{pending,inflight,delivered}/`
(clip files + JSON sidecars; state encoded by directory). No database. See
[`data-model.md`](data-model.md).

**Testing**: `cargo test --workspace` runs unit tests in every crate plus
host-runnable integration tests under `tests/integration/` that drive the
full delivery loop against a fake perchpub (axum) and a fake
`QrFrameSource` fed by PNG fixtures. A `tests/contract/openapi_sync.rs`
test diffs `references/openapi.json` against the station's mirrored
schemas. On-device behaviour that genuinely needs a Pi (camera-driven QR
capture, real mTLS interop with a deployed perchpub) is covered by a
documented release smoke test, not by mocks.

**Target Platform**: 64-bit Raspberry Pi OS Bookworm on Pi 4 and Pi
Zero 2 W (`aarch64-unknown-linux-gnu`). Cross-compiled from x86_64
Linux/macOS dev hosts via `cargo-zigbuild`. Dev-host tests run on the host
triple.

**Project Type**: Cargo workspace with three crates (`perchstation-core`,
`perchstation-hw`, `perchstation`). See "Project Structure" below.

**Performance Goals** (derived from spec Success Criteria):
- 99 % of captured clips reach perchpub within 5 minutes of capture under
  normal connectivity (SC-001).
- Resume delivery within 60 s of power-on after an outage (SC-003).
- Enrollment complete end-to-end within 3 min of operator action (SC-004).

**Constraints**:
- Memory ceiling: target < 50 MB RSS for `perchstation serve` on a Pi
  Zero 2 W (Principle III).
- Upload body streamed from disk; clips are never fully buffered in RAM.
- Bounded queue: default 500 clips OR 2 GiB total, whichever first; default
  eviction `drop_oldest_undelivered`, alternate `refuse_new`; both logged
  per eviction (FR-009).
- Bounded per-clip retry budget: default 12 attempts and 24 h wall-clock;
  exponential backoff with multiplier 2.0 and ±20 % jitter, initial delay
  10 s, per-attempt delay capped at 3600 s (FR-007, FR-015; full TOML
  schema in `research.md` R-10).
- Outbound destinations restricted to the configured perchpub origin plus
  OS-level DNS / NTP; enforced by CA pinning + URL allowlist; verified by
  integration test (SC-007).
- `unsafe` forbidden in `perchstation-core`; allowed only in
  `perchstation-hw` with per-block invariant comments.
- License: AGPL-3.0-or-later, declared at the workspace level.

**Scale/Scope**: A single station communicates with a single perchpub
deployment, fixed at enrollment. Realistic clip rate: a handful per
minute at peak, low dozens per day typical. Designed for months of
unattended operation between owner interactions.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle / Gate                   | Result | Where it shows up                                                                                                                                                                                              |
| ---------------------------------- | ------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| I. Unattended Reliability          | ✅     | Persistent queue with `rename`-atomic state transitions (FR-004; data-model.md); bounded retry + backoff (FR-007, FR-015; research.md R-7); crash-restart resumption (boot reconciliation: `inflight/` → `pending/`); 4xx is terminal-per-clip not loop-forever (FR-008; contracts/perchpub-api.md); cert-expired stops uploads with a prominent log rather than silent failure (FR-014; log-events.md `delivery.cert_expired`). |
| II. Hardware at the Boundary       | ✅     | Workspace split: `perchstation-core` contains all delivery logic; `perchstation-hw` is the only place that touches a camera (and is cfg-gated). Delivery depends only on a `QrFrameSource` trait; production camera adapter is swappable (research.md R-5).                                                                                       |
| III. Resource Discipline           | ✅     | Bounded queue (FR-009) with explicit eviction policy; bounded retry budget (FR-015); streaming uploads (reqwest `Body::wrap_stream`); journald-side rotation with documented `SystemMaxUse=200M` (quickstart.md §7); dependency picks are narrow, pure-Rust, single-tokio.                                                                       |
| IV. Observable, Not Chatty         | ✅     | `tracing` JSON-line events to stderr/journald (contracts/log-events.md); explicit allowlist of outbound destinations (research.md R-12) verified by `outbound_allowlist.rs` integration test; no telemetry endpoint; verbose mode opt-in and field-redaction-disciplined.                                                                              |
| V. Test-First (non-negotiable)     | ✅     | Every functional requirement maps to at least one host-runnable integration test (test matrix referenced under "Project Structure"). Hardware-bound paths covered by a documented release smoke test (quickstart.md §"What this quickstart does not prove"), not by mocks.                                                                              |
| Technology & resource constraints  | ✅     | Rust 2024, MSRV 1.95, Tokio sole runtime, no `openssl-sys`, AGPL-3.0+ in the workspace `Cargo.toml`. `unsafe` confined to `perchstation-hw`.                                                                                                                                                                                                  |
| Development workflow               | ✅     | Spec-driven via speckit (this command). CI configuration (`cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`) is a tasks-level concern, not a plan-level violation. Cross-project coordination with perchpub: the `references/openapi.json` mirror plus the contract-drift test is the coordination surface for this iteration. |

No violations to justify. Complexity Tracking section below is empty.

## Project Structure

### Documentation (this feature)

```text
specs/001-clip-delivery/
├── plan.md                  # This file
├── research.md              # Phase 0 — decisions, alternatives, constitution recheck
├── data-model.md            # Phase 1 — entities, lifecycle, on-disk layout
├── quickstart.md            # Phase 1 — dev-host end-to-end smoke
├── contracts/               # Phase 1
│   ├── perchpub-api.md      # Consumed subset of perchpub's HTTP API + station-side behaviour
│   ├── cli.md               # Operator CLI surface
│   └── log-events.md        # Structured log event codes
├── checklists/
│   └── requirements.md      # Existing spec quality checklist
├── spec.md                  # Feature spec (unchanged)
└── tasks.md                 # Phase 2 — created by /speckit-tasks, NOT this command
```

### Source Code (repository root)

```text
Cargo.toml                       # workspace manifest
crates/
├── perchstation-core/           # platform-agnostic
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── config.rs            # parsed config: perchpub URL, paths, ceilings
│       ├── enrollment/
│       │   ├── mod.rs
│       │   ├── csr.rs           # rcgen Ed25519 keypair + CSR (PEM)
│       │   └── confirm.rs       # POST /enrollment/confirm + validation + atomic persist
│       ├── identity.rs          # StationIdentity load/save + cert expiry parsing
│       ├── perchpub/
│       │   ├── mod.rs
│       │   ├── client.rs        # mTLS reqwest client, upload + classify-task GET, allowlist
│       │   └── types.rs         # serde mirrors: EnrollmentRequest/Response, ClassifyTaskPublic, ...
│       ├── queue/
│       │   ├── mod.rs
│       │   ├── store.rs         # directory layout, atomic renames, boot reconciliation
│       │   ├── policy.rs        # bounds + eviction strategies
│       │   └── inbox.rs         # Inbox trait (capture-subsystem-facing)
│       ├── delivery/
│       │   ├── mod.rs
│       │   ├── runner.rs        # the long-running delivery loop
│       │   ├── retry.rs         # exponential backoff + budgets
│       │   └── classify.rs      # classify-task poller
│       ├── observability/
│       │   ├── mod.rs
│       │   ├── tracing.rs       # JSON formatter, log-event helpers, redaction
│       │   └── status.rs        # snapshot used by `perchstation status`
│       └── hw_traits.rs         # QrFrameSource, Clock
│
├── perchstation-hw/             # only place hardware lives
│   ├── Cargo.toml               # cfg(target_os = "linux") + arch hints
│   └── src/
│       ├── lib.rs
│       ├── camera_qr.rs         # libcamera-still shell-out → QrFrameSource (deferred: native binding)
│       └── clock.rs             # SystemClock implementation
│
└── perchstation/                # the binary
    ├── Cargo.toml
    └── src/
        ├── main.rs
        ├── cli.rs               # clap definitions (global flags, subcommands)
        ├── commands/
        │   ├── enroll.rs        # `perchstation enroll`
        │   ├── serve.rs         # `perchstation serve`
        │   └── status.rs        # `perchstation status`
        └── bin/
            └── fakepub.rs       # dev-only fake perchpub for the quickstart

tests/
├── integration/
│   ├── enrollment_happy.rs          # spec US1 acceptance #1
│   ├── delivery_happy.rs            # spec US1 acceptance #2, #3
│   ├── reenroll_conflict.rs         # spec US1 acceptance #4
│   ├── outage_recovery.rs           # spec US2 acceptance #1, #2
│   ├── queue_eviction.rs            # spec US2 acceptance #3
│   ├── permanent_failure.rs         # spec US2 acceptance #4
│   ├── crash_recovery.rs            # spec US2 acceptance #5
│   ├── status_surface.rs            # spec US3 acceptance #1, #2
│   ├── outbound_allowlist.rs        # spec US3 acceptance #3 / SC-007
│   ├── log_redaction.rs             # contracts/log-events.md "Field discipline"
│   ├── support/                     # fake perchpub server, fake QrFrameSource, fixtures
│   └── fixtures/                    # PNG QR codes, sample.mp4, test certs/CAs
└── contract/
    └── openapi_sync.rs              # references/openapi.json drift check

deploy/
├── systemd/perchstation.service     # Type=notify unit (see quickstart §7)
└── config.example.toml              # commented example for /etc/perchstation/config.toml

references/
└── openapi.json                     # perchpub OpenAPI (existing)

CLAUDE.md                            # SPECKIT marker points at this plan
.specify/                            # speckit machinery (existing)
```

**Structure Decision**: Cargo workspace with three crates.
`perchstation-core` holds every line of delivery, enrollment, queue, and
perchpub-client logic and is fully runnable on a dev host.
`perchstation-hw` is the only crate that ever touches a camera/GPIO/sensor;
it is cfg-gated and trait-implementing. `perchstation` is the thin binary
that wires them together via `clap` subcommands. Integration tests live in
the workspace root `tests/` and exercise the full loop against a fake
perchpub (axum) and fake hardware sources, satisfying Principle II's
"runnable on a developer's machine" promise.

## Complexity Tracking

> No constitution violations. This table is intentionally empty.

| Violation | Why Needed | Simpler Alternative Rejected Because |
| --------- | ---------- | ------------------------------------- |
| —         | —          | —                                     |
