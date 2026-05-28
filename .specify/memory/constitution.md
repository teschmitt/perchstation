<!--
SYNC IMPACT REPORT
==================
Version change: (template, unfilled placeholders) → 1.0.0
Bump rationale: Initial ratification — first concrete constitution replacing
the unfilled `.specify/memory/constitution.md` template. Treated as 1.0.0
because there is no prior published version to compare against.

Modified principles:
  - [PRINCIPLE_1_NAME] → I. Unattended Reliability (new)
  - [PRINCIPLE_2_NAME] → II. Hardware at the Boundary (new)
  - [PRINCIPLE_3_NAME] → III. Resource Discipline (new)
  - [PRINCIPLE_4_NAME] → IV. Observable, Not Chatty (new)
  - [PRINCIPLE_5_NAME] → V. Test-First Where It Matters (new, non-negotiable
    for non-hardware code)

Added sections:
  - Technology & Resource Constraints
  - Development Workflow
  - Governance

Removed sections:
  - None (template placeholders replaced in-place)

Templates requiring updates:
  - ✅ .specify/templates/plan-template.md — "Constitution Check" section
    already references the constitution dynamically; no edit needed for the
    initial ratification. Future amendments should re-evaluate.
  - ✅ .specify/templates/spec-template.md — no constitution-specific
    references; no edit needed.
  - ✅ .specify/templates/tasks-template.md — task categories are generic
    and compatible with the new principles (testing discipline, observability,
    resource discipline all expressible as tasks); no edit needed.
  - ✅ .specify/templates/checklist-template.md — not inspected in detail;
    no known constitution-coupled fields. Flag for review on next amendment.
  - ✅ CLAUDE.md — points to "the current plan" for tech context; remains
    accurate. No edit needed.

