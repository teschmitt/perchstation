## Context

The station's identity to perchpub is an mTLS client certificate; perchpub pins
`SHA256(SubjectPublicKeyInfo)` of the leaf at enrollment and matches it on every
upload, and it derives the step-ca authorization subject from the CSR's **first
DNS SAN** (CN only as fallback). `docs/perchstation-csr-contract.md` (authoritative
as of perchpub `4de1f30`) captures these rules. An audit of the current client
against that contract found:

- `enrollment/csr.rs:48-54` builds the CSR with
  `CertificateParams::new(vec!["station-enrollment".into()])` and never sets a
  DistinguishedName. rcgen 0.13.2's `CertificateParams::new` returns
  `{ subject_alt_names, ..Default::default() }`, and `Default` pushes
  `CommonName = "rcgen self signed cert"`. So every station's CSR carries
  `CN = "rcgen self signed cert"` + `SAN = DNS:station-enrollment` — the verbatim
  §10 bug (CN ≠ SAN, CN at the rcgen default), and a SAN that is identical across
  all stations (not unique, §3.1). The module doc (`csr.rs:10-12`) still claims
  perchpub "rewrites the subject server-side", which §1/§10 contradict.
- `enrollment/confirm.rs:262` builds its `:443` client via
  `tls::rustls_builder_with_roots`, which sets `.tls_built_in_root_certs(false)`
  (`tls.rs:44`) and trusts **only** the QR's `ca_chain_pem`. §7 says the `:443`
  edge is Let's-Encrypt and MUST be validated against system trust; the device CA
  is not the edge CA. The working deployment only compensates by smuggling the LE
  intermediate (`YR2`) into the QR bundle (`deploy/CAMERA-LESS-TEST.md`).
