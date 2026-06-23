## Why

`docs/perchstation-csr-contract.md` — the device-certificate contract extracted
from the perchpub server (authoritative as of perchpub commit `4de1f30`) — was
audited against the station's certificate handling. The audit found that the CSR
builder still reproduces **the exact bug the contract was written to close**
(§10): it leaves the Subject CommonName at rcgen's default
`"rcgen self signed cert"` and puts the only identity in a fixed, non-unique DNS
SAN, the literal `station-enrollment`
(`crates/perchstation-core/src/enrollment/csr.rs:51`; rcgen `CertificateParams::new`
inherits the placeholder CN). Per §10 this CN≠SAN mismatch is what step-ca
rejects with `403 "...does not contain the valid DNS names"`, surfacing as
`502 {"detail":"certificate issuance failed"}` — the precise symptom recorded as
the live enrollment blocker in `deploy/CAMERA-LESS-TEST.md` (currently attributed
to a perchpub signing-key fault).

Three further conformance gaps were found: the enrollment-`confirm` client
validates the public `:443` edge against the QR's **device CA** with public roots
disabled instead of system trust (§7); the station **never reuses its keypair**,
so every enroll mints a new identity and no path exists to reuse `station.key`
(§2/§8/§10 anti-pattern); and the upload handshake presents a **leaf-only**
client identity instead of the full chain (§6).

## What Changes

- **CSR identity (F1, HIGH — likely the live 502 blocker)**: the CSR Subject
  CommonName is set **equal to its first DNS SAN**, and the identity becomes a
  stable, **unique**, DNS-valid `station-<id>` string. Today CN is the rcgen
  placeholder and the SAN is the literal `station-enrollment` for every station
  (`enrollment/csr.rs:51`).
- **Enrollment edge TLS (F2, HIGH)**: `enrollment::confirm` validates the `:443`
  Let's-Encrypt edge against **system/public trust** (§7) rather than pinning the
  QR's device CA with public roots disabled (`tls.rs:44`, `confirm.rs:262`). The
  QR no longer needs to smuggle the Let's Encrypt intermediate — the ~2953-byte
  two-cert bundle and the YR2-rotation fragility documented in
  `deploy/CAMERA-LESS-TEST.md` go away.
- **Keypair generate-once / reuse (F3, HIGH)**: the station persists and
  **reuses one keypair** for its life, including across renewals (same SPKI),
  regenerating only on a deliberate re-enrollment as a *new* station. Today
  `enroll` unconditionally calls `csr::generate()` and nothing can reload
  `station.key` as a keypair (`enroll.rs:90`) — the §10 "regenerate the keypair ⇒
  new/unknown station" anti-pattern.
- **Full client chain on upload (F5, MEDIUM)**: the upload mTLS identity presents
  **leaf + intermediate** (§6 SHOULD), not the leaf alone (`perchpub/client.rs:126`).
- **CSR conformance tests (F6, MEDIUM)**: the §12.1 structural assertions become
  automated tests (CN present, ≠ rcgen default, == first DNS SAN; ≥1 DNS-valid
  SAN; proof-of-possession verifies; key algorithm ∈ {Ed25519, P-256}). Today
  `csr.rs`'s tests only check PEM markers and keypair freshness — which is why F1
  shipped uncaught.
- **Doc reconciliation (F7, LOW)**: the stale internal contract
  `specs/001-clip-delivery/contracts/perchpub-api.md` (which still claims
  "perchpub rewrites the subject" and ":443 validated against only the QR CA")
  and the `csr.rs` module doc are corrected to match the authoritative contract.
- **Reconcile `cert-renewal` (cross-change)**: the in-flight `cert-renewal`
  change's design decision *"Renew a new key each time"* directly violates §8
  ("Renewal **MUST reuse** the existing keypair"). This change supersedes that
  decision; `cert-renewal`'s design/spec/tasks are updated to reuse the persisted
  key.

Relationship to existing changes: finding **F4** (no proactive renewal —
`delivery/runner.rs:139` halts after expiry) is already the scope of the
**`cert-renewal`** change and is **not** duplicated here; only its key-rotation
decision is corrected. That correction (task 5.2) is a **lockstep dependency**:
`cert-renewal`'s design/spec/proposal/tasks must carry the same-key (same-SPKI)
decision as this change, or the two in-flight changes contradict §8. The
reconciling edits to `cert-renewal` have been applied alongside this proposal.

## Capabilities

### New Capabilities
- `device-cert-conformance`: the station's conformance to the perchpub
  device-certificate contract — the CSR Subject identity (CN equal to a unique
  DNS SAN, never a library default), generate-once-and-reuse keypair, the
  enrollment-edge TLS trust model, the full-chain upload identity, and the CSR
  structural conformance checks.

### Modified Capabilities
<!-- None as a delta against openspec/specs/ (empty — OpenSpec was just adopted),
     so this is captured as a new capability spec. The in-flight cert-renewal
     change's "new key per renewal" decision is reconciled directly in its own
     files (see tasks §5), not as a delta here. -->

## Impact

- **Code**: `enrollment/csr.rs` (CN + unique DNS SAN, from a station-identity
  source), `tls.rs` / `enrollment/confirm.rs` (public-root edge trust for
  confirm), `identity.rs` (load/reuse the persisted keypair; persist the identity
  string), `commands/enroll.rs` (reuse the key when re-enrolling the same station
  vs. `--force` minting a new station), `perchpub/client.rs` (append the
  intermediate to the mTLS identity).
- **Enrollment tooling / deploy**: `mint-enroll-qr` and
  `deploy/CAMERA-LESS-TEST.md` — the QR `ca_chain_pem` becomes **device-CA-only**
  (drop the Let's Encrypt `YR2` cert) once `confirm` uses system trust.
- **Wire contract (cross-repo, perchpub)**: F1/F2/F5 need **no** perchpub change
  — perchpub already prefers the SAN and pins SPKI; this change makes the station
  *produce* a conformant CSR and validate the edge correctly. F3's renewal
  key-reuse depends on the perchpub renewal endpoint tracked by `cert-renewal`
  (§8 open dependency).
- **Docs**: `specs/001-clip-delivery/contracts/perchpub-api.md` (F7), the
  `csr.rs` module doc (F7), and the reconciled `cert-renewal` artifacts.
- **Tests**: §12.1 structural CSR conformance tests; a `confirm`-client test
  asserting public-root edge validation; a key-reuse test (same SPKI after
  reload); an upload-identity test asserting the intermediate is presented.
