## 1. Persisted projection schema + writer (perchstation-core)

- [ ] 1.1 Add `Deserialize` to `CaptureSnapshot`, `CaptureFailureSnapshot`, and
  `CaptureLivenessSnapshot` (they already derive `Serialize`); keep the
  `rename_all`/RFC3339-Z representations stable.
- [ ] 1.2 Define the persisted record `{ as_of: DateTime<Utc>, capture:
  CaptureSnapshot }` and a `data_dir`-relative path constant (e.g.
  `capture-status.json`).
- [ ] 1.3 Implement an atomic writer (temp + `rename`, matching the queue
  store's discipline) that serialises `CaptureState::snapshot()` plus an
  injected `now`. Write a unit test asserting temp-then-rename and that a
  concurrent read sees no partial file.
- [ ] 1.4 Implement a reader that loads + parses the projection, returning
  "no projection" for both missing and unparseable files. Unit-test the
  missing and corrupt cases.

## 2. Freshness-aware snapshot (perchstation-core)

- [ ] 2.1 Extend the capture projection carried in `StatusSnapshot` with an
  `as_of` and a `stale` indicator (added field/enum), defaulting to the
  current never-observed shape when absent.
- [ ] 2.2 In `status::snapshot`, when `capture: Option<&CaptureState>` is
  `None`, read the sidecar; classify live vs stale against the threshold
  (small multiple of `liveness_poll_secs`, with a constant floor); fall back to
  default on missing/corrupt. Leave the `Some` path untouched.
- [ ] 2.3 (TDD) Unit-test `snapshot`: fresh projection → reported live; stale
  projection → marked stale and not "healthy"; missing → never observed;
  corrupt → never observed + Ok.

## 3. Text + JSON rendering (perchstation-core)

- [ ] 3.1 Update `render_text` so the Capture block shows the `as_of` time and a
  stale marker when applicable; keep the existing layout when live.
- [ ] 3.2 Confirm the JSON output includes `as_of`/freshness; snapshot-test the
  serialized shape.

## 4. Wire serve to publish the projection (perchstation binary)

- [ ] 4.1 In `serve.rs`, spawn a small supervised flush task that snapshots the
  shared `CaptureState` and writes the projection every `liveness_poll_secs`.
- [ ] 4.2 Write the projection once more on graceful shutdown (after the capture
  task stops) so the final state is persisted.
- [ ] 4.3 Ensure the flush task is a no-op-safe when capture init was disabled
  (no `CaptureState`) — either skip publishing or publish never-observed.

## 5. Stop forcing None in the status command (perchstation binary)

- [ ] 5.1 In `commands/status.rs`, remove the hard-coded `None`; the
  freshness-aware sidecar read inside `status::snapshot` now supplies the
  capture block. Update the module doc comment that documented the old
  limitation.

## 6. Integration test (separate-process semantics)

- [ ] 6.1 Add an integration test (extend `status_surface.rs` or a new file)
  that runs the capture loop, publishes a projection, and asserts a snapshot
  taken *without* the in-process `CaptureState` reflects the last recording and
  sensor liveness.
- [ ] 6.2 Assert the stale path: an old `as_of` is rendered stale and not as a
  live `healthy` reading.

## 7. Docs + gate

- [ ] 7.1 Update `specs/002-capture-subsystem/contracts/cli.md` §`status` for
  the `as_of`/freshness fields (text + JSON).
- [ ] 7.2 Update the `README.md` Status "known limitation" note: Finding A is
  resolved (drop the "always renders never observed" caveat; keep the journal
  as the corroborating source per SC-007).
- [ ] 7.3 Run the gate: `cargo fmt --check`, `cargo clippy --all-targets
  --workspace -- -D warnings`, `cargo test --workspace`.
- [ ] 7.4 Re-run RELEASE-CHECKLIST Step 3.2 intent (status reflects the
  recording) against a host-side `serve`; confirm Finding A is cleared.
