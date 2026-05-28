# Fixing a Ubuntu-only CI flake in `delivery_happy`

A short debugging write-up. PR #73 was green on macOS, red on `cargo test (ubuntu-latest)` with one failing assertion in `tests/integration/delivery_happy.rs:167`:

```
missing event classify.terminal in
  ["service.ready", "delivery.attempt_started",
   "delivery.upload_succeeded", "classify.polled"]
```

## Approach

I worked through the `superpowers:systematic-debugging` skill — four phases, no fix attempts before Phase 1 closed. That discipline mattered here because the "obvious" reading of the failure (bump the grace period) would have papered over a deeper test-design bug.

## Root cause

The poller in `classify.rs:131-159` runs two operations on every successful poll, in order: (1) `update_delivered_sidecar` — a rename-atomic write that flips `last_classify_status` to `Success` on disk; (2) `tracing::info!(event = CLASSIFY_TERMINAL, …)` — the event the test asserts on.

The test waited for `last_classify_status == "Success"` on disk, then slept 50 ms before SIGKILLing the subprocess. The 50 ms was the author's bet that tracing's unbuffered `write(2)` to the stderr pipe would land before the kill closed the fd. On Ubuntu CI scheduling stretched that window past 50 ms; the byte never made it to the kernel pipe buffer, and the parent's `wait_with_output()` returned a truncated stream.

## Why local reproduction failed

I ran the test 50× clean, 30× pinned to two cores under four background `yes` burners, and the full workspace suite five times. All green. The CI environment's particular mix of vCPU steal time, kernel version, and tokio worker scheduling wasn't reproducible inside the sandbox — but the *order* of operations in the production code was enough to confirm the race analytically.

## Backpressure from `gh`

The sandbox had no `gh` auth, so I worked from the public REST API. The job-listing endpoint gave me step durations and the failure annotation ("exit code 101"), but `actions/jobs/.../logs` returned 403 ("admin rights required"). The full subprocess stderr in the failed assertion — which would have told me whether the sidecar reached `Success` (Mode A: SIGKILL-before-flush) or stayed on `Prepared` (Mode B: second poll never fired within 10 s) — was therefore unreachable.

The fix I landed handles both modes: it observes the `classify.terminal` event directly in stderr (closing Mode A entirely) and, if Mode B is the real bug, fails with the same 10 s deadline but a strictly more diagnostic event list. No guesswork either way.

## Fix

`tests/integration/delivery_happy.rs` now drains the subprocess's stderr line-by-line in a background `tokio::spawn` task, parsing each line as JSON into a shared `Arc<Mutex<Vec<Value>>>`. The main task waits for `classify.terminal` to appear *in that buffer* before sending SIGKILL — so the event's `write(2)` has provably reached the kernel before the fd is closed. The fixed 50 ms grace period is gone; nothing about the deadline or the assertion set changed.

Verified: 50× tight loop, 30× under CPU contention, full workspace `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets --workspace -- -D warnings` — all clean.

## Skills used

- **`superpowers:using-superpowers`** at session start (mandatory).
- **`superpowers:systematic-debugging`** for the four-phase investigation. Pulled in the `condition-based-waiting.md` supporting technique once it was clear the test's `setTimeout`-equivalent (the 50 ms grace) was the symptom.

No production code touched. The race exists in production too — the runner emits `delivery.upload_succeeded` after `transition_delivered` — but the other integration tests don't gate on a post-mutation event, so the same fix isn't required there yet. If they start flaking, the same concurrent-drain pattern lifts cleanly into `support/logs.rs`.
