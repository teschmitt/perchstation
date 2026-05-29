# Methodology recap — feature 002 (capture subsystem)

## The pipeline (same six gates as 001, but tighter)

Every artefact below is one slash-command, one markdown file, one commit on `002-capture-subsystem`:

| # | Command | Output | Commit |
|---|---|---|---|
| 1 | `/speckit-specify` | `spec.md` (user stories, FRs, SCs, edge cases — no implementation language; gated by `checklists/requirements.md`) | `8bae06d` |
| 2 | `/speckit-plan` | `plan.md` + `research.md` (R-1…R-12) + `data-model.md` + `contracts/{cli,hw-traits,log-events}.md` + `quickstart.md` | `042429d` |
| 3 | `/speckit-tasks` | `tasks.md` — 40 tasks across 6 phases, each tagged `[P]`/`[Story]` with exact file paths | `58230b6` |
| 4 | `/speckit-analyze` | Cross-artefact consistency sweep → 3 reconciliation commits (`72125b0` MEDIUM/LOW, `bd76aeb` CRITICAL/HIGH, `046a54e`/`8222a7c` HIGH-MEDIUM/LOW) **before any Rust** | — |
| 5 | `/speckit-implement` | 5 phase commits: foundational (`acce31a`) → US1 MVP (`668356c`) → US2 robustness (`b9753b4`) → US3 status (`ada1f6c`) → polish (`fb7f67f`) | — |
| 6 | PR #74 review | Parallel per-phase review subagents → 3 follow-up commits (`28a30c7`, `715516d`, `9e8ec94`) | — |

## Tooling stack

- **Spec Kit plugin** (`/speckit-*` skills) — the six gates above. Installed as Claude Code skills under `.claude/skills/`.
- **Constitution v1.1.0** (`.specify/memory/constitution.md`) — five principles plus, **new for 002**, a **subagent-driven implementation principle**: "Tasks executed by fresh agents with no prior context. Shared types in `data-model.md`, interfaces in `contracts/`, before implementation. Each task self-contained, 1–2 files, dependencies stated explicitly." This is what made `[P]` task fan-out actually safe.
- **Claude Code session tooling** (`50ce0e3`) — `.claude/hooks/cargo-attempts-logger.sh` and `.claude/scripts/generate-reasoning.sh`, added between 001 and 002.
- **GitHub PR + parallel code-review subagents** — one review subagent per phase commit, table-summarised (0/0/1/0 blocking, 0/3/0/0 nit) before any fix was written.

## Discipline rules that showed up in the commit graph

- **Foundational-before-stories.** `acce31a` lands `CaptureConfig`, the two traits, 14 `capture.*` event-code constants, the `CaptureSnapshot` read-side projection, and both test fakes (`FakeMotionSensor`, `FakeCamera`) in one commit. Nothing in Phases 3–5 invents a shared type.
- **One phase, one commit, hard gates.** Each phase commit was `fmt --check` + `clippy -D warnings` + `cargo test --workspace` green before the next opened. The PR description carries the gate table.
- **`/speckit-analyze` catches what single-doc review misses.** Real examples: `contracts/hw-traits.md` claimed `capture_bounded_clip.rs` exercised `Mode::Hang` but `tasks.md` correctly used `Mode::Ok` → analyze surfaced the missing camera-hang test path → new task `T029a` + new `capture_camera_hang.rs`. Also caught: `cli.md` text rendering said "(unknown)" while the JSON enum had no matching variant → resolved by adding a fourth `never_observed` liveness variant.
- **Contracts evolve in the same patch as code.** Phase-3 review nit #2 added `CAPTURE_INIT_FAILED`/`CAPTURE_SKIPPED` constants *and* the matching rows in `contracts/log-events.md` in `28a30c7`. No drift TODOs.
- **Retrospective → constitution → next feature.** `docs/001-clip-delivery-retrospective.md` (`46e7882`) → constitution amendment `cb55084` → the v1.1.0 subagent principle was the binding rule for 002, not a vibe.

## Net delta from 001

|  | 001 (clip-delivery) | 002 (capture) |
|---|---|---|
| Constitution | v1.0.0 (5 principles) | **v1.1.0** (+ subagent-driven) |
| `/speckit-analyze` rounds | 1 (`5af261c`) | **4** (severity-tiered: CRITICAL/HIGH → HIGH/MEDIUM → LOW) |
| Phase commits | 6 | 5 (foundational merged into US1's predecessor) |
| Code review | post-hoc | **per-phase, parallel subagents, summarised on PR** |
| Session tooling | none | `.claude/hooks/` + `.claude/scripts/` |
