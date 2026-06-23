## 1. CSR subject identity + structural conformance (F1, F6 — priority: unblocks enrollment)

- [x] 1.1 (TDD) Add §12.1 structural conformance tests in `enrollment/csr.rs`
  that parse the produced CSR and assert: parses as PKCS#10; public-key algorithm
  ∈ {Ed25519, P-256}; Subject CommonName is present, is **not**
  `"rcgen self signed cert"`, and **equals** the first DNS SAN; ≥1 DNS-valid
  (RFC 1123) `dNSName`; self-signature (proof-of-possession) verifies. These fail
  against today's builder.
- [x] 1.2 Introduce a station-identity source — a stable, unique, DNS-valid
  `station-<id>` string (recommended: derived from `SHA256(SPKI)` of the keypair).
- [x] 1.3 In `csr::generate` (and the build-from-key path of §2.2), set
  `params.subject_alt_names = vec![SanType::DnsName(identity)]` and a
  `DistinguishedName` with `CommonName == identity`, overwriting rcgen's default
  DN (per the §11 reference builder). Make 1.1 pass.
- [x] 1.4 Remove/replace the stale `csr.rs:10-12` module doc that claims perchpub
  "rewrites the subject server-side".

## 2. Keypair generate-once / reuse (F3)

- [x] 2.1 Add `identity::load_keypair()` that reads `station.key` back into an
  rcgen `KeyPair`; (TDD) unit-test that a saved-then-loaded key has the same SPKI.
- [x] 2.2 Refactor `csr::generate` to a build-CSR-from-an-existing-`KeyPair` path
  (keep a generate-fresh wrapper) so re-enroll and renewal share one builder.
- [x] 2.3 In `commands/enroll.rs`, reuse the persisted key when re-enrolling the
  same station; generate a fresh key only when none exists or `--force` is given
  (deliberate *new* station = new SPKI). Make the minted-new-SPKI path explicit in
  the CLI help and `enroll` logs. (Operator-facing behavior flip approved by the
  maintainer — Option A. Blast radius reconciled: `cli.md §enroll`,
  `perchpub-api.md §4.4`, `quickstart.md §5`, `log-events.md`, the
  `reenroll_conflict` integration test. The §001 `tasks.md` T022/T027 entries are
  left as historical as-built records.)
- [x] 2.4 (TDD) Test: re-enroll without `--force` reuses the key (same SPKI);
  `--force` mints a new key (different SPKI). (Rewritten `reenroll_conflict.rs`:
  pass 1 asserts same SPKI + no overwrite audit; pass 2 asserts a different SPKI +
  the WARN `enrollment.overwritten` naming old & new station.)
- [x] 2.5 Audit every `csr::generate()` call site: the fresh-key path is reachable
  only at initial enrollment (no persisted key) or under `--force`; every other
  caller (renewal, plain re-enroll) goes through the build-CSR-from-existing-key
  path. Add a code comment at the generate-or-load branch in `enroll.rs`. (Audited:
  the sole non-test caller of `csr::generate` is `build_fresh_csr`, reached only
  from the `--force` and first-enrollment branches; comment added at the branch.)

## 3. Enrollment edge TLS trust (F2) + QR tooling

- [x] 3.1 Switch `enrollment::confirm::build_client` to a public-roots TLS base
  (the `tls::rustls_builder_for_upload` model: system roots on, device CA additive
  only) so the `:443` Let's-Encrypt edge validates against system trust (§7);
  certificate verification stays enabled (SEC-4).
- [x] 3.2 (TDD) Test that the `confirm` client validates a publicly-rooted edge
  cert without the device CA being present, and still pins/uses the device CA for
  the device-issued chain where needed.
- [x] 3.3 Update `mint-enroll-qr` so the QR `ca_chain_pem` is **device-CA-only**
  (no Let's Encrypt `YR2`), and revise `deploy/CAMERA-LESS-TEST.md` topology +
  "Why these choices" to drop the two-cert/YR2 workaround.

## 4. Full-chain upload identity (F5)

- [x] 4.1 In `perchpub::client::build_inner`, append the intermediate from
  `ca_chain.pem` to the mTLS `Identity` PEM (leaf, then intermediate, then key) so
  the handshake presents leaf + intermediate (§6); do not send the root. Select
  the intermediate as the cert in `ca_chain.pem` that issued the leaf (match the
  leaf's issuer), not by position — a self-signed root is skipped.
- [x] 4.2 (TDD) Test that the assembled identity PEM contains the leaf and the
  intermediate, and that an upload still authenticates against a fake mTLS edge.
  (Unit test in `perchpub::client` asserts the chain selection; hardened with
  `tests/integration/upload_full_chain_mtls.rs` — a 3-test suite driving
  `upload_clip` over a real `RequireAndVerifyClientCert` edge anchored at the
  device **root only**: leaf+intermediate authenticates, leaf-only is rejected,
  and a leaf-under-root control isolates the failure to the missing intermediate.
  This closes the gap where `fakepub` only does *optional* client auth.)

## 5. Doc reconciliation (F7) + reconcile the cert-renewal change

- [x] 5.1 Correct `specs/001-clip-delivery/contracts/perchpub-api.md`: remove the
  "perchpub rewrites the subject" claim (§4.4 behavioural contract) and fix the
  `:443` enrollment-confirm trust note ("validated against only the QR CA chain;
  OS/public trust not consulted") to match §7 (system trust for the LE edge).
  (Note: the "no key reuse across attempts" bullet in §4.4 is reconciled with the
  re-enroll semantics in task 2.3 — see the open question on that task.)
- [x] 5.2 Reconcile `openspec/changes/cert-renewal` with §8: flip the design
  decision "Renew a new key each time" to **reuse the existing keypair** (same
  SPKI); update its spec requirement ("a new key per renewal" → reuse), the
  proposal's "freshly generated Ed25519 keypair" line, and tasks 4.1/4.2.
  (Already applied in commit `3d4b04e`; verified no residual "new key per
  renewal" language remains — the only mentions are in "Rejected:" context.)

## 6. Gate

- [x] 6.1 Run the gate: `cargo fmt --check`, `cargo clippy --all-targets
  --workspace -- -D warnings`, `cargo test --workspace`. (All green: fmt clean,
  clippy clean under `-D warnings`, `cargo test --workspace` exit 0 — 40 test
  binaries, 0 failures. Note: the sandbox intermittently denies *executing* some
  capture-subsystem test binaries (`os error 13`); a clean pass required a retry,
  but the result is deterministic green once all binaries run.)
- [~] 6.2 Re-run the camera-less enrollment smoke (`deploy/CAMERA-LESS-TEST.md`)
  against live perchpub and confirm the §10 502 is gone (a conformant CSR issues a
  leaf); update the doc's "proven working" line. **ENROLLMENT half VERIFIED on a
  Pi 2026-06-23**: live perchpub/step-ca issued a leaf for the conformant CSR
  (`enrollment.persisted`, station 9e9107df, cert valid ~24 h) — the §10 502 is
  cleared. Doc "Last verified" line updated. **Upload half (§4) still pending** a
  live run (the persisted creds are valid ~24 h, so it can be exercised without
  re-enrolling).
