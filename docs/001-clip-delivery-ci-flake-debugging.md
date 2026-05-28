# Fixing a macOS-flaky `queue_eviction` test

PR #73 went red on `cargo test (macos-latest)` with an empty event buffer. Ran Phase 1 of `superpowers:systematic-debugging` first: parallel reads of the failing test, helper, production code, CI log, and tracing-core source before forming a position. Root cause turned out to be a process-global cache in tracing-core poisoned by a sibling test that fires the same event from a thread with no scoped subscriber. Reproducible on Linux at ~1/30 runs; not macOS-specific.

**Backpressure from `gh`.** `gh run view --job <id> --log-failed | head -200` for the panic and per-test completion order; `gh run view <id> --json status,conclusion,jobs` to ask "is every required job green?" as a five-line projection; `gh run watch <id> --exit-status` in the background to avoid polling.

**Backpressure from `cargo`.** Aggregate in the shell, not the conversation. Stress loops of 30× and 200× emitted one summary line each (`==> 0 failures across 200 runs`) — never the per-run compile chatter. One reproduction was enough — source analysis plus a single observed failure was sufficient evidence to act.

**Skills used.** `superpowers:systematic-debugging` for the parallel-read Phase 1 that pushed past the handoff's surface hypotheses; `commit` for message + push. `superpowers:verification-before-completion` followed in spirit — done is "CI green on the previously-failing job", not "I think the local fix works".