Follow-up TODOs:
  - Confirm the intended wording for the truncated fragment in the
    Technology & Resource Constraints section ("Cross-compilation is the
    expected build workflow; on-device builds are not required."). The
    original input read "...workflow; on-device builds are not required."
    — the leading clause was reconstructed from context.
  - Confirm `RATIFICATION_DATE` (2026-05-26) reflects the actual adoption
    date; adjust if the project decides to backdate to an earlier moment.
-->

# Perchstation Constitution

Perchstation is a Rust client application that runs on a Raspberry Pi inside
a bird feeder. It detects birds via motion and weight sensors, records short
video clips, and sends them to a perchpub backend
(<https://codeberg.org/perchpub/perchpub>) so a hobbyist owner can watch their
visitors in a friendly UI.

The device runs unattended outdoors, often for months, on a small SBC behind
home Wi-Fi. Its users are not engineers; they want it to work and stay out of
their way. These constraints — unattended hardware, resource limits, lay
users, and being one half of a client/server pair — drive the principles
below.

## Core Principles

### I. Unattended Reliability

Every failure mode MUST be designed for, not discovered in the field. Code
fails safe: queue rather than drop, log and continue rather than crash, retry
with backoff rather than give up. The application MUST survive power loss,
network outages, sensor glitches, and clock skew without human intervention.
Restart-as-recovery is acceptable; silent data loss is not.

*Rationale*: The device lives outdoors for months at a time, owned by people
who cannot SSH in to debug a crash loop. Anything we don't design for is a
field incident we can't reach.

### II. Hardware at the Boundary

Code that talks to real hardware — camera, GPIO, motion sensor, weight
sensor, the network — MUST live behind narrow Rust traits in a thin adapter
layer. Everything else (queueing, retry, scheduling, business logic,
perchpub protocol) MUST be platform-agnostic and runnable on a developer's
machine against in-memory or filesystem fakes. The bulk of the codebase MUST
be testable without a Pi.

*Rationale*: Hardware is slow, scarce, and stateful. Pushing the hardware to
the edge keeps the iteration loop fast and the test surface honest.

### III. Resource Discipline

A Raspberry Pi is not a server. Memory, CPU, and SD-card write endurance are
first-class constraints, not afterthoughts. Queues and buffers MUST be
bounded. Busy loops are forbidden. Log files MUST be rotated and capped; log
levels and rotation MUST be designed to protect the SD card. Long-running
allocations MUST be scrutinized. Choosing a crate that doubles the binary
size or pulls in a `tokio-1.x`-and-`1.y` graph is a design decision, not a
default.

*Rationale*: An SD card with a worn-out flash region or a process OOM-killed
overnight is indistinguishable, from the owner's perspective, from "the
camera broke." Resource budgets keep us out of that failure class.

### IV. Observable, Not Chatty

The device MUST emit structured logs (one event per line, machine- and
human-readable) to standard error / the system journal. Logs MUST be useful
at a glance for a hobbyist over SSH and parseable for tooling. The device
MUST NOT emit telemetry, phone-home traffic, or analytics — a backyard
camera respects its owner's privacy. Verbose modes MAY exist for debugging
but MUST be opt-in.

*Rationale*: Owners trust us with footage of their backyards and, by
extension, their homes. Observability without telemetry means we get
diagnostics without becoming a surveillance product.

### V. Test-First Where It Matters (NON-NEGOTIABLE for non-hardware code)

TDD applies to all platform-agnostic code: queueing, retry logic, perchpub
client, configuration, state machines. Tests MUST be written first, MUST
fail first, then pass. Hardware adapters MUST be tested via the trait
boundary with fakes; real-hardware integration MUST be exercised through a
documented manual smoke test before each release. No fake test theatre — if
something can only be verified on a Pi, the spec MUST say so explicitly
rather than mocking around it.

*Rationale*: The only code paths that get exercised reliably in the field are
the ones we exercised reliably before shipping. TDD is how we keep that
honest for the parts of the codebase that don't need a Pi to run.

## Technology & Resource Constraints

- **Language**: Rust, stable toolchain, edition 2021 or later.
- **Target hardware baseline**: 64-bit Raspberry Pi OS on Pi 4 and Pi Zero 2 W.
  Older Pis are not supported unless an explicit decision is made.
  Cross-compilation is the expected build workflow; on-device builds are not
  required.
- **Async runtime**: A single, project-wide choice (likely Tokio); mixing
  runtimes is forbidden.
- **`unsafe` Rust**: Forbidden outside the hardware adapter layer. Where it
  appears there, each `unsafe` block MUST carry a comment explaining the
  invariant it upholds.
- **Dependency policy**: Prefer mature, actively maintained crates. Anything
  touching hardware or networking MUST be reviewed for licensing and
  maintenance status before adoption.
- **License**: AGPL-3.0 or later.
- **System of record**: Perchpub is the source of truth for stored clips. The
  device is a cache and a sensor, not a database.

## Development Workflow

- **Spec-driven development via speckit**: Every non-trivial feature has a
  spec → clarify → plan → tasks → implement cycle. Drive-by changes are
  reserved for fixes that don't change behavior.
- **Subagent-driven implementation**: Tasks are executed by fresh agents
  with no prior context. All shared types MUST be in `data-model.md` and
  all interfaces in `contracts/` before implementation begins. Each task
  MUST be self-contained (context, file paths, acceptance criteria) and
  touch 1–2 files. Cross-task dependencies MUST be minimised and stated
  explicitly where unavoidable.
- **Branching and review**: One feature per branch, reviewed before merge to
  `main`. Commit messages follow the conventions already in the repository.
- **Continuous integration**: CI MUST run `cargo fmt --check`,
  `cargo clippy -- -D warnings`, unit tests, and host-runnable integration
  tests on every PR. A red CI does not merge.
- **Release gate**: On-device smoke test required before tagging a release.
  The documented manual test plan MUST pass on real hardware against a real
  perchpub instance.
- **Cross-project coordination**: Changes that affect the perchpub wire
  protocol require explicit coordination with the perchpub project before
  merge.

## Governance

The constitution supersedes ad-hoc preferences. Amendments are made through
pull requests that include rationale, a migration note for any in-flight
specs, and an updated version number. Reviewers MUST verify that PRs (specs,
plans, and code) comply with these principles; complexity that violates a
principle MUST carry a justification and an explicit exception.

Versioning policy follows semantic versioning:

- **MAJOR**: Backward-incompatible governance or principle removals or
  redefinitions.
- **MINOR**: A new principle or section is added, or material guidance is
  expanded.
- **PATCH**: Clarifications, wording, typo fixes, non-semantic refinements.

Runtime development guidance lives in the project's `CLAUDE.md` (and any
sibling agent-guidance files); those files defer to this constitution where
they conflict.

**Version**: 1.1.0 | **Ratified**: 2026-05-26 | **Last Amended**: 2026-05-28