- `commands/enroll.rs:90` always calls `csr::generate()`, which always calls
  `KeyPair::generate_for`. No code loads `station.key` back into a `KeyPair`, so
  there is no way to reuse the key — violating §2 ("generate once … reuse for the
  life of the station") and blocking the §8 same-SPKI renewal requirement.
- `perchpub/client.rs:126-131` assembles the mTLS `Identity` from `station.crt` +
  `station.key` only, presenting a leaf-only chain where §6 SHOULD present
  leaf + intermediate.

Two pieces already conform and are reused: `confirm::validate_chain` (SPKI match,
chain verification, validity window) is exactly the success validation §4.3
requires, and `tls::rustls_builder_for_upload` (`tls.rs:72`) already implements
the public-roots-plus-additive-CA model §7 wants for the edge.

## Goals / Non-Goals

**Goals:**
- Produce a CSR that satisfies §3 unconditionally: CN == first DNS SAN, a unique
  DNS-valid identity, Ed25519 PoP — so enrollment cannot 502 on a CN/SAN mismatch.
- Validate the `:443` enrollment edge against system trust (§7) and stop relying
  on a device-CA-pinned QR to anchor a public edge.
- Make the keypair a generate-once, reused-for-life identity (§2/§8/§10), with a
  loadable persisted key.
- Present the full client chain on upload (§6).
- Lock all of the above behind §12.1 conformance tests.

**Non-Goals:**
- Implementing proactive renewal — that is the `cert-renewal` change (F4). This
  change only corrects `cert-renewal`'s key-rotation decision to reuse the key.
- Defining or changing the perchpub renewal endpoint (§8 open dependency).
- Changing the upload server-trust model (`rustls_builder_for_upload`) — it
  already conforms; it is only reused for the `confirm` edge.

## Decisions

### A station identity string, derived once and persisted
The CSR needs a stable, unique, DNS-valid identity for both the first DNS SAN and
the CN (§3.1/§3.2). Recommended: derive `station-<hex>` from the keypair's SPKI
hash (e.g. the first bytes of `SHA256(SPKI)`), so the name is deterministic from
the key, automatically unique, stable as long as the key is (which §2 now
guarantees), and needs no extra persisted state. Alternative: a random id stored
in `identity.json`. Either satisfies the contract; the SPKI-derived form is
preferred because it ties the human-facing name to the pinned identity and adds
no new field. The chosen identity is set as both `SanType::DnsName` and
`DnType::CommonName`, replacing rcgen's default DN — per the §11 reference builder.

### `confirm` uses public-root edge trust, QR carries the device CA only
`confirm::build_client` switches from `rustls_builder_with_roots` to a
public-roots base (the `rustls_builder_for_upload` model: system roots on, the
device CA added only as an additive anchor if ever needed), so the `:443` LE edge
validates against system trust per §7. The QR `ca_chain_pem` then carries **only**
the device CA (no `YR2`), which is what gets persisted and used for upload
server-trust. This removes the LE-intermediate pinning, the QR size pressure, and
the breakage when Let's Encrypt rotates intermediates. `mint-enroll-qr` and
`deploy/CAMERA-LESS-TEST.md` are updated accordingly.

### Generate the keypair once; reuse it; new key only for a new station
`identity` gains a `load_keypair()` that reads `station.key` back into a
`KeyPair`. `enroll` reuses the persisted key when re-enrolling the *same* station
and only generates a fresh key when there is no key yet or the operator passes
`--force` to deliberately enroll as a *new* station (new SPKI). `csr::generate`
is refactored to accept an existing `KeyPair` (build-CSR-from-key) so renewal and
re-enroll share one path. This is the foundation the `cert-renewal` flow consumes.

### Full-chain upload identity
`perchpub/client::build_inner` appends the intermediate from `ca_chain.pem` to the
`Identity` PEM (leaf, then intermediate, then key) so the handshake presents the
full client chain (§6). The intermediate is selected from the persisted chain;
the root (if present) is not sent.

### §12.1 conformance tests
Add tests that parse the produced CSR and assert: parses as PKCS#10; key algorithm
∈ {Ed25519, P-256}; CN present, ≠ `"rcgen self signed cert"`, == first DNS SAN;
≥1 DNS-valid (RFC 1123) `dNSName`; self-signature (PoP) verifies. These encode
the §12.1 acceptance list and would have caught F1.

### Reconcile the `cert-renewal` change
`cert-renewal`'s design ("Renew a new key each time"), its spec requirement
("a new key per renewal, not the existing one"), the proposal's "freshly
generated Ed25519 keypair", and tasks 4.1/4.2 are edited to **reuse the existing
keypair** (same SPKI), citing §8. Without this, a "successful" renewal would
present a leaf with a new SPKI that perchpub's enrollment-time pin would reject as
an unknown station (`401`) on every subsequent upload.

## Risks / Trade-offs

- **QR bundle change forces a re-mint**: existing two-cert QRs (with `YR2`) keep
  working only while `confirm` still pins them; once `confirm` uses system trust,
  operators rebuild QRs as device-CA-only. The new QRs are simpler and survive LE
  intermediate rotation, so this is a one-time migration, not ongoing cost. Both
  the tooling and `CAMERA-LESS-TEST.md` are updated in the same change.
- **Cross-repo renewal dependency**: F3's *renewal* key-reuse only matters once
  perchpub ships a renewal endpoint (tracked by `cert-renewal`, §8 open
  dependency). The maintainer owns both repos; the contract states the SPKI is
  pinned at enrollment and matched on every upload, so reuse is the safe path. If
  perchpub instead re-pins on a re-enroll-style renewal, key reuse becomes
  optional — but §8 mandates reuse, so this change follows the contract.
- **Re-enroll semantics shift**: today `--force` (and indeed every enroll) mints a
  new key. After this change, re-enroll without `--force` reuses the key (same
  station), and `--force` is the explicit "new station" path. This must be
  documented clearly so an operator does not orphan a station's identity by
  accident; the CLI help and `enroll` logs call out which path mints a new SPKI.
- **Identity-from-SPKI ordering**: the identity string must be derived before the
  CSR is built but the SPKI comes from the keypair, which exists first — so the
  ordering is keypair → derive identity → build CSR; no chicken-and-egg.
- **Security**: unchanged posture — the private key stays device-only and `0600`,
  TLS verification is never disabled (system trust replaces a narrower pin; it is
  not weakened), and the redaction registry still scrubs the key/CSR PEM.
