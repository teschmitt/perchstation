## 1. Config: `[renewal]` block

- [ ] 1.1 Add a `RenewalConfig` (enabled, renew-before threshold, warn-before
  threshold, retry/backoff bounds; optional `renew_url` defaulting to the upload
  base) with serde defaults.
- [ ] 1.2 Extend startup numeric-range validation (as `[queue]`/`[retry]`/
  `[capture]` do) and unit-test the bounds; document every field in
  `deploy/config.example.toml`.

## 2. Approaching-expiry warning (first, independent slice)

- [ ] 2.1 In `observability/status.rs`, project remaining lifetime and an
  "expiring soon" reading (new `EnrollmentState` variant or added field) keyed
  off `cert_not_after` and the warn-before threshold.
- [ ] 2.2 Render the warning in `render_text` and JSON; snapshot-test both.
- [ ] 2.3 Emit an `ENROLLMENT_EXPIRING_SOON` journal warning event (define it in
  `observability` events and `contracts/log-events.md`), once per crossing.
- [ ] 2.4 (TDD) Unit tests: healthy → no warning; within warn-before → warning;
  expired → existing `Expired` state unchanged.

## 3. Perchpub renewal contract + fakepub handler

- [ ] 3.1 Document `POST /api/v1/enrollment/renew` in
  `specs/001-clip-delivery/contracts/perchpub-api.md`: mTLS auth, `{ csr_pem }`
  request, `{ certificate_pem, ca_chain_pem }` response, 4xx-terminal /
  5xx-transient error semantics.
- [ ] 3.2 Add a renew handler to the dev `fakepub` binary that signs a submitted
  CSR with its CA and returns the new cert + chain (parameterizable validity for
  tests).

## 4. Renewal client + response validation (perchstation-core)

- [ ] 4.1 Add request/response types and a renewal client that POSTs a CSR built
  over the station's **existing** keypair (same SPKI, §8 — reusing the
  `cert-contract-conformance` build-CSR-from-key path + `identity::load_keypair()`)
  over mTLS using the station identity and `tls::rustls_builder_for_upload`
  (public roots + enrollment CA) against the renewal base.
- [ ] 4.2 Reuse `confirm::validate_chain` (or factor it shared) to validate the
  renewed leaf: chain to trusted CA, validity window contains now, public key
  matches the station's existing key. (TDD) Unit-test accept + each rejection
  path.

## 5. Atomic credential rotation (identity.rs)

- [ ] 5.1 Implement a crash-safe rotation that stages the renewed cert/chain (the
  reused key is unchanged, §8) plus a re-stamped `identity.json` and swaps them as
  a unit (staging dir + rename or equivalent), never leaving a leaf that
  mismatches the persisted key.
- [ ] 5.2 (TDD) Unit tests: post-rotation `load` yields the new
  `cert_not_after` and a key/cert that match; a simulated interruption leaves a
  complete old-or-new set.

## 6. RenewalRunner task + serve wiring

- [ ] 6.1 Implement a supervised `RenewalRunner` loop: coarse timer, remaining-
  lifetime check against renew-before, attempt → validate → rotate; bounded,
  jittered backoff on failure; emit `RENEWAL_*` events (started / succeeded /
  failed). (TDD) Unit-test the trigger threshold and backoff.
- [ ] 6.2 Spawn it from `serve.rs` alongside delivery/classify under the shared
  runtime + shutdown; on a successful rotation call `PerchpubClient::reload()`
  so uploads adopt the new identity without a restart.
- [ ] 6.3 Confirm the fallback: with renewal disabled or always-failing and the
  cert expired, `delivery/runner.rs` halt behavior and `status` `Expired` are
  unchanged.

## 7. Integration tests (against fakepub renew)

- [ ] 7.1 Happy renewal: a near-expiry station renews, rotates credentials, and a
  subsequent upload succeeds under the new cert without restart.
- [ ] 7.2 Failure retention: renew endpoint down → credentials retained, delivery
  continues, renewal retried.
- [ ] 7.3 Expiry fallback: renewal never succeeds and cert expires → delivery
  halts and `status` shows `Expired` (parity with today).
- [ ] 7.4 Warning surface: a station inside the warn-before window shows the
  approaching-expiry reading via `status` and logs the warning event.

## 8. Docs + gate

- [ ] 8.1 `README.md`: move "enrollment certificate renewal" out of the Deferred
  list; note the new `[renewal]` config and the perchpub dependency.
- [ ] 8.2 `deploy/config.example.toml` `[renewal]` section; `contracts/log-events.md`
  for the new events.
- [ ] 8.3 Run the gate: `cargo fmt --check`, `cargo clippy --all-targets
  --workspace -- -D warnings`, `cargo test --workspace`.
- [ ] 8.4 Coordinate the perchpub `/enrollment/renew` implementation (separate
  repo) before enabling renewal on real hardware.
