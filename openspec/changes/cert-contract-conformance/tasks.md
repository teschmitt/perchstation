## 1. CSR subject identity + structural conformance (F1, F6 — priority: unblocks enrollment)

- [ ] 1.1 (TDD) Add §12.1 structural conformance tests in `enrollment/csr.rs`
  that parse the produced CSR and assert: parses as PKCS#10; public-key algorithm
  ∈ {Ed25519, P-256}; Subject CommonName is present, is **not**
  `"rcgen self signed cert"`, and **equals** the first DNS SAN; ≥1 DNS-valid
  (RFC 1123) `dNSName`; self-signature (proof-of-possession) verifies. These fail
  against today's builder.
- [ ] 1.2 Introduce a station-identity source — a stable, unique, DNS-valid
  `station-<id>` string (recommended: derived from `SHA256(SPKI)` of the keypair).
- [ ] 1.3 In `csr::generate` (and the build-from-key path of §2.2), set
  `params.subject_alt_names = vec![SanType::DnsName(identity)]` and a
  `DistinguishedName` with `CommonName == identity`, overwriting rcgen's default
  DN (per the §11 reference builder). Make 1.1 pass.
- [ ] 1.4 Remove/replace the stale `csr.rs:10-12` module doc that claims perchpub
  "rewrites the subject server-side".

## 2. Keypair generate-once / reuse (F3)

- [ ] 2.1 Add `identity::load_keypair()` that reads `station.key` back into an
  rcgen `KeyPair`; (TDD) unit-test that a saved-then-loaded key has the same SPKI.
- [ ] 2.2 Refactor `csr::generate` to a build-CSR-from-an-existing-`KeyPair` path
  (keep a generate-fresh wrapper) so re-enroll and renewal share one builder.
- [ ] 2.3 In `commands/enroll.rs`, reuse the persisted key when re-enrolling the
  same station; generate a fresh key only when none exists or `--force` is given
  (deliberate *new* station = new SPKI). Make the minted-new-SPKI path explicit in
  the CLI help and `enroll` logs.
- [ ] 2.4 (TDD) Test: re-enroll without `--force` reuses the key (same SPKI);
  `--force` mints a new key (different SPKI).
- [ ] 2.5 Audit every `csr::generate()` call site: the fresh-key path is reachable
  only at initial enrollment (no persisted key) or under `--force`; every other
  caller (renewal, plain re-enroll) goes through the build-CSR-from-existing-key
  path. Add a code comment at the generate-or-load branch in `enroll.rs`.

## 3. Enrollment edge TLS trust (F2) + QR tooling

- [ ] 3.1 Switch `enrollment::confirm::build_client` to a public-roots TLS base
  (the `tls::rustls_builder_for_upload` model: system roots on, device CA additive
  only) so the `:443` Let's-Encrypt edge validates against system trust (§7);
  certificate verification stays enabled (SEC-4).
- [ ] 3.2 (TDD) Test that the `confirm` client validates a publicly-rooted edge
  cert without the device CA being present, and still pins/uses the device CA for
  the device-issued chain where needed.
- [ ] 3.3 Update `mint-enroll-qr` so the QR `ca_chain_pem` is **device-CA-only**
  (no Let's Encrypt `YR2`), and revise `deploy/CAMERA-LESS-TEST.md` topology +
  "Why these choices" to drop the two-cert/YR2 workaround.

## 4. Full-chain upload identity (F5)

- [ ] 4.1 In `perchpub::client::build_inner`, append the intermediate from
  `ca_chain.pem` to the mTLS `Identity` PEM (leaf, then intermediate, then key) so
  the handshake presents leaf + intermediate (§6); do not send the root. Select
  the intermediate as the cert in `ca_chain.pem` that issued the leaf (match the
  leaf's issuer), not by position — a self-signed root is skipped.
- [ ] 4.2 (TDD) Test that the assembled identity PEM contains the leaf and the
  intermediate, and that an upload still authenticates against a fake mTLS edge.

## 5. Doc reconciliation (F7) + reconcile the cert-renewal change

- [ ] 5.1 Correct `specs/001-clip-delivery/contracts/perchpub-api.md`: remove the
  "perchpub rewrites the subject" claim (§4.4 behavioural contract) and fix the
  `:443` enrollment-confirm trust note ("validated against only the QR CA chain;
  OS/public trust not consulted") to match §7 (system trust for the LE edge).
- [ ] 5.2 Reconcile `openspec/changes/cert-renewal` with §8: flip the design
  decision "Renew a new key each time" to **reuse the existing keypair** (same
  SPKI); update its spec requirement ("a new key per renewal" → reuse), the
  proposal's "freshly generated Ed25519 keypair" line, and tasks 4.1/4.2.

## 6. Gate

- [ ] 6.1 Run the gate: `cargo fmt --check`, `cargo clippy --all-targets
  --workspace -- -D warnings`, `cargo test --workspace`.
- [ ] 6.2 Re-run the camera-less enrollment smoke (`deploy/CAMERA-LESS-TEST.md`)
  against live perchpub and confirm the §10 502 is gone (a conformant CSR issues a
  leaf); update the doc's "proven working" line.
