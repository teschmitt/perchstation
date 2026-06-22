## Context

The capture loop keeps its runtime state in `CaptureState`
(`crates/perchstation-core/src/capture/state.rs`), an `Arc<RwLock<…>>` the
supervisor mutates via `record_success` / `record_failure` / `set_liveness`.
`status::snapshot(data_dir, now, capture: Option<&CaptureState>)`
(`observability/status.rs:179`) projects it into `CaptureSnapshot`. The
standalone `perchstation status` binary runs in a different process from
`serve`, so it has no `CaptureState` to pass and supplies `None`; the snapshot
then uses `CaptureSnapshot::default()`, whose `sensor_liveness` is
`NeverObserved`. Result: the capture block is always empty for the command an
operator actually runs.

Everything else `status` reports (enrollment, queue depth, recent deliveries) it
reads straight from `data_dir` — `status` is documented as "a pure read against
`data_dir`, safe to run alongside `serve`". Capture state is the one field with
no on-disk representation. This change gives it one.

## Goals / Non-Goals

**Goals:**
- A separate-process `perchstation status` reflects the live capture loop within
  the SC-007 30-second budget.
- Preserve the "`status` is a pure, side-effect-free read, safe alongside
  `serve`" property.
- Distinguish live state from a stale projection left by a stopped `serve` — no
  false "healthy" readings.
- Keep the in-process path (integration tests) as the source of truth when a
  live `CaptureState` is supplied; no behavior change there.

**Non-Goals:**
- Live, on-demand querying of `serve` (no socket/RPC) — see Decisions.
- Sub-second freshness; the projection is eventually-consistent within the
  liveness-poll cadence.
- Any change to the capture decision logic, the wire contract, or perchpub.
- Hot-reload of capture config (tracked separately).

## Decisions

### File-based sidecar projection, not IPC

`serve` writes its `CaptureSnapshot` to a sidecar file under `data_dir`
(working name `capture-status.json`), and `status` reads it. This matches the
existing architecture: every other `status` field already comes from a pure
`data_dir` read, and the sidecar is readable whether `serve` is up, down, or
mid-restart.

Rejected alternatives:
- **Unix-domain socket / RPC to `serve`**: forces `status` to handle a daemon
  that may be absent or restarting, adds a socket lifecycle and timeout
  handling, and breaks the "pure read" model. No upside for a low-frequency
  projection.
- **Shared-memory mmap**: more machinery (layout/versioning, platform nuance)
  than a once-every-few-seconds JSON write warrants.

### Refresh cadence: periodic flush at the liveness-poll interval

A small task in `serve` snapshots `CaptureState` and rewrites the sidecar every
`capture.liveness_poll_secs` (default 5 s), plus once on shutdown. That cadence
already governs how fast liveness transitions become visible internally, keeps
`as_of` comfortably inside the 30 s SC-007 budget, and means a single timer
covers both value changes and heartbeat freshness. Write-through on every
mutation was considered but adds a `data_dir` write path into core state for no
freshness benefit over a 5 s flush.

### Schema: persisted snapshot + `as_of`

Persist `{ as_of: DateTime<Utc>, capture: CaptureSnapshot }`. `CaptureSnapshot`
already derives `Serialize`; add `Deserialize` (and to its nested types) for the
read side. `as_of` is the write time, used for the freshness rule below.

### Freshness rule

`status` treats the projection as **live** when `now - as_of <=` a staleness
threshold, and **stale** otherwise. Threshold = a small multiple of the flush
cadence (e.g. `3 × liveness_poll_secs`, floored at a fixed minimum) so a single
missed flush does not flip to stale, but a stopped `serve` does within ~15 s.
Because `status` cannot read `serve`'s config without an enrollment, the
threshold is derived from the loaded `Config` (which `status` already loads) or a
constant if capture config is unavailable. A stale block is rendered with its
`as_of` and omits any live "healthy" assertion.

### `status::snapshot` signature

Keep the `capture: Option<&CaptureState>` parameter. When `Some`, behavior is
unchanged (source of truth for tests). When `None`, attempt to load + parse the
sidecar; on success project it with freshness; on missing/corrupt, fall back to
`CaptureSnapshot::default()`. The freshness annotation rides in the snapshot
(e.g. an added `as_of: Option<DateTime<Utc>>` plus a `stale: bool`, or an enum)
and is rendered by `render_text` / serialized in JSON per the updated contract.

### Atomic writes

Write to `capture-status.json.tmp` then `rename` into place — the same
temp-then-rename discipline the queue store already uses — so a concurrent
reader sees either the old or the new complete file.

## Risks / Trade-offs

- **Stale-but-plausible readings**: a `serve` that wedges without exiting could
  keep `as_of` fresh while the capture loop is dead. Mitigated only partially by
  the supervisor design (the loop is independently supervised); the journal
  remains the ground truth, and SC-007 explicitly pairs `status` *and* the log.
- **Extra `data_dir` write every few seconds**: negligible (a few hundred bytes,
  atomic rename) and bounded; no growth (single file overwritten).
- **Schema coupling**: persisting `CaptureSnapshot` ties the on-disk format to
  that struct. Acceptable — it is already the JSON contract surface for
  `status`; the file is additive and versionable if needed later.
- **Clock**: `as_of` and the freshness comparison use the same `Utc::now()` /
  injected clock the rest of `status` uses; no new time source.
