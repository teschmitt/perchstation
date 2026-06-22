## Why

`perchstation status` runs in its own process and cannot read `serve`'s
in-memory `CaptureState`, so it passes `None` and the capture block always
renders `(never observed)` / `(none)` regardless of what the running capture
loop is doing (`crates/perchstation/src/commands/status.rs:20-27`). That makes
**SC-007** ("an operator with only local shell access can determine, from
`perchstation status` and the JSON log alone and within 30 seconds, when the
last clip was captured, whether the most recent attempt failed and why, and
whether the sensor is currently healthy") satisfiable only via the journal, not
via `status`. This was filed as **Finding A** of the 2026-06-18 on-device gate
run and currently blocks RELEASE-CHECKLIST Step 3.2. This is the "003" feature
in the project's sequence (after `001-clip-delivery`, `002-capture-subsystem`).

## What Changes

- `serve` persists its live capture projection to a sidecar file under
  `data_dir`, refreshed at the sensor-liveness poll cadence and on shutdown.
- The persisted projection carries an `as_of` timestamp so a reader can tell a
  live reading from a stale one left behind by a stopped `serve`.
- `perchstation status` (separate process) reads that sidecar when it has no
  in-process `CaptureState`, instead of falling back to `(never observed)`.
- The capture block in both text and JSON output gains an `as_of` / freshness
  annotation; a stale projection is rendered as stale rather than as a false
  live "healthy" reading.
- Reads stay pure and side-effect-free; writes are atomic (temp + rename) so
  `status` never sees a torn projection — preserving the "`status` is safe to
  run alongside `serve`" contract.
- The in-process path (integration tests pass a live `CaptureState`) is
  unchanged and remains the source of truth when present.
- Doc updates: the capture `status` CLI contract and the README "known
  limitation" note (which this change resolves).

## Capabilities

### New Capabilities
- `capture-status-surface`: how the capture loop's runtime state (last
  recording, last capture failure, sensor liveness) is surfaced to a
  separate-process `perchstation status`, including the persisted projection,
  its freshness semantics, and the read/write safety guarantees.

### Modified Capabilities
<!-- None: openspec/specs/ is empty (OpenSpec was just adopted), so this
     behavior is captured as a new capability spec rather than a delta against
     an existing one. -->

## Impact

- **Code**: `crates/perchstation-core/src/observability/status.rs` (sidecar
  read + freshness), a new persisted-projection writer in
  `perchstation-core` (likely `capture/state.rs` or a sibling),
  `crates/perchstation/src/commands/serve.rs` (periodic flush + shutdown
  flush), `crates/perchstation/src/commands/status.rs` (no longer forced to
  `None`).
- **On-disk layout**: one new file under `data_dir` (e.g.
  `capture-status.json`); additive, ignored by older readers.
- **Docs**: `specs/002-capture-subsystem/contracts/cli.md` §`status`,
  `README.md` Status section.
- **Tests**: a separate-process integration test that asserts the capture
  block reflects live state and that a stale projection is marked stale.
- **No API / wire-contract change**; no perchpub involvement.
