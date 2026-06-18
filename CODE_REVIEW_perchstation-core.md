# Code review findings — `crates/perchstation-core`

**Scope:** the whole `perchstation-core` crate as checked in on `main`.
**Date:** 2026-06-18. **Method:** max-effort `/code-review` (10 finder angles → cluster verifiers → gap sweep), then a follow-up agent pass that re-read each file to verify line numbers, extract real excerpts, and write concrete remediations + test guidance.

> **Line numbers** were verified against `main` at review time. As you land fixes the offsets will drift — trust the function/symbol names over the exact line. Each finding is independently fixable; dependencies are called out.

## How to use this doc

Each finding is a self-contained unit: **Problem** (what's wrong) → **Trigger** (how it bites) → **Fix** (concrete remediation referencing real symbols) → **Tests** (what to add and where). Flip `Status:` from `todo` to `done` (or tick the checklist below) as you go. Severity drives priority; effort is `S` (local edit) / `M` (one module + tests) / `L` (cross-module / design).

### Progress checklist

**Queue crash-safety & integrity**
- [x] PS-01 — reconcile re-queues terminal entries → duplicate delivery *(Critical)*
- [x] PS-02 — one corrupt sidecar wedges the delivery & classify loops forever *(Critical)*
- [x] PS-04 — orphan-media entries permanently wedge the delivery loop *(High)*
- [x] PS-07 — `enqueue` orphans the mp4 if the sidecar write fails *(High)*

**Queue policy & eviction**
- [x] PS-03 — `max_clips = 0` (unvalidated) silently bricks the queue *(High)*
- [ ] PS-05 — eviction counts un-evictable entries → drops fresh clips yet still `QueueFull` *(High)*
- [ ] PS-20 — phantom byte accounting for delivered entries *(Low)*
- [ ] PS-21 — eviction-reason mislabel when both ceilings breach *(Low)*

**Delivery, retry & backoff**
- [ ] PS-06 — a 200 with undecodable body / unknown status → re-upload / infinite poll *(High)*
- [x] PS-08 — no numeric config validation → downstream overflow panics *(Medium)*
- [ ] PS-11 — jitter bypasses the injected `Clock` → retry storms + untestable *(Medium)*
- [ ] PS-12 — disk-full backoff defaults to 1 hour *(Medium)*
- [ ] PS-25 — `delivered/` never pruned → unbounded re-scan every tick *(Medium)*
- [ ] PS-27 — `apply_policy` runs synchronous all-sidecar reads on the reactor *(Medium)*

**Concurrency & task lifecycle**
- [ ] PS-09 — `spawn_supervised` nests `tokio::spawn`; abort detaches the worker *(Medium)*
- [ ] PS-10 — eviction races `transition_inflight` on `pending/` with no lock *(Medium)*

**Perchpub wire client**
- [ ] PS-16 — response body read with no size cap → OOM *(Medium)*
- [ ] PS-18 — client never reloads creds after re-enrollment *(Low)*
- [ ] PS-22 — 2xx-other status collapsed to Terminal → `Undeliverable` *(Low)*
- [ ] PS-23 — `Retry-After` HTTP-date form dropped *(Low)*

**Enrollment & identity (security)**
- [ ] PS-13 — `validate_chain` accepts expired / non-CA certs *(High)*
- [ ] PS-14 — enrollment client follows redirects → re-sends `auth_token`+CSR *(Medium)*
- [ ] PS-15 — untrusted QR images decoded with no size limits → decompression bomb *(Medium)*
- [ ] PS-17 — no parent-dir fsync after credentials rename *(Medium)*
- [ ] PS-19 — `cert_is_expired` strict `<` boundary *(Low)*

**Observability**
- [ ] PS-26 — `RedactingWriter` clones the secrets `Vec` on every log line *(Low)*
- [ ] PS-24 — `pick_last_failure` filter fragility *(Low — STALE, hardening only)*

**Conventions, portability & cleanup**
- [ ] PS-28 — top-level `std::os::unix` import makes core non-portable *(Cleanup)*
- [ ] PS-29 — hardware `sensor_*`/`camera_*` fields in core config *(Cleanup)*
- [ ] PS-30 — `/dev/gpiochip0` default hardcoded in core *(Cleanup)*
- [ ] PS-31 — duplicated logic (clip-path, sidecar read, TLS builder, dir tally) *(Cleanup)*
- [ ] PS-32 — dead crate-level `Error` enum + `Result` alias *(Cleanup)*
- [ ] PS-33 — cooldown `last_outcome` write-only; doc comment is false *(Cleanup)*

## Summary

| ID | Sev | Eff | Title | Primary file | Depends |
|----|-----|-----|-------|--------------|---------|
| PS-01 | Critical | L | reconcile re-queues terminal → duplicate delivery | queue/store.rs, delivery/runner.rs | PS-04 |
| PS-02 | Critical | M | corrupt sidecar wedges delivery & classify loops | queue/store.rs, delivery/classify.rs | (PS-25) |
| PS-04 | High | L | orphan-media permanent wedge; no MissingMedia arm | queue/store.rs, delivery/runner.rs | PS-01, PS-07 |
| PS-07 | High | S | `enqueue` orphans mp4 on sidecar-write failure | queue/store.rs | — |
| PS-03 | High | M | `max_clips=0` bricks the queue | queue/policy.rs, config.rs | PS-08 |
| PS-05 | High | M | eviction counts un-evictable → drops fresh clips | queue/policy.rs | PS-20, PS-21 |
| PS-13 | High | M | `validate_chain` accepts expired / non-CA certs | enrollment/confirm.rs | PS-11 |
| PS-06 | High | M | undecodable 200 / unknown status → re-upload / infinite poll | delivery/retry.rs, perchpub/types.rs, perchpub/client.rs | — |
| PS-08 | Medium | M | no numeric config validation → overflow panics | config.rs, capture/recording.rs, delivery/retry.rs | PS-03 |
| PS-09 | Medium | M | `spawn_supervised` abort detaches worker | supervision.rs | (PS-12) |
| PS-10 | Medium | L | eviction races `transition_inflight`, no lock | queue/policy.rs, queue/store.rs | — |
| PS-11 | Medium | S | jitter bypasses injected `Clock` | delivery/retry.rs | — |
| PS-12 | Medium | S | disk-full backoff = 1 hour | delivery/runner.rs | — |
| PS-14 | Medium | S | enrollment client follows redirects | enrollment/confirm.rs | — |
| PS-15 | Medium | M | QR image decode has no size limits | enrollment/file_source.rs, enrollment/mod.rs | — |
| PS-16 | Medium | M | unbounded response body → OOM | perchpub/client.rs | — |
| PS-17 | Medium | S | no parent-dir fsync after rename | identity.rs | PS-28 |
| PS-25 | Medium | M | `delivered/` never pruned → unbounded re-scan | delivery/classify.rs, queue/policy.rs | — |
| PS-27 | Medium | M | `apply_policy` blocking reads on the reactor | queue/policy.rs | PS-05, PS-20 |
| PS-18 | Low | M | no cert reload after re-enrollment | perchpub/client.rs | — |
| PS-19 | Low | S | `cert_is_expired` strict `<` | identity.rs | — |
| PS-20 | Low | M | phantom byte accounting | queue/policy.rs | PS-05 |
| PS-21 | Low | S | eviction-reason mislabel | queue/policy.rs | — |
| PS-22 | Low | S | 2xx-other → Terminal/`Undeliverable` | perchpub/client.rs | — |
| PS-23 | Low | S | `Retry-After` HTTP-date dropped | perchpub/client.rs | PS-11 |
| PS-24 | Low | S | `pick_last_failure` fragility (**STALE**) | observability/status.rs | — |
| PS-26 | Low | M | `RedactingWriter` clones secrets per write | observability/tracing.rs | — |
| PS-28 | Cleanup | S | `std::os::unix` import in core | identity.rs | — |
| PS-29 | Cleanup | L | hardware fields in core config | config.rs | PS-30 |
| PS-30 | Cleanup | S | `/dev/gpiochip0` default in core | config.rs | PS-29 |
| PS-31 | Cleanup | M | duplicated logic across modules | (multi-file) | PS-02 |
| PS-32 | Cleanup | S | dead crate-level `Error`/`Result` | lib.rs | — |
| PS-33 | Cleanup | S | cooldown `last_outcome` write-only + false doc | capture/cooldown.rs | — |

## Recommended fix order

The four queue-store crash-safety findings (PS-01, PS-02, PS-04, PS-07) share one root cause — **rename-then-write with no rollback and a `reconcile` that doesn't check terminality** — and are best done as one focused pass. A config-validation pass (PS-03 + PS-08) is a shared prerequisite for several others. Suggested batches:

1. **Config validation foundation** — PS-03 + PS-08 land as a single `Config::validate` pass wired into `serve`/`enroll` startup (note `serve.rs` currently bypasses `ensure_runtime_ready`).
2. **Queue crash-safety core** — PS-07 → PS-01 → PS-04 (mutually entangled in `reconcile_inflight`/`transition_*`), then PS-02 (corrupt-sidecar skip, also unblocks PS-31's shared reader).
3. **Loop resilience** — PS-12, PS-25, then PS-09 (task lifecycle / cancellation).
4. **Eviction correctness** — PS-05 with PS-20 + PS-21 (same `count_queue`/loop), then PS-27 (move it off the reactor), PS-10 (locking).
5. **Delivery & wire correctness** — PS-06, PS-11, PS-16, PS-22, PS-23, PS-18.
6. **Enrollment & identity security** — PS-13, PS-14, PS-15, PS-17, PS-19.
7. **Conventions & cleanup** — PS-28 (unblocks PS-17's dir-fsync cfg strategy), PS-29 + PS-30, PS-31, PS-32, PS-33, PS-24.

---

# Queue crash-safety & integrity

## PS-01 — reconcile_inflight re-queues terminal entries → duplicate delivery
**Severity:** Critical · **Effort:** L · **Confidence:** confirmed · **Status:** done
**Files:** `crates/perchstation-core/src/queue/store.rs:228-252,265-283`; `crates/perchstation-core/src/delivery/runner.rs:176-200`
**Depends on:** PS-04

```rust
// reconcile_inflight (store.rs): no is_terminal() check
let mut entry = read_sidecar(&sidecar)?;
entry.next_attempt_after = None;
entry.last_error = None;
self.transition_back_to_pending(&entry)?;
// runner.rs try_once success branch:
match self.client.upload_clip(&mp4_path, &clip_id).await {
    Ok(task) => { ... delivered.outcome = Some(Outcome::Delivered); ...
        self.store.transition_delivered(&delivered)?;
```

**Problem:** `reconcile_inflight` (store.rs:243-251) re-queues every `inflight/*.json` with no `is_terminal()`/outcome check. `transition_delivered` writes the `Delivered` sidecar (store.rs:271) **before** unlinking the mp4 (273) and renaming to `delivered/` (279). A crash in that window leaves a terminal sidecar in `inflight/` that gets re-queued and re-uploaded. A **second, independent** window lives in `runner.rs`: `upload_clip` returns `Ok` (176, bytes accepted by perchpub) but `transition_delivered` fails (185), so the entry stays non-terminal in `inflight/` and is re-queued next boot.
**Trigger:** Power loss after `write_sidecar_atomic` at store.rs:271 but before the rename at 279; **or** `upload_clip` succeeds then `transition_delivered` at runner.rs:185 returns `Err` (e.g. ENOSPC writing the sidecar). On next boot `reconcile_inflight` re-queues and `try_once` re-uploads → perchpub receives a duplicate clip + duplicate classify task.
**Fix:**
1. In `reconcile_inflight` (store.rs:243-251), after `read_sidecar`, guard on terminality: if `entry.is_terminal()` (helper at `queue/mod.rs:127`), **finish** the interrupted transition by calling `transition_delivered(&entry)` (idempotent — unlink+rename already tolerate `NotFound`) instead of re-queueing, then `continue`. An entry that crashed *before* upload (no outcome) must still be re-queued.
2. The accept-vs-record window in runner.rs:176-200 is inherently **at-least-once**; re-upload must be made *safe* rather than prevented: require perchpub to treat `upload_clip` as idempotent keyed on `clip_id` (already passed at 176), document the window in the runner.rs header doc, and add an inline comment at runner.rs:185 that a failure here leaves an inflight entry for reconcile to *finish*, not re-send.
Preserve the data-model invariant that the mp4 is unlinked before the `delivered/` rename. Coordinate with PS-04 (same reconcile/transition rework).
**Tests:** store.rs `#[cfg(test)]`: `reconcile_inflight_finishes_terminal_entry_left_in_inflight` — hand-write `inflight/<id>.json` with `outcome=Delivered` (+ mp4, simulating a crash before unlink/rename), assert it lands in `delivered/`, not `pending/`, and is absent from the returned recovered Vec; `reconcile_inflight_skips_terminal_does_not_requeue`. runner tests: inject a `QueueStore` whose `transition_delivered` fails after a successful upload; assert reconcile does not cause a second `upload_clip` against an idempotent client stub.
**Notes:** `reconcile_inflight` is called once at boot from `crates/perchstation/src/commands/serve.rs:82-84`. `is_terminal()` = `outcome.is_some()` at `queue/mod.rs:127`.

## PS-02 — a single corrupt sidecar wedges the delivery & classify loops forever
**Severity:** Critical · **Effort:** M · **Confidence:** confirmed · **Status:** done
**Files:** `crates/perchstation-core/src/queue/store.rs:113-140` (delivery loop); `crates/perchstation-core/src/delivery/classify.rs:101-129` (classify poller)
**Depends on:** PS-25 (soft — shared warn-dedup / in-memory set)

```rust
// store.rs pick_oldest_pending:
sidecars.sort();
for path in sidecars {
    let entry = read_sidecar(&path)?;   // <- ? aborts the whole scan
// classify.rs scan_non_terminal:
let bytes = fs::read(&path).map_err(|s| PollerError::DeliveredIo { .. })?;
let entry: ClipQueueEntry = serde_json::from_slice(&bytes)
    .map_err(|s| PollerError::DeliveredParse { .. })?;   // <- same
```

**Problem:** Both `pick_oldest_pending` (store.rs:113-140) and `scan_non_terminal` (classify.rs:101-129) call `read_sidecar`/`from_slice` with `?` *inside* the scan loop over their (sorted) directory. A single corrupt/truncated `*.json` makes the whole scan return `Err` on every tick. In the delivery loop the runner catch-all (runner.rs:124-130) just logs + sleeps `IDLE_TICK`, forever — even healthy newer clips are never picked. In the classify poller `poll_round` returns `Err` every tick (5–50ms) and no entry is ever polled; journald is spammed.
**Trigger:** Any `pending/*.json` or `delivered/*.json` that fails `serde_json::from_slice` (truncation from a crash mid-write outside the atomic path, partial fsync, disk corruption, or a hand edit). Both loops spin permanently with zero progress.
**Fix:** In both loops, do **not** `?`-propagate per-entry deserialise failures. Match on the read: on `Ok` proceed (keep the `next_attempt_after` backoff skip in `pick_oldest_pending` and the terminal/`classify_task_id` filters in `scan_non_terminal`); on `Err(Deserialise/DeliveredParse/DeliveredIo)` emit a `tracing::warn` and `continue`. Keep the outer `read_dir` error fatal. **Prefer quarantine** (rename the bad sidecar + any mp4 into a `corrupt/` subdir) so the head advances permanently rather than re-warning every tick; otherwise rate-limit the per-path warn (a `HashSet` of warned paths, or fold into PS-25's in-memory polling set). The now-unused `PollerError::DeliveredParse/DeliveredIo` read/parse variants can be repurposed.
**Tests:** store.rs: `pick_oldest_pending_skips_corrupt_sidecar` — a valid newer entry + an invalid older `pending/<id>.json` (`b"{ not json"`); assert `Ok(Some(valid))`, and (if quarantined) the bad file moved out. classify.rs (add a `#[cfg(test)]` module — none today): `scan_skips_corrupt_sidecar_and_continues` and `poll_round_advances_despite_corrupt` returning `Ok(1)`.

## PS-04 — orphan-media entries permanently wedge the delivery loop; no `MissingMedia` arm, no cancellation
**Severity:** High · **Effort:** L · **Confidence:** confirmed · **Status:** done
**Files:** `crates/perchstation-core/src/queue/store.rs:151-187,203-221`; `crates/perchstation-core/src/delivery/runner.rs:113-141`
**Depends on:** PS-01, PS-07

```rust
// store.rs transition_inflight: rename then write, no rollback
fs::rename(&pending_mp4, &inflight_mp4).map_err(|s| QueueError::Io { .. })?;
write_sidecar_atomic(&inflight_sidecar, &updated)?;   // fail here => orphaned mp4
// runner.rs run(): MissingMedia falls into the generic catch-all
Err(err) => { tracing::warn!(message=%err, "delivery iteration aborted on internal queue error"); sleep(IDLE_TICK).await; }
```

**Problem:** `transition_inflight` renames the mp4 to `inflight/` (175) then writes the sidecar (178) with **no rollback** — if the write fails, the mp4 is orphaned in `inflight/` while the `pending/` sidecar remains, and `reconcile_inflight` (which only enumerates `inflight/*.json`, store.rs:237) can't recover it. Separately, `transition_back_to_pending`'s `if inflight_mp4.exists()` guard (210) means a reconciled terminal entry whose mp4 was already unlinked becomes a `pending/` sidecar with no mp4. Either way the next `transition_inflight` returns `MissingMedia` (162-164) every tick; the runner funnels it into the generic catch-all (124-130) that only logs + sleeps, so `pick_oldest_pending` keeps returning the same orphan and the queue head never advances. Also: `DeliveryRunner::run` is a pure `loop {}` (97) with **no** `CancellationToken` arm.
**Trigger:** (a) `write_sidecar_atomic` fails at store.rs:178 after the rename at 175 succeeds → mp4 stranded in `inflight/`. (b) PS-01's reconcile re-queues a `Delivered` entry whose mp4 was unlinked → `pending/` sidecar with no mp4. Either → permanent no-progress on the queue head.
**Fix:**
1. Make `transition_inflight` rollback-safe: if `write_sidecar_atomic` (178) fails, best-effort rename `inflight_mp4` back to `pending_mp4` before returning the error.
2. Add a dedicated arm in `run()` between the `DiskFull` arm and the catch-all: `Err(RunnerError::Queue(QueueError::MissingMedia { clip_id })) => { ... }` that **quarantines/removes** the orphan sidecar (e.g. a `QueueStore::quarantine_orphan(clip_id)` helper that removes `pending/<id>.json` idempotently, or moves it to `delivered/` stamped `Undeliverable` with `last_error.kind="missing_media"` so `status` surfaces it) and `continue`s so the next pick advances.
3. Thread a `tokio_util::CancellationToken` into `run()` (and `ClassifyPoller::run`) and add a `_ = token.cancelled() => break` `select!` arm (see PS-09).
**Tests:** store.rs: `transition_inflight_rolls_back_mp4_on_sidecar_write_failure`. runner: `try_once_quarantines_missing_media_and_advances` — stage a `pending/` sidecar with no mp4, run `try_once`, assert `Ok(true)` and the orphan is removed/quarantined (a second `try_once` returns `Ok(false)`); `quarantine_orphan_removes_sidecar_idempotently`; `run_exits_on_cancellation` once the token is threaded.

## PS-07 — `enqueue` orphans the mp4 if the sidecar write fails
**Severity:** High · **Effort:** S · **Confidence:** confirmed · **Status:** done
**Files:** `crates/perchstation-core/src/queue/store.rs:88-107`

```rust
if fs::rename(clip_source, &mp4_target).is_err() { ... }   // mp4 placed first
let entry = ClipQueueEntry::new(clip_id.clone(), meta.captured_at, Utc::now(), byte_size);
let sidecar_target = pending.join(format!("{clip_id}.json"));
write_sidecar_atomic(&sidecar_target, &entry)?;            // fail => orphan mp4
```

**Problem:** `enqueue` moves the mp4 into `pending/<id>.mp4` (88-101) **before** `write_sidecar_atomic` (105). If the sidecar write fails, the mp4 is orphaned in `pending/` with no sidecar. `pick_oldest_pending` and `count_queue` only enumerate `*.json`, so the orphan is invisible, never delivered, never cleaned → silent disk leak that eats the eviction budget.
**Trigger:** `write_sidecar_atomic` at store.rs:105 returns `Err` (disk full writing the `.json`/`.tmp`, or a crash between the two renames). On the disk-constrained Pi these accumulate.
**Fix:** Option A (preferred): on `write_sidecar_atomic` failure at 105, best-effort `remove_file(&mp4_target)` before returning the error so no orphan remains (clip_id is freshly generated per call, so no collision risk). Option B: add a boot sweep (in `reconcile_inflight` or dedicated) that deletes `pending/`+`inflight/` `*.mp4` with no matching `*.json` — shares the orphan-sweep idea with PS-04. Preserve the cross-filesystem copy+remove fallback at 93-101.
**Tests:** store.rs: `enqueue_removes_mp4_when_sidecar_write_fails` (inject a sidecar-write failure, assert no orphan `<id>.mp4`); if a sweep is added, `reconcile_sweeps_sidecarless_mp4`.

---

# Queue policy & eviction

## PS-03 — `max_clips = 0` (unvalidated) silently bricks the queue
**Severity:** High · **Effort:** M · **Confidence:** confirmed · **Status:** done
**Files:** `crates/perchstation-core/src/queue/policy.rs:140-145`; `crates/perchstation-core/src/config.rs:202-208`
**Depends on:** PS-08 (shared config-validation pass)

```rust
let (mut clips, mut bytes) = count_queue(store)?;
let needs_eviction =
    clips + 1 > policy.max_clips || bytes.saturating_add(incoming_bytes) > policy.max_bytes;
if !needs_eviction { return Ok(()); }
```

**Problem:** `apply_policy` uses `clips + 1 > policy.max_clips` (policy.rs:142) but nothing validates `max_clips >= 1` / `max_bytes > 0`. `Config::ensure_runtime_ready` (config.rs:202-208) checks only `perchpub_url`.
**Trigger:** Operator config `queue.max_clips = 0` (or `max_bytes = 0`). Every submit: `0 + 1 > 0` is always true → under `RefuseNew` every clip is `QueueFull`; under `DropOldestUndelivered` it evicts everything then still returns `QueueFull`. The station records nothing and emits no startup error.
**Fix:** Reject `max_clips < 1` and `max_bytes == 0` in the config-validation pass (this is the same pass owned by PS-08 — do them together). Add a typed `ConfigError` variant (e.g. `OutOfRange { field, reason }`). Keep the `status` subcommand path able to skip/independently surface this (it intentionally tolerates a missing `perchpub_url`).
**Tests:** config.rs: `ensure_runtime_ready_rejects_zero_max_clips`, `..._zero_max_bytes` (valid `perchpub_url` + offending bound → new error variant).

## PS-05 — eviction counts un-evictable inflight/delivered → drops fresh clips yet still returns `QueueFull`
**Severity:** High · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/queue/policy.rs:107-128` (count), `:154-193` (loop), `:211-239` (candidates)
**Depends on:** PS-20, PS-21 (same `count_queue`/loop)

```rust
for dir in [store.pending_dir(), store.inflight_dir(), store.delivered_dir()] {
    ...
    clips = clips.saturating_add(1);
    bytes = bytes.saturating_add(sidecar.byte_size);
}
```

**Problem:** `count_queue` (110) sums clips/bytes across **pending + inflight + delivered**, but `enumerate_evictable` (211-239) only yields `delivered/Undeliverable` and `pending/` entries. The loop decrements `clips` per evicted candidate (177), so `inflight/` and `delivered/Delivered` entries are counted toward the ceiling but can never be evicted to satisfy it.
**Trigger:** A `delivered/Delivered` (or `inflight/`) backlog pushes the census near/over `max_clips`. A fresh clip under `DropOldestUndelivered`: the loop evicts every Undeliverable then every `pending/` entry (**deleting fresh clips**) but `clips` never falls below the ceiling, so `candidates.pop_front()` returns `None` and `apply_policy` returns `QueueFull` anyway — fresh clips destroyed *and* submission refused.
**Fix:** Make the census agree with the evictable set. Either (a) `count_queue` returns only the evictable population, with a separate fixed baseline for the un-evictable floor (inflight + delivered/Delivered) that the ceiling comparison accounts for; or (b) terminate the loop against `evictable_clips_remaining`, and surface `QueueFull` **early — before deleting any pending clip** — when the un-evictable floor alone exceeds `max_clips`/`max_bytes`. Don't delete fresh pending clips you can't possibly free enough by evicting. Preserve Undeliverable-before-pending order and the `QUEUE_EVICTED` event. Coordinate with PS-20/PS-21.
**Tests:** `tests/integration/queue_eviction.rs`: `delivered_backlog_does_not_destroy_fresh_pending_then_queuefull` — pre-populate `delivered/` with N `Delivered` (mp4-unlinked) sidecars near `max_clips` + one pending clip; submit a fresh clip; assert the existing pending clip is NOT deleted and submission succeeds or returns `QueueFull` without evicting un-freeable pending entries.

## PS-20 — phantom byte accounting: counts `byte_size` of delivered entries whose mp4 was unlinked
**Severity:** Low · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/queue/policy.rs:110-125`; `crates/perchstation-core/src/queue/store.rs:265-283`
**Depends on:** PS-05

**Problem:** `count_queue` sums `sidecar.byte_size` (policy.rs:124) for every sidecar including those in `delivered/`, but `transition_delivered` (store.rs:273) unlinks the mp4 before moving the sidecar to `delivered/`. So `delivered/` sidecars contribute bytes that are no longer on disk.
**Trigger:** Any `delivered/` backlog. The byte total overstates real usage by the sum of all delivered `byte_size`, so `max_bytes` triggers eviction (and `QueueFull`) earlier than real disk usage warrants; the loop also subtracts phantom bytes (178) when evicting a delivered/Undeliverable entry that has no mp4.
**Fix:** In `count_queue`, do not add `byte_size` for `delivered/` entries (they have no mp4 — invariant at store.rs:10 / enforced at 273). Branch on the directory (only `pending/` + `inflight/` contribute bytes) or on media presence. Keep counting delivered entries toward `clips` (the sidecar still occupies a slot) unless PS-05's redesign supersedes. `inflight/` entries **do** retain their mp4 — keep counting their bytes.
**Tests:** `tests/integration/queue_eviction.rs`: `delivered_bytes_not_counted_against_max_bytes` — `delivered/` Delivered sidecars (large `byte_size`, no mp4) totalling > `max_bytes`; submit a small pending clip; assert accepted.

## PS-21 — eviction-reason mislabel when both ceilings breach
**Severity:** Low · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/queue/policy.rs:170-188`

```rust
let reason = if clips + 1 > policy.max_clips {
    EvictionReason::MaxClipsExceeded
} else { EvictionReason::MaxBytesExceeded };
```

**Problem:** The eviction reason (170-174) reports `MaxClipsExceeded` whenever the clip ceiling is breached, even when byte pressure is the dominant/actual driver — mislabelling the `queue.evicted` event's `reason` field (183). Its stated purpose (policy.rs:47-48) is count-vs-bytes attribution.
**Trigger:** Queue over both ceilings (many large clips). Every eviction in the loop reports `reason=max_clips_exceeded` regardless of byte pressure → wrong telemetry.
**Fix:** Compute the reason from which ceiling each eviction actually relieves: report `MaxBytesExceeded` when `bytes.saturating_add(incoming_bytes) > max_bytes` is still true and only `MaxClipsExceeded` when clip count is the sole remaining breach; or add an `EvictionReason::BothExceeded` ("both_exceeded") variant + `as_str` arm (policy.rs:55-63). Coordinate with PS-05 if the loop condition is rewritten.
**Tests:** `tests/integration/queue_eviction.rs`: `eviction_reason_reflects_byte_pressure_when_both_breached` — small `max_clips` + small `max_bytes` both breached by large clips; capture the `queue.evicted` event (existing `CaptureBuffer`/`install_json_subscriber` helper) and assert `reason` matches the dominant ceiling.

---

# Delivery, retry & backoff

## PS-06 — a 200 with undecodable body / unknown `ClassifyTaskStatus` → Transient → re-upload / infinite poll
**Severity:** High · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/delivery/retry.rs:107-118`; `crates/perchstation-core/src/perchpub/types.rs:43-58`; `crates/perchstation-core/src/perchpub/client.rs:196-213`

```rust
// retry.rs classify_upload_error:
ClientError::Network { .. } | ClientError::Decode { .. } => FailureKind::Transient,
// types.rs: no #[serde(other)] catch-all
pub enum ClassifyTaskStatus { Prepared, Queued, Processing, Success, Failed }
// client.rs upload tail: a 200 whose body won't parse becomes Decode
response.json::<ClassifyTaskPublic>().await.map_err(|err| ClientError::Decode { .. })
```

**Problem:** `ClassifyTaskStatus` (types.rs:43-50) has no `#[serde(other)]`, so an unknown status string in a 200 body fails the whole `ClassifyTaskPublic` deserialize → `ClientError::Decode` (client.rs:209-212 upload, 237-240 poll). `classify_upload_error`/`classify_poll_error` (retry.rs:111,125-126) map `Decode → Transient`, conflating *successfully-stored-but-undecodable* with *retryable failure*.
**Trigger:** perchpub returns HTTP 200 with a malformed body or a status the station doesn't model (e.g. a future `Cancelled`). **Upload path:** `Transient` → `transition_back_to_pending` → re-upload an already-accepted clip + new classify task each retry until `per_clip_max_attempts`. **Poll path:** `Transient` → poller leaves the sidecar untouched and re-polls forever; even a parsed unknown status would never satisfy `is_terminal()` (types.rs:55-57), so `scan_non_terminal` (classify.rs:123) re-selects it indefinitely.
**Fix:**
1. types.rs: add `#[serde(other)] Unknown` to `ClassifyTaskStatus`. Keep `is_terminal()` returning `false` for `Unknown` so a genuinely-still-running unknown keeps polling, but bound the poll via a finite attempt/wallclock budget so a stuck-unknown can't loop forever.
2. client.rs upload path (209-212): a 200 whose body won't decode means the clip is **already stored** — it must NOT cause a re-upload. Either (a) a distinct `ClientError::UndecodableSuccess` that `classify_upload_error` maps to `Terminal` (mark delivered with unknown classify status), or (b) on a 200 parse a minimal subset (`id` + `object_name`) and default `status` to `Unknown`.
   (Note: `ClassifyTaskPublic.status` already has `#[serde(default)]` (types.rs:66) so a *missing* status defaults to `Prepared`; the gap is an *unknown value*.)
   Keep genuine network/connection-reset Decode-equivalents as Transient. On the poll path an undecodable 200 stays Transient but under a finite poll budget.
**Tests:** retry.rs: assert the new `UndecodableSuccess`/2xx-decode variant → `Terminal`. types.rs: `classify_status_unknown_deserialises` for `{"status":"Cancelled"}` → `Unknown`, `!is_terminal()`. client.rs (or wiremock/httptest): a 200 with malformed body and a 200 with unknown status → the new non-Transient variant / `Unknown` status, not `Decode`-as-Transient. runner.rs: a 200-but-undecodable upload transitions to delivered (no re-upload).

## PS-08 — no numeric config validation → downstream Duration/arithmetic panics
**Severity:** Medium · **Effort:** M · **Confidence:** confirmed · **Status:** done
**Files:** `crates/perchstation-core/src/config.rs:200-207`; `crates/perchstation-core/src/capture/recording.rs:50-58`; `crates/perchstation-core/src/capture/runner.rs:288-291`; `crates/perchstation-core/src/delivery/retry.rs:52-62,180-185`
**Depends on:** PS-03 (same missing-validation class)

```rust
// config.rs: only perchpub_url is checked
pub fn ensure_runtime_ready(&self) -> Result<(), ConfigError> {
    if self.perchpub_url.as_deref().is_none_or(str::is_empty) {
        return Err(ConfigError::MissingRequired { field: "perchpub_url" });
    }
    Ok(())
}
// retry.rs from_config: unchecked multiply
per_clip_max_wallclock: Duration::from_secs(cfg.per_clip_max_wallclock_hours * 3600),
// retry.rs base_delay: can overflow Duration
Duration::from_secs_f64(capped.max(0.0))
```

**Problem:** `ensure_runtime_ready` validates only `perchpub_url`; every numeric field deserializes raw with no range check, and several downstream sites do unchecked arithmetic: (1) recording.rs:57 `let outer = max_duration + hang_margin;` panics on `Duration` add overflow (`clip_duration_secs` + `hang_margin_secs` via runner.rs:290-291); (2) retry.rs:60 `per_clip_max_wallclock_hours * 3600` unchecked `u64` multiply; (3) retry.rs:184 `Duration::from_secs_f64` panics when the value (bounded only by `max_attempt_delay_secs`) exceeds Duration's range. PS-03 (`max_clips=0`) is the same class.
**Trigger:** A config (or corrupt/hand-edited TOML) with a huge numeric value reaches the math: debug panic (aborts the capture/delivery task) or release wrap to an absurd budget.
**Fix:** Add **one** validation pass routed through `serve`/`enroll` startup. Add `Config::validate(&self) -> Result<(), ConfigError>` (or extend `ensure_runtime_ready` and wire `serve.rs:45-53`, which currently re-checks `perchpub_url` inline and bypasses it). Add `ConfigError::OutOfRange { field, reason }`. Enforce: `clip_duration_secs` in `[1, 3600]` and `clip_duration_secs + hang_margin_secs` non-overflowing (`checked_add`); `hang_margin_secs <= 600`; `cooldown_secs`/`liveness_*_secs >= 1`; `per_clip_max_attempts >= 1`; `per_clip_max_wallclock_hours` capped (e.g. `<= 24*365`) so `*3600` can't overflow; `initial_delay_secs`/`max_attempt_delay_secs` bounded with `initial <= max`; `max_clips >= 1` / `max_bytes >= 1` (PS-03). Keep `status` tolerant of a missing `perchpub_url` (split numeric validation from the URL requirement). As defence-in-depth also harden recording.rs:57 (`checked_add`) and retry.rs:60 (`saturating_mul`), and clamp `capped` to finite/in-range before `from_secs_f64`.
**Tests:** config.rs: `validate_rejects_zero_max_clips`, `validate_rejects_clip_plus_margin_overflow`, `validate_rejects_huge_wallclock_hours`, `validate_rejects_huge_max_attempt_delay_secs`, positive `validate_accepts_research_r10_defaults`. retry.rs: `from_config_saturates_huge_wallclock`, `base_delay_clamps_huge_max_attempt_delay` (no panic). recording.rs: clean error rather than panic on overflowing margin if `checked_add` is added.
**Notes:** `multiplier`/`jitter_fraction` are hardcoded R-7 constants (retry.rs:31-34), not operator inputs.

## PS-11 — `apply_jitter` calls `chrono::Utc::now()` directly, bypassing the injected `Clock`
**Severity:** Medium · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/delivery/retry.rs:188-201` (jitter), `:144-165` (schedule)

```rust
fn apply_jitter(base: Duration, jitter_fraction: f64) -> Duration {
    if jitter_fraction <= 0.0 { return base; }
    let nanos = chrono::Utc::now().timestamp_subsec_nanos();   // bypasses injected Clock
    let normalized = (f64::from(nanos) / 500_000_000.0) - 1.0;
    ...
}
```

**Problem:** `apply_jitter` (197) seeds its PRNG from `chrono::Utc::now()` directly even though `schedule()` (145-156) already holds the injected `clock: &dyn Clock`. Violates the `Clock` contract (hw_traits.rs:47-51: backoff logic "depends on this trait rather than calling `chrono::Utc::now` directly").
**Trigger:** A `Clock` with coarse subsecond resolution, or a sweep of clips failing in the same backoff tick: `schedule()` is called for many entries with the same injected `now`, but `apply_jitter` pulls live nanos that are identical/near-identical across the batch → all clips reschedule to the same `next_attempt_after` → synchronized retry storm. Also makes jitter untestable with a `FakeClock` (the existing `jitter_stays_inside_pm_20_percent` test only passes because it `sleep`s 1ms between real-clock samples).
**Fix:** Thread the clock's `now` into `apply_jitter` (signature `apply_jitter(base, jitter_fraction, now)`, derive `nanos` from `now.timestamp_subsec_nanos()`), or better accept an explicit seed/RNG. Since coarse clocks give identical nanos across a batch, mix in a per-entry value (hash `clip_id` + attempt) so same-tick clips get well-distributed deterministic jitter. Update `schedule()` (165) to pass it; fix the stale module doc (18-20) / inline comment (192-196). Keep the `jitter_fraction <= 0.0` early-return.
**Tests:** retry.rs: `jitter_is_deterministic_under_fake_clock`; rewrite `jitter_stays_inside_pm_20_percent` to drop `std::thread::sleep`; `jitter_differs_across_entries_same_tick` once a per-entry seed is threaded.

## PS-12 — disk-full backoff defaults to 1 hour, stalling all delivery on transient ENOSPC
**Severity:** Medium · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/delivery/runner.rs:92-96,116-123`

```rust
let disk_full_backoff = self.schedule.max_attempt_delay;   // = 3600s default
...
Err(RunnerError::Queue(QueueError::DiskFull { path })) => { ... sleep(disk_full_backoff).await; }
```

**Problem:** `disk_full_backoff` is set to `self.schedule.max_attempt_delay` (96), the upload-retry ceiling; its default (config.rs:222-224, `max_attempt_delay_secs=3600`; mirrored at retry.rs:231) is one hour, so a transient ENOSPC stalls the entire delivery loop for up to an hour.
**Trigger:** Any queue write hits `StorageFull` → `QueueError::DiskFull` → the run() arm sleeps up to 3600s; even if eviction/operator frees space seconds later, all delivery is paused for the full hour. Couples disk-full recovery latency to an unrelated upload-backoff ceiling.
**Fix:** Introduce a short dedicated floor, e.g. `const DISK_FULL_RETRY: Duration = Duration::from_secs(5)` (optionally a config knob), and replace `let disk_full_backoff = self.schedule.max_attempt_delay;` (96) with it. Keep the `queue.disk_full` event. Optionally `min(DISK_FULL_RETRY, max_attempt_delay)` so a deliberately tiny `max_attempt_delay` isn't overridden upward. Must be `> 0`.
**Tests:** runner.rs: `disk_full_backs_off_for_short_floor_not_max_delay` — `max_attempt_delay=3600s`, force `DiskFull`, assert the chosen backoff is the short floor. Factor the duration into a const/helper if timing assertions are awkward.

## PS-25 — `delivered/` Delivered entries never pruned → unbounded growth re-scanned every tick
**Severity:** Medium · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/delivery/classify.rs:101-129`; `crates/perchstation-core/src/queue/policy.rs:209-220`

**Problem:** The eviction policy (policy.rs:209-220) only treats `delivered/Undeliverable` as evictable; `Delivered` entries are never pruned. `delivered/` grows without bound, and `scan_non_terminal` (classify.rs:101-129) re-reads + re-parses every `delivered/*.json` every poll tick — O(n) `fs::read` + serde per tick, growing forever on a Pi.
**Trigger:** Normal steady state: every delivered clip leaves a permanent `delivered/<id>.json`. Over weeks the dir holds thousands; each `ClassifyPoller` tick (5–50ms) reads + parses all of them just to filter out terminal ones (123-125). Amplifies PS-02 (more files → higher corrupt-odds) and PS-26/PS-27 cost.
**Fix:** (1) Prune/age-out terminal delivered entries: once a clip's classify task is terminal (classify.rs poll_one terminal branch ~140-150) or `classify_lost_at` is set, the sidecar is no longer needed — delete after a configurable retention window or move to `archive/`. Add `QueueStore::prune_delivered(before)` or extend the eviction policy to age out `delivered/Delivered`+terminal-classify sidecars (mirror the `DeliveredUndeliverable` handling, gated on a retention age from `delivered_at`). (2) Avoid the full re-scan: track still-pollable `clip_id`s in memory on `ClassifyPoller` (populate from `delivered/` at startup, maintain incrementally as `transition_delivered` adds and `poll_one` reaches terminal). Never prune a non-terminal classify. Coordinate the in-memory set with PS-02's skip-and-warn dedup.
**Tests:** policy.rs: `delivered_terminal_entries_are_aged_out` (old `delivered_at` + terminal status → prune target after the window; a fresh terminal entry is NOT pruned). classify.rs: `scan_skips_already_terminal_without_rereading` (or, after the in-memory-set refactor, a terminal sidecar isn't re-read on later ticks). Check pruning doesn't race the poller mid-poll.

## PS-27 — `apply_policy` runs synchronous all-sidecar reads inline on the reactor
**Severity:** Medium · **Effort:** M · **Confidence:** plausible · **Status:** todo
**Files:** `crates/perchstation-core/src/queue/policy.rs:93-103,107-128`
**Depends on:** PS-05, PS-20 (they reshape `count_queue`)

```rust
apply_policy(&self.store, self.policy, incoming_bytes)?;   // blocking, on the reactor
self.inner.submit(clip_path, meta).await
```

**Problem:** `PolicyInbox::submit` (100) calls `apply_policy` inline — not `spawn_blocking` — inside an async fn. `apply_policy → count_queue` does synchronous `fs::read_dir` + `fs::read` + `serde_json::from_slice` over every sidecar in all three dirs (110-125), and on eviction `enumerate_evictable` re-reads `pending/`+`delivered/` (215,225). This blocking I/O runs on the tokio worker thread on every submit.
**Trigger:** At the 500-entry default ceiling, every captured clip triggers ~500+ synchronous reads + parses on the reactor before the await. On a Pi's slow SD card under a backlog this stalls the runtime, delaying delivery/classify polling and other tasks.
**Fix:** Wrap the preflight in `tokio::task::spawn_blocking` (clone the `QueueStore` — it's `Clone` — and `Copy` the `QueuePolicy`), await the JoinHandle, propagate `InboxError`. **Caveat:** the inline comment (95-99) chose inline execution for tracing-scope reasons — `spawn_blocking` does **not** inherit the caller's `DefaultGuard` subscriber, so `queue.evicted` events emitted inside `apply_policy` would no longer reach a test's scoped subscriber. Either switch those tests to a global subscriber or emit the events from the async side after the blocking call returns the eviction list. Larger alternative: cached atomic counters (`clip_count`, `byte_total`) on `QueueStore` adjusted on enqueue/evict/transition so no directory scan is needed. Coordinate with PS-05/PS-20.
**Tests:** A `#[tokio::test(flavor="multi_thread")]` asserting a concurrent lightweight task makes progress while a submit against a large pre-populated queue is in flight. Adapt `tests/integration/queue_eviction.rs` event-capture to a global subscriber if events move off the blocking thread.

---

# Concurrency & task lifecycle

## PS-09 — `spawn_supervised` nests `tokio::spawn`; aborting the handle detaches the worker
**Severity:** Medium · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/supervision.rs:39-60`
**Depends on:** PS-12 (soft — shared cancellable-loop refactor)

```rust
tokio::spawn(async move {
    let inner = tokio::spawn(fut);   // nested — returned handle is the OUTER one
    if let Err(err) = inner.await
        && err.is_panic() { ... }
})
```

**Problem:** `spawn_supervised` wraps the work future in a **nested** `tokio::spawn` (44) and returns the **outer** task's handle (43). Aborting the returned handle cancels the outer task while it's parked at `inner.await` (48), which **drops** the inner `JoinHandle` — and dropping a tokio `JoinHandle` **detaches** (does not abort) the task, so the worker keeps running unsupervised.
**Trigger:** `crates/perchstation/src/commands/serve.rs:158-159` calls `delivery_handle.abort()` / `classify_handle.abort()` on the outer handles (neither runner gets a `CancellationToken`, so abort is the only stop mechanism). The abort detaches the inner worker, which keeps holding the `QueueStore`, polling perchpub, and touching the on-disk queue after `serve::run` returns `Ok`. (Capture is unaffected — it gets a `CancellationToken` + 2s drain at serve.rs:138/165.)
**Fix:** Eliminate the nested spawn. **Preferred:** spawn the worker once and catch panics in-place via `futures::FutureExt::catch_unwind` (with `AssertUnwindSafe`); on a caught panic emit the same `service.task_panicked` event. The returned handle is then the only task, so `abort()` actually stops it. **Alternative (lower risk):** give the worker a `CancellationToken`, add a `select!` cancellation arm to `DeliveryRunner::run` and `ClassifyPoller::run`, pass tokens from `serve.rs` (mirroring capture), and keep `abort()` only as a post-drain backstop. Fix the misleading docs (19-24, 45-47). The token route shares the cancellable-sleep refactor with PS-12.
**Tests:** supervision.rs: `abort_actually_stops_inner_worker` — supervise a future that increments an `Arc<AtomicUsize>` in a loop; abort + await the handle; assert the counter stopped advancing (current code fails). Keep the existing panic-isolation / clean-exit tests passing. If the token route is chosen, add cancellation-arm tests to the runner/poller loops.

## PS-10 — eviction races `transition_inflight` on `pending/` with no lock
**Severity:** Medium · **Effort:** L · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/queue/policy.rs:266-286`; `crates/perchstation-core/src/queue/store.rs:151-187`

**Problem:** `evict()` (policy.rs:266-286) runs `fs::remove_file` on `pending/<id>.{mp4,json}` inline on the **capture** task, while the **delivery** task concurrently calls `transition_inflight` (store.rs:151-187) doing `fs::rename` on the same `pending/<id>.mp4`. Neither `PolicyInbox` nor `QueueStore` holds a lock.
**Trigger:** Capture evicts the lexicographic head at the same moment the runner picks that head. Interleavings: (a) evict removes the mp4 after `pick_oldest_pending` but before `transition_inflight`'s `exists()` check (store.rs:162) → spurious `MissingMedia`; (b) evict removes the mp4 between the `exists()` check and the `fs::rename` (175) → rename fails with `QueueError::Io`; (c) torn pair → `count_queue` accounting drift.
**Fix:** Introduce a queue-level mutex (e.g. `Arc<tokio::sync::Mutex<()>>` owned by `QueueStore`, shared by `PolicyInbox` and the runner) serialising `pending/` mutations across the `apply_policy`/`transition` critical sections. Alternatively, have eviction skip the current delivery head (thread the in-flight `clip_id` into `apply_policy`), and make `transition_inflight` tolerate the rename-vs-unlink race (treat `NotFound` on the rename as it already does for `remove_file`). Preserve existing idempotent `NotFound` handling.
**Tests:** `tests/integration/queue_eviction_race.rs` (new): stage one pending clip that is both the eviction head and delivery head; spawn `apply_policy/evict` and `transition_inflight` concurrently under repeated-iteration stress; assert no spurious `MissingMedia`/`Io` escapes and the final on-disk state is consistent (mp4 in exactly one of pending/inflight, sidecar count correct).

---

# Perchpub wire client

## PS-16 — response body read with no size cap → OOM on a Pi
**Severity:** Medium · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/perchpub/client.rs:129-142,197-213,225-241`

```rust
let message = response.text().await.unwrap_or_default();          // error path, unbounded
response.json::<ClassifyTaskPublic>().await.map_err(...)          // success path, unbounded
```

**Problem:** Both the error path (`response.text()` at 200/228) and the success path (`response.json()` at 209-212/237-240) buffer the entire body into RAM with no upper bound. `Client::builder` (129-142) sets `.timeout(Duration::from_mins(1))` but no body-size limit, and reqwest has no global cap.
**Trigger:** A buggy/compromised perchpub (still CA-pinned, so it passes mTLS) responds to `POST /api/v1/upload/` or `GET /api/v1/classify-task/{id}` with a multi-GB body (large JSON, or a huge error body trickling in under the 1-minute timeout). On a Pi (~512MB-1GB RAM) the buffered allocation OOM-kills the process.
**Fix:** Cap the buffered body in `upload_clip` and `get_classify_task`. Replace bare `text()`/`json()` with a size-limited read: stream `bytes_stream()` (or loop `chunk()`) into a `Vec<u8>` and bail with a `ClientError` once length exceeds a small ceiling (`const MAX_RESPONSE_BYTES` ~1 MiB — `ClassifyTaskPublic`/`HTTPValidationError` are tiny). Defence-in-depth: reject when `content_length()` reports above the ceiling before reading. Then `serde_json::from_slice` (success) / lossy-decode (error). Preserve: run `parse_retry_after(response.headers())` before consuming the body (already done at 199/227); an over-limit success body → `ClientError::Decode` (transient), not a panic.
**Tests:** Stand up a loopback fake perchpub with a cert chaining to the test CA from `write_credentials`; (1) endpoint returns a body larger than the cap → error (Decode/new TooLarge) without OOM; (2) a normal small 200 `ClassifyTaskPublic` still decodes. If a TLS fake is too heavy, factor a `read_capped(response, max) -> Result<Vec<u8>, ClientError>` free function and unit-test it.

## PS-18 — `PerchpubClient` caches TLS identity for the process lifetime; re-enrollment needs a serve restart
**Severity:** Low · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/perchpub/client.rs:88-154`

**Problem:** `PerchpubClient::new` reads `station.crt`/`station.key`/`ca_chain.pem` exactly once and bakes them into the `reqwest::Client` (`inner`), which caches the TLS identity + root store for its lifetime. No `reload()`, no filesystem watch; the struct is `Clone` (Arc) so clones share the stale identity.
**Trigger:** An operator re-enrolls (`identity::save` overwrites `credentials/`) while `serve` is running. The long-running `DeliveryRunner`/`ClassifyPoller` keep presenting the old client cert / validating against the old CA until restart. If the old cert was rotated/revoked, every upload silently fails mTLS (surfacing as `Network` → Transient) and clips back up with no clear cause.
**Fix:** (A) **Document + enforce restart** (pragmatic minimum): note in the `PerchpubClient` doc and at the `serve.rs` construction site that re-enrollment requires a `serve` restart, and have `enroll` instruct the operator to restart. (B) **Hot reload** (L upgrade): extract the load+builder body into `build_inner(data_dir, base_url)`, store `inner` behind `arc_swap::ArcSwap<Client>` (or `RwLock`), add `pub fn reload(&self) -> Result<(), ClientError>` that rebuilds and swaps; have `serve` watch `credentials/` (notify) or reload on SIGHUP. Keep `base_url`/`authority()` stable; a failed reload must leave the previous working client in place.
**Tests:** (A) doc/comment + an entry in `deploy/RELEASE-CHECKLIST.md`. (B) client.rs: `write_credentials`, build, capture `authority()`, rewrite credentials with a fresh CA/leaf, `reload()` → `Ok` and `authority()` unchanged; negative `reload()` against an empty `ca_chain.pem` → `Err(TlsConfig)` without poisoning the existing client.

## PS-22 — 2xx-other status (201/202/204) collapsed to Terminal → clip wrongly marked `Undeliverable`
**Severity:** Low · **Effort:** S · **Confidence:** plausible · **Status:** todo
**Files:** `crates/perchstation-core/src/perchpub/client.rs:197-207,225-235`; `crates/perchstation-core/src/delivery/retry.rs:129-134`

```rust
let status = response.status();
if status != StatusCode::OK { ... return Err(ClientError::Http { status: status.as_u16(), .. }); }
```

**Problem:** `upload_clip` (198) and `get_classify_task` (226) treat every status other than exactly `200` as a failure via `ClientError::Http`; `classify_status` (retry.rs:129-134) maps any non-listed code through `_ => Terminal`, so a 2xx-other success (201/202/204) → `Undeliverable`.
**Trigger:** perchpub (or its Traefik front / a future API revision) responds to a successful upload with 201/202. The clip was accepted but is recorded `Http{201}` → Terminal → `Undeliverable`, permanently dropping it. Per `specs/001-clip-delivery/contracts/perchpub-api.md` only 200 is contracted for success, so 2xx-other is undefined today — hence plausible.
**Fix:** Replace `if status != StatusCode::OK` with `if !status.is_success()` in both methods so any 2xx falls through to body decode (a 204 empty body would then fail `json()` → `Decode`/Transient, acceptable). Alternatively, if the contract is amended to mandate strict-200, add a comment explaining the intentional non-200=error gate and downgrade this finding.
**Tests:** retry.rs: assert a 2xx reaching `classify_status` is not Terminal once loosened. Integration (loopback fake perchpub, see PS-16): `/api/v1/upload/` returns 201 + valid body → `upload_clip` returns `Ok`.

## PS-23 — `Retry-After` HTTP-date form dropped → server backoff floor lost on 429/503
**Severity:** Low · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/perchpub/client.rs:267-273`
**Depends on:** PS-11 (Clock injection)

```rust
fn parse_retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;   // delta-seconds only
    Some(Duration::from_secs(secs))
}
```

**Problem:** Only the delta-seconds form is parsed; the RFC-7231 HTTP-date form (`Wed, 21 Oct 2015 07:28:00 GMT`) fails `parse::<u64>()` and returns `None`, silently discarding the server's backoff floor.
**Trigger:** perchpub or any upstream proxy emits `Retry-After: <HTTP-date>` on a 429/503. `retry_after` becomes `None`, the scheduler ignores the server's instruction and uses only local exponential backoff — retrying sooner than asked and worsening rate-limiting.
**Fix:** Keep the delta-seconds fast path; on failure parse with `chrono::DateTime::parse_from_rfc2822(raw.trim())` (IMF-fixdate is RFC-2822-compatible), convert to UTC, subtract `now`, return `Some(delta.to_std().unwrap_or(Duration::ZERO))` clamping negatives to zero. Note this free function has no `Clock` in scope — either document the direct `Utc::now` or thread a clock (coordinate with PS-11). Update the doc comment at 267-268 (currently "HTTP-date form is not supported").
**Tests:** client.rs: a `HeaderMap` with `RETRY_AFTER` = `"120"` → `Some(120s)`; a future HTTP-date → `Some(~delta)`; a past date → `Some(0)`; garbage → `None`; absent → `None`. Pin a fake clock if one is threaded.

---

# Enrollment & identity (security)

## PS-13 — `validate_chain` accepts expired / non-CA certs
**Severity:** High · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/enrollment/confirm.rs:255-303`
**Depends on:** PS-11 (inject a clock, don't call `Utc::now` directly)

```rust
match leaf.verify_signature(Some(ca.public_key())) {
    Ok(()) => return Ok(()),
    Err(err) => last_err = Some(format!("verify against pinned CA: {err}")),
}
```

**Problem:** `validate_chain` (255-303) accepts the perchpub-issued leaf as soon as its signature verifies against any pinned QR CA cert (leaf.verify_signature at 295). It never checks the leaf's `not_before`/`not_after`, nor that the pinned issuer is actually a CA (`BasicConstraints cA=TRUE` + `keyCertSign`). A correctly-signed but expired/not-yet-valid leaf — or a leaf signed by a non-CA pinned cert — passes and is handed to `identity::save` to persist and trust.
**Trigger:** perchpub returns `success=true` with a leaf whose validity is past/future, or signed by a non-CA pinned cert. `validate_chain` returns `Ok`, `identity::save` persists `station.crt`, then `cert_is_expired(now)` returns true on every upload preflight → the delivery loop never sends a clip; enrollment "succeeds" but the station is dead on arrival.
**Fix:** In `validate_chain`, after parsing `leaf` (~284), against an injected `now: DateTime<Utc>` (thread the `Clock` through `send`/`validate_response`; don't call `Utc::now()` — see PS-11): (1) reject if `now < not_before` or `now > not_after` (read `leaf.tbs_certificate.validity.*`, convert via `.timestamp()` + `Utc.timestamp_opt` as identity.rs:264-273 does) with new `ConfirmError::CertExpired`/`CertNotYetValid`; (2) for each pinned `ca`, require `ca.tbs_certificate.is_ca()` (or `basic_constraints().value.ca`) and `key_usage().value.key_cert_sign()`, skipping non-CA pinned certs rather than letting them verify a leaf. Add the variants to `ConfirmError` (38-62). Preserve iterating all pinned CAs and the `ChainMismatch` behaviour; handle not-representable timestamps as a rejection, not a panic.
**Tests:** confirm.rs `mod tests` (312): extend `build_ca()`/`sign_csr()` to mint leaves with `not_after` in the past / `not_before` in the future and a non-CA issuer (`IsCa::NoCa`); `validate_response_rejects_expired_leaf`, `..._not_yet_valid_leaf`, `..._leaf_signed_by_non_ca`. Thread a fixed `now` into `validate_response`; have the happy-path test pass a `now` inside the cert window.
**Notes:** `x509-parser` is 0.16; identity.rs:264-273 shows the validity-timestamp parsing pattern. `cert_is_expired` (identity.rs:110-112, see PS-19) is what bricks uploads once an expired leaf is persisted.

## PS-14 — enrollment client follows redirects → re-sends `auth_token`+CSR
**Severity:** Medium · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/enrollment/confirm.rs:195-217`

```rust
let mut builder = reqwest::Client::builder()
    .use_rustls_tls()
    .tls_built_in_root_certs(false)
    .min_tls_version(reqwest::tls::Version::TLS_1_2)
    .https_only(true)
    .timeout(Duration::from_secs(30));   // no .redirect(Policy::none())
```

**Problem:** `build_client` (195-217) builds the pre-enrollment HTTPS client without `.redirect(Policy::none())`. reqwest's default follows up to 10 redirects; on 307/308 it preserves method + body, so the confirm POST's bearer `auth_token` and CSR are re-sent to whatever host `Location` names. `https_only(true)` constrains the scheme but **not** the host. The sibling mTLS client already sets `Policy::none()` at client.rs:141.
**Trigger:** A malicious/compromised perchpub front (or an injected 3xx) returns 307/308 with `Location` at attacker.example.org; reqwest auto-follows, re-POSTing `{ auth_token, csr_pem }` to the attacker, leaking the one-time enrollment token + CSR. The transient-3xx handling at attempt_once (190-192) never runs because reqwest swallows the redirect — its own comment confirms the gap.
**Fix:** Add `.redirect(reqwest::redirect::Policy::none())` to the builder (207-212), matching client.rs:141. Then in `attempt_once` change the trailing catch-all (190-192) so a real 3xx surfaces as a terminal rejectable error (`ConfirmError::UnexpectedRedirect`/`ServerRejected`), not `Transient` (retrying a redirect is pointless). Update the now-stale comment.
**Tests:** confirm.rs `mod tests`: `build_client` succeeds; a local mock HTTPS server (pinned self-signed CA) returns 307 with `Location` to a second host — assert the second host never receives a request and `send_with_backoff` returns terminal, not a leaked POST. Minimum: assert the builder is configured with `Policy::none()` and an `attempt_once`-level test that a synthetic 3xx classifies Terminal.

## PS-15 — untrusted QR images decoded with no size/pixel limits → decompression-bomb OOM
**Severity:** Medium · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/enrollment/file_source.rs:54-60`; `crates/perchstation-core/src/enrollment/mod.rs:72-84`

```rust
let img = image::load_from_memory(&bytes)   // no image::Limits at all
    .map_err(|source| FileQrError::Image { path: self.path.clone(), source })?;
Ok(img.into_luma8())
// mod.rs:
rqrr::PreparedImage::prepare_from_greyscale(width as usize, height as usize, |x, y| ...)  // width*height alloc
```

**Problem:** `FileQrSource::load` decodes an operator-supplied PNG/JPEG with bare `image::load_from_memory`, which installs **no** `image::Limits`. The pixels are then allocated again in `decode_enrollment_session` via `prepare_from_greyscale(width, height, ...)`. Nothing rejects absurd declared dimensions before allocation.
**Trigger:** `perchstation enroll --qr-source=file --qr-file <png>` (recovery/phone-photo path, semi-trusted). A few-KB PNG whose header declares e.g. 60000×60000 (~3.6 GB luma) is decoded with no cap → OOM-kills the process; a successfully-decoded huge image triggers a second `width*height` alloc in `prepare_from_greyscale`.
**Fix:** Replace `image::load_from_memory(&bytes)` (file_source.rs:57) with a limited reader: `let mut reader = image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?; reader.limits(limits);` where `limits` is an `image::Limits` with explicit `max_image_width`/`max_image_height` (e.g. 8192) and a tightened `max_alloc` — **both dimension caps default to `None`, so they must be set explicitly**; then `reader.decode()?.into_luma8()`. Map the new `with_guessed_format` io error + `ImageError::Limits` into `FileQrError::Image` so it surfaces as `QrFrameError::Decode` (preserve the Io-vs-Decode split). Defence-in-depth: in `decode_enrollment_session` (mod.rs:75) reject `image.dimensions()` above the same cap before `prepare_from_greyscale`, via a new `QrDecodeError::FrameTooLarge`. Share the cap as a named const (must exceed the 200×200/300×300 test fixtures).
**Tests:** file_source.rs: `rejects_oversized_image_as_decode_error` (header-only byte stream declaring oversized dims → `QrFrameError::Decode`, no OOM); existing `surfaces_non_image_as_decode_error` still passes. mod.rs: `decode_rejects_oversized_frame`; existing 200×200/300×300 fixtures still decode.
**Notes:** image 0.25 — the type is `image::ImageReader` (the brief's `image::io::Limits`/`Reader.limits()` is the 0.24 spelling). `Limits::default()` sets `max_alloc=Some(512 MiB)` but `max_image_width/height=None`, and bare `load_from_memory` installs none.

## PS-17 — no parent-dir fsync after credentials rename
**Severity:** Medium · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/identity.rs:172-240,242-253,152-171`
**Depends on:** PS-28 (shared unix-cfg strategy for opening a dir)

```rust
if let Err(source) = fs::rename(&staging_path, &creds_path) {   // dir entry never fsync'd
    if creds_existed { let _ = fs::rename(&rotated_path, &creds_path); }
    return Err(IdentityError::Io { path: creds_path, source });
}
```

**Problem:** `save()` calls `file.sync_all()` per file in `write_mode` (251), but the directory-entry change from `fs::rename(&staging_path, &creds_path)` (222) is never made durable — the parent `data_dir` is never fsync'd. File contents are on disk but the rename may still be in the page cache.
**Trigger:** `perchstation enroll`; `save()` returns `Ok`; a power cut on the field Pi before the FS flushes `data_dir`. After reboot the rename can be lost (and `credentials.tmp`/`.old` left inconsistent), so the just-enrolled identity disappears — silently, since `save()` reported success.
**Fix:** After the successful rename at 222 (and ideally the rotation rename at 216), open the parent dir and `sync_all()` it. Add `fn fsync_dir(path) -> Result<(), IdentityError>` doing `File::open(path)?.sync_all()` and call it on `data_dir` before the `StationIdentity::load` at 239. Belt-and-braces: also fsync `&staging_path` after the writes (after 211). Update the doc (152-171) to document the dir-fsync step. The dir-open is unix-flavoured — keep cfg-gating consistent with PS-28.
**Tests:** identity.rs: `save_fsyncs_parent_dir_after_rename` (at minimum asserts save still succeeds + files round-trip). Extract the helper and add `fsync_dir_succeeds_on_existing_dir` / `fsync_dir_errors_on_missing_path`.

## PS-19 — `cert_is_expired` uses strict `<` (boundary off-by-one at `not_after` second)
**Severity:** Low · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/identity.rs:107-113,263-274`

```rust
pub fn cert_is_expired(&self, now: DateTime<Utc>) -> bool {
    self.cert_not_after < now
}
```

**Problem:** Strict `<` (111) means at the exact `not_after` second the cert is still treated as valid. `parse_cert_not_after` truncates to whole seconds (272-273), so the boundary second is a real, reachable instant.
**Trigger:** `now == self.cert_not_after` exactly: `status` and the per-upload preflight treat the cert as valid at the instant it expires; an upload then can be rejected by perchpub as expired → confusing local-vs-server mismatch.
**Fix:** Change 111 to `self.cert_not_after <= now` (conservative pre-expiry halt, matching the whole-second truncation). Update the doc (107-108, "strictly in the past"). Optionally add a small safety margin (`<= now + margin`) threaded as a param/const without breaking call sites.
**Tests:** identity.rs: `cert_is_expired_at_exact_not_after_second_is_true`, plus `not_after - 1s == false` and `not_after + 1s == true`.

---

# Observability

## PS-26 — `RedactingWriter` clones the secrets `Vec` on every log line
**Severity:** Low · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/observability/tracing.rs:298-310,231-238`

```rust
fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    let secrets = self.registry.snapshot();   // locks + clones Vec<String> every line
    let mut stderr = io::stderr().lock();
    if secrets.is_empty() { stderr.write_all(buf)?; return Ok(buf.len()); }
```

**Problem:** `RedactingWriter::write` calls `self.registry.snapshot()` unconditionally at the top of every write; `snapshot()` (235-238) locks the secrets `Mutex` and clones the entire `Vec<String>`. The tracing fmt layer emits one `write()` per log line, so every line locks + heap-clones all secrets even when none are registered (the common steady-state once enrollment is done).
**Trigger:** Every emitted log line in steady-state `serve`: one `Mutex` lock + full `Vec<String>` clone, with zero benefit when the registry is empty.
**Fix:** Short-circuit the empty case before locking/cloning. Add an `AtomicUsize` length to `RedactionRegistry` (alongside `secrets: Mutex<Vec<String>>`), bump it in `register()` only when a new secret is actually pushed (inside the dedup block), and have `write()` read it `Relaxed` first: if 0, take the fast path (`write_all(buf)`) **without** calling `snapshot()`. Only call `snapshot()`/`scrub()` when non-zero. Preserve: empty-string secrets are still ignored; the `from_utf8_lossy` + `scrub` path and `flush()` unchanged; keep `snapshot()`/`len()`/`is_empty()`/`contains_any()` public API. (Alternative: `arc_swap::ArcSwap<Vec<String>>` read-borrow — larger dep change, the AtomicUsize is preferred.)
**Tests:** tracing.rs `mod tests`: `empty_registry_write_passes_through_untouched`; `len_counter_tracks_register_dedup` (register twice + empty → count == 1); keep a scrub test proving non-empty still redacts. Assert via the public `scrub()` + counter rather than real stderr.

## PS-24 — `pick_last_failure` filter fragility (**STALE — not a live bug**)
**Severity:** Low · **Effort:** S · **Confidence:** plausible · **Status:** STALE (defensive hardening only)
**Files:** `crates/perchstation-core/src/observability/status.rs:304-318`; `crates/perchstation-core/src/delivery/runner.rs:176-195`

```rust
fn pick_last_failure(delivered: &[ClipQueueEntry]) -> Option<FailureSnapshot> {
    delivered.iter()
        .filter(|e| e.outcome == Some(Outcome::Undeliverable) || e.last_error.is_some())
        .filter(|e| e.delivered_at.is_some() || e.last_attempt_at.is_some())
```

**Problem:** `pick_last_failure` admits any `delivered/` entry whose `last_error.is_some()` regardless of outcome. **Verified not a live bug:** the success path clears the error — runner.rs:184 sets `delivered.last_error = None` before `transition_delivered`; `transition_inflight`/`reconcile_inflight` also clear it; the classify poller only touches `last_classify_status`/`classify_lost_at`. So no `Delivered` entry in `delivered/` ever carries `last_error`.
**Trigger:** Would only fire if a `Delivered` entry had `last_error.is_some()` — no current code path produces that state.
**Fix (hardening, optional):** The filter trusts an invariant maintained elsewhere; if runner.rs:184 is ever dropped the latent bug reappears. Tighten the filter to `e.outcome == Some(Outcome::Undeliverable)` only and drop the `|| e.last_error.is_some()` disjunct (the `map_or_else` fallback at 312-315 already handles a missing error string).
**Tests:** If hardened: `pick_last_failure_ignores_delivered_with_stale_error` (entry with `Delivered` + `last_error=Some(..)` → returns None / skipped). No regression test needed for current behaviour (state unreachable).

---

# Conventions, portability & cleanup

## PS-28 — top-level `std::os::unix` import makes core non-portable
**Severity:** Cleanup · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/identity.rs:17-20,242-253`

```rust
use std::os::unix::fs::OpenOptionsExt;   // top-level, non-cfg-gated; used by .mode() at 246
```

**Problem:** A top-level (non-test, non-cfg-gated) `use std::os::unix::fs::OpenOptionsExt;` (19), used by `write_mode`'s `.mode(mode)` (246), makes `perchstation-core` fail to compile on non-unix targets — contradicting the root CLAUDE.md "platform-agnostic" description.
**Trigger:** `cargo check -p perchstation-core --target x86_64-pc-windows-msvc` (or any portability check) fails to compile.
**Fix:** Cfg-gate the unix-specific permission handling: move the import behind `#[cfg(unix)]` and either split `write_mode` into `#[cfg(unix)]` (calls `.mode(mode)`) and `#[cfg(not(unix))]` (drops the mode, keeps `create_new(true).write(true)`) variants, or apply the mode via a `#[cfg(unix)] { opts.mode(mode); }` block. Preserve the security-critical `0o600` for `station.key` on unix (the only production target). The `#[cfg(test)] use ...PermissionsExt` (348) is already correctly gated; gate the mode-assert test (`#[cfg(unix)]`) if `write_mode` becomes target-split. Coordinate with PS-17 (the dir-fsync helper also opens a dir).
**Tests:** No new behavioural test required; verify with a non-unix `cargo check`. If `write_mode` is split, gate `save_writes_all_four_files_with_correct_modes` (379-415) `#[cfg(unix)]`.

## PS-29 — hardware `sensor_*`/`camera_*` fields live in platform-agnostic core config
**Severity:** Cleanup · **Effort:** L · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/config.rs:109-145`
**Depends on:** PS-30 (the `/dev/gpiochip0` default rides along)

```rust
#[serde(default = "default_sensor_gpiochip")] pub sensor_gpiochip: PathBuf,
#[serde(default = "default_sensor_line")]     pub sensor_line: u32,
#[serde(default = "default_camera_width")]    pub camera_width: u32,  // etc.
```

**Problem:** `CaptureConfig` (118-145) holds hardware-specific fields (`sensor_gpiochip: PathBuf`, `sensor_line`, `sensor_active_high`, `camera_width/height/framerate`, `camera_bitrate_bps`) inside `perchstation-core`, which CLAUDE.md mandates be platform-agnostic. The struct's own doc (113-115) admits these are only consumed by the production adapters in `perchstation-hw`.
**Trigger:** Convention violation (no runtime crash): these fields are read only at `crates/perchstation/src/commands/serve.rs:193-220` to build the hw GPIO sensor/camera adapters; the agnostic Capture supervisor (runner.rs) never touches them.
**Fix:** Move the hardware knobs out of core. Either (a) an hw-owned config struct (`SensorHwConfig`/`CameraHwConfig`) in `perchstation-hw` deserializing its own subtable, with the binary owning the wiring; or (b) an opaque pass-through (`pub hardware: toml::Table`) that core never interprets and hw decodes. Keep only the agnostic supervisor knobs in core (`clip_duration_secs`, `hang_margin_secs`, `cooldown_secs`, `liveness_*_secs`, `max_staging_bytes`). Update serve.rs:193-220, remove the `default_sensor_*`/`default_camera_*` fns (PS-30 owns `default_sensor_gpiochip`), update `CaptureConfig::default`, the config tests, and `deploy/config.example.toml`. Preserve the TOML field names + defaults so operator configs keep parsing.
**Tests:** A parse round-trip test in `perchstation-hw` proving the relocated config deserializes the `sensor_*`/`camera_*` keys with prior defaults. In core config.rs, assert `CaptureConfig` no longer exposes the hardware fields. Verify `deploy/config.example.toml` still parses end-to-end.

## PS-30 — `/dev/gpiochip0` Linux device-node literal hardcoded as a core default
**Severity:** Cleanup · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/config.rs:262-264,131-132`
**Depends on:** PS-29 (clean fix is the shared relocation)

```rust
fn default_sensor_gpiochip() -> PathBuf { PathBuf::from("/dev/gpiochip0") }
```

**Problem:** `default_sensor_gpiochip` (262-264) hardcodes the Linux device node `/dev/gpiochip0` in core and wires it as the serde default for `sensor_gpiochip` (131-132), violating the CLAUDE.md rule that hardware lives only in `perchstation-hw`.
**Fix:** Move the default alongside the field when PS-29 relocates the hardware block — `/dev/gpiochip0` belongs with the hw adapter (cfg-gated to Linux). Delete `default_sensor_gpiochip` from config.rs once the field moves. If PS-29 is deferred, the minimal step is to remove the literal from core and require the hw layer to provide it.
**Tests:** Covered by PS-29's hw-side round-trip test. If fixed standalone, a `perchstation-hw` test for the relocated default + confirm core no longer references the literal.

## PS-31 — duplicated logic that should be centralised
**Severity:** Cleanup · **Effort:** M · **Confidence:** confirmed · **Status:** todo
**Files:** multi-file — `queue/store.rs:88-89,104,157-160,194,205-208,267-269,319-324`; `queue/policy.rs:107-128,245-262,272-273`; `observability/status.rs:230-264,281-284`; `perchpub/client.rs:114-147,187`; `enrollment/confirm.rs:195-217`; `capture/staging.rs:98-117`; `delivery/classify.rs:101-128`; `delivery/runner.rs:152`
**Depends on:** PS-02 (the shared sidecar reader funnels corrupt-handling through one place)

**Problem:** Four pieces of logic are hand-reimplemented across modules and already drift (different error types, `is_some_and` vs `is_none_or` extension filters): (a) clip filename construction `format!("{clip_id}.mp4")`/`.json`; (b) sidecar read (`fs::read` + `from_slice` + typed-error map); (c) the hardened reqwest rustls TLS builder + CA-PEM→`Certificate` loop; (d) directory byte/clip tallying.
**Trigger:** Maintenance hazard: a layout change (compression suffix, sidecar schema, TLS hardening, `.mp4` extension) must be applied in 8+ unsynchronised sites; missing one silently breaks only that path (e.g. `status` under-counts bytes, or one client omits `https_only`).
**Fix:**
- **(a)** Add `QueueStore::media_name(clip_id)`/`sidecar_name(clip_id)` (or per-dir path accessors) and route every `format!("{clip_id}.mp4")`/`.json` join through them — store.rs (88-89,104,157-160,194,205-208,267-269), policy.rs evict (272-273), runner.rs poll_one mp4_path (152), client.rs upload multipart filename (187). Keep the `.mp4.tmp` staging suffix as a distinct helper.
- **(b)** Make `store.rs read_sidecar` (319-324) the single reader; call it from status.rs `load_delivered` (281-284), policy.rs `count_queue`/`read_sidecars` (119-122,255-258), classify.rs `scan_non_terminal` (113-116). Preserve each module's error variant (return `QueueError` and map at the call site, or have the reader return the raw error for callers to wrap). Directly enables PS-02.
- **(c)** Extract the rustls builder shared by client.rs (114-147) and confirm.rs (195-217) into e.g. `rustls_builder_with_roots(ca_pem) -> Result<ClientBuilder, _>` (PEM→roots parse + empty-roots guard + the four hardening flags). Callers add their differences (client.rs: identity + `redirect(none)` + 1-min timeout; confirm.rs: no cert + 30s timeout — and once PS-14 lands, `redirect(none)` there too). Keep each crate's error type.
- **(d)** Unify `read_dir` + extension-filter + saturating-sum: status.rs `sum_mp4_bytes` (246-264), staging.rs `staging_bytes` (98-117), policy.rs `count_queue` (107-128), status.rs `count_sidecars` (230-244) into one parametrised scanner. Preserve each caller's `NotFound`-tolerance (status/staging → 0/empty; policy currently propagates — decide deliberately).
**Tests:** (a) `media_name`/`sidecar_name` match the on-disk convention + round-trip through transitions. (b) shared `read_sidecar` corrupt-JSON test + regression in classify.rs/status.rs that their own Parse variant still surfaces. (c) shared builder rejects empty CA PEM; existing `new_rejects_empty_ca_chain`/`build_client_rejects_empty_ca_chain` keep passing. (d) extend `staging_bytes` / `bytes_on_disk_sums_*` tests against the unified scanner + a `count_queue` mixed-dir tally test.

## PS-32 — dead crate-level `Error` enum + `Result` alias
**Severity:** Cleanup · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/lib.rs:15-33`

```rust
#[derive(Debug, Error)]
pub enum Error { Config(String), Io(#[from] std::io::Error), Enrollment(String), ... }
pub type Result<T> = std::result::Result<T, Error>;
```

**Problem:** lib.rs:17-33 define a crate-wide `pub enum Error` and `pub type Result<T>` that are completely unused — every module returns its own thiserror type; nothing constructs/returns `crate::Error`/`crate::Result`.
**Trigger:** Not a runtime fault; dead code misleads contributors into thinking there's a unifying error type and invites accidental coupling. The lone `use thiserror::Error;` (15) exists only for this dead enum.
**Fix:** Delete lib.rs:15-33 in full (the `use thiserror::Error;` import, the `pub enum Error` block 17-31, and the `pub type Result<T>` alias 33). Keep the `#![deny(...)]` attributes (1-2) and `pub mod` declarations (4-13). Re-confirm zero references first: `grep -rn 'crate::Error\|crate::Result\|perchstation_core::Error\|perchstation_core::Result' --include='*.rs' .` (already run: no matches; the `Error::Config` hits are `CommandError::Config` in the binary). `Error` is `pub` so this is technically a public-API removal, but the crate is consumed only internally — safe.
**Tests:** None (pure deletion). Verify via `cargo build/clippy/test --workspace` + the grep returning no hits.

## PS-33 — cooldown `last_outcome` is write-only; doc comment falsely claims it is surfaced on `capture.cooldown_skip`
**Severity:** Cleanup · **Effort:** S · **Confidence:** confirmed · **Status:** todo
**Files:** `crates/perchstation-core/src/capture/cooldown.rs:10-12,26,37-46,61-64`; `crates/perchstation-core/src/capture/runner.rs:230-238`

```rust
/// Informational — surfaced on `capture.cooldown_skip` so an operator can tell why...
pub enum CooldownOutcome { ... }
    last_outcome: Option<CooldownOutcome>,   // written, read only by tests
// runner.rs cooldown_skip emission emits ONLY cooldown_until:
tracing::debug!(event = ..CAPTURE_COOLDOWN_SKIP, cooldown_until = %until.to_rfc3339(), ...);
```

**Problem:** The `CooldownOutcome` doc (cooldown.rs:10-12) claims the outcome is "surfaced on `capture.cooldown_skip`", but the emission in runner.rs `handle_trigger` (232-236) logs only `cooldown_until`. The `last_outcome` field (26) is written by `start_after` (45) but read by nothing outside tests (getter 62-64 used only at tests 80/90/104).
**Trigger:** Not a runtime fault; an operator reading the doc expects the event to say *why* the loop is in cooldown, but the info is plumbed through every `start_cooldown` call site (runner.rs:249,269,333,345,357,382,401,413) yet silently dropped at the log site.
**Fix (pick one):** (A) **Make the doc true** — add a `cooldown_reason = o.as_str()` field to the `cooldown_skip` emission (230-238), requiring an `as_str()` on `CooldownOutcome` (snake_case wire form per `contracts/log-events.md`); keeps the field meaningful. (B) **Remove the dead state** (lower effort, recommended unless the diagnostic is wanted) — drop the `last_outcome` field (26), stop writing it in `start_after` (delete 45 + the `outcome` param at 41), delete the getter (61-64), rewrite the false doc (10-12), and update the 7 `start_cooldown` call sites + helper signature (418-420) + import (runner.rs:16) + cooldown.rs tests. Preserve `start_after`'s `i64::try_from(..).unwrap_or(i64::MAX)` saturation (43) and the `cooldown_until` event field (234) either way.
**Tests:** (A) a runner test asserting `capture.cooldown_skip` carries the expected reason after a failure-induced cooldown + a trigger in the window; a cooldown.rs `as_str()` test over all variants. (B) update existing cooldown.rs tests (76-82, 84-95, 97-105) to drop the removed `last_outcome`/`outcome` assertions while keeping `until()`/`is_active()` coverage.

---

*Generated from a `/code-review` (max effort) pass + a 13-cluster verification workflow. 33 findings (4 — PS-01/02/04/08 — span two files each). PS-24 is verified STALE (hardening only). Re-run `cargo fmt --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace` after each fix.*
