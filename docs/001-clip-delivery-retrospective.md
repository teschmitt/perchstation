# Building perchstation's clip-delivery subsystem with Spec Kit

A retrospective on the *workflow*, not the code. The feature itself — a Rust subsystem that uploads bird-feeder clips to perchpub — was the test case; the question is how Spec Kit shaped the way it got built.

## The pipeline

Spec Kit walked the feature through six gated artefacts, each one a slash-command producing a markdown document that fed the next:

1. **`/speckit-constitution`** ratified the project constitution as commit `183a53a` *before* the first feature was even specced. Its five principles (unattended reliability, hardware-at-the-boundary, resource discipline, observable-not-chatty, test-first) became the gate that every later plan had to pass.
2. **`/speckit-specify` → `spec.md`** captured user stories with priorities, requirements, edge cases, and success criteria — no implementation language. A separate `checklists/requirements.md` gated that constraint.
3. **`/speckit-plan` → `plan.md` + `research.md` + `data-model.md` + `contracts/` + `quickstart.md`**. Phase 0 closed every `NEEDS CLARIFICATION` (R-1 … R-12); Phase 1 produced contracts and a runnable quickstart. A Constitution Check table in `plan.md` proved each principle held before any task was generated.
4. **`/speckit-tasks` → `tasks.md`** generated 69 tasks across six phases, tagged with user-story ownership, parallel-safety, and exact file paths.
5. **`/speckit-analyze`** ran a cross-artefact consistency sweep (commit `5af261c`) that refined plan + tasks before implementation began. This catches contradictions that single-document review misses.
6. **`/speckit-implement`** executed tasks phase-by-phase, committing at each checkpoint.

## How the methodology actually showed up in the work

**TDD enforced by commit ordering, not just stated.** Phase 3 is the clearest example: commit `0e24a5a` lands four integration tests *RED* (failing meaningfully on the existing `unimplemented!()` stubs) before any implementation exists. The next two commits — `a7e4ed9` enrollment, `04c3fe0` delivery loop — turn those same tests GREEN. The commit graph is the proof.

**Phase boundaries as natural backpressure.** Six commits map one-to-one onto six phases. Each phase's checkpoint is a hard stop: green tests + clean clippy + clean fmt before the next phase opens. No half-finished phase ever crossed a commit boundary.

**Contracts evolve with the code, not separately.** When implementation forces a contract change, the change is committed in the same patch. The enrollment commit added two missing rows to `contracts/log-events.md` (`enrollment.overwritten`, `enrollment.session_invalid`); the polish commit amended `quickstart.md` to match a `--ca-key` flag the dev-only `fakepub` binary actually grew. Documentation drift is a code-review concern, not a future-cleanup TODO.

**Backpressure was a methodology decision, not a code one.** The spec named the queue-bound policy as an explicit, configurable choice (`drop_oldest_undelivered` vs `refuse_new`) in FR-009 — not a code-level detail. `research.md` R-7 settled the retry numbers (12 attempts, 24 h wall-clock, ±20 % jitter) before any line of `retry.rs` was written. Tests for the bounded-traffic invariant (`outage_recovery.rs`, `queue_eviction.rs`, the retry unit tests) landed RED before the scheduler existed. The result: every backpressure decision is traceable from spec → research → contract → RED test → implementation, with no ad-hoc "I'll just add a sleep here" hidden in a code commit.
