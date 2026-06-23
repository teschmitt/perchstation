## Context

Enrollment is one-shot today. The operator scans a QR carrying a `session_id`,
`auth_token`, and the perchpub CA chain; the station generates an Ed25519
keypair + CSR (`enrollment/csr.rs`), POSTs `/api/v1/enrollment/confirm/{session_id}`
pinned to the QR CA, validates the issued leaf (`confirm::validate_chain` —
chain + validity window + key-match), and persists `station.key`, `station.crt`,
`ca_chain.pem`, `identity.json` (`identity.rs`). Thereafter `delivery` and
`classify` present that cert as their mTLS client identity for uploads.

When `cert_not_after <= now`, `delivery/runner.rs` logs `DELIVERY_CERT_EXPIRED`
once and idles on a 1-minute tick without exiting, so `status` keeps reporting
`Expired` until a human re-enrolls. There is no renewal and no early warning.
Two existing pieces make renewal tractable: `confirm::validate_chain` is exactly
the validation a renewal response needs, and `PerchpubClient::reload()` (PS-18)
already hot-swaps the in-process mTLS identity behind an `RwLock` (today driven
by SIGHUP after a manual re-enroll).

## Goals / Non-Goals

**Goals:**
- Keep an unattended station uploading across certificate boundaries with no
  operator action, for the lifetime of the deployment.
- Renew using the station's existing mTLS identity — no QR, no physical visit.
- Rotate credentials crash-safely and take effect without a restart.
- Degrade to exactly today's expiry behavior when renewal is unavailable, and
  give the operator early warning regardless.

**Non-Goals:**
- Changing the initial QR enrollment flow or its trust pinning.
- Rotating the CA trust anchors themselves (the renewal may *deliver* an updated
  chain, but bootstrap trust still comes from enrollment).
- Designing perchpub's CA/issuance internals — only the station-facing contract.
- Short-lived/ACME-style certificates or an on-device CA.

## Decisions

### New mTLS endpoint: `POST /api/v1/enrollment/renew`

Renewal is a post-enrollment, authenticated operation, so it fits the existing
rule from `contracts/perchpub-api.md` that *every* call except
`/enrollment/confirm` presents the station client certificate. Contract:

- **Auth**: mTLS with the current `station.crt`; perchpub derives `station_id`
  from the client cert (no body identifier needed, preventing a station from
  renewing another's cert).
- **Channel/base**: the mTLS upload edge (`upload_url`, `:8443`), reusing
  `tls::rustls_builder_for_upload` (public roots + enrollment CA) and the
  station identity — the same trust model the upload client already uses.
- **Request**: `{ "csr_pem": "<PKCS#10>" }` — a CSR built over the station's
  **existing** persisted keypair (same SPKI, per device-cert contract §8; see the
  "Reuse the existing keypair across renewals" decision below).
- **Response**: `{ "certificate_pem": "...", "ca_chain_pem": "..." }`, mirroring
  `EnrollmentResponse` so `validate_chain` is reused verbatim.
- **Errors**: 4xx → terminal for this attempt (no retry of a rejected CSR); 5xx /
  transport → transient, retried with backoff.

The station side ships behind a `[renewal] enabled` flag and is inert until
perchpub implements the endpoint; a `fakepub` handler provides it for tests.

### Reuse the existing keypair across renewals

Each renewal builds its CSR over the station's **existing** persisted keypair, so
the renewed leaf keeps the same `SHA256(SubjectPublicKeyInfo)` and perchpub
recognizes the same station with no server-side re-pin (device-cert contract §8,
`docs/perchstation-csr-contract.md`). This consumes the generate-once/reuse
keypair and `identity::load_keypair()` introduced by the
`cert-contract-conformance` change. Rejected: a fresh key per renewal — it changes
the SPKI, which perchpub's enrollment-time pin would reject as an unknown station
(`401` on every subsequent upload). (Supersedes the earlier draft decision, which
called for a new key each renewal — a §8 violation surfaced by the cert-contract
audit.)

### Dedicated supervised `RenewalRunner` task

A new long-lived task spawned by `serve` alongside `delivery`/`classify`, woken
on a coarse timer (e.g. hourly), that: loads `cert_not_after`; if remaining
lifetime `< renew_before`, attempts renewal; on failure backs off (bounded,
jittered) before the next attempt. Renewal cadence is slow, so this reuses the
backoff *ideas* from `[retry]` but is its own small loop, not the per-clip
delivery retry. Jitter on the trigger avoids fleet-synchronized renewals.

Rejected: folding renewal into the delivery loop — it would entangle two
independent failure domains and the delivery loop already halts on expiry.

### Atomic credential rotation

The reused keypair (§8) means `station.key` is unchanged across a renewal, so
rotation stages the renewed `station.crt`/`ca_chain.pem` (plus a possibly
re-stamped `identity.json`) in a temp location under `credentials/`, `fsync`,
then swaps atomically so a crash leaves either the old or new complete set — never
a leaf that mismatches the persisted key or a half-written chain. Because the
files must move together, the swap uses a staging directory + directory rename (or
an equivalent transactional move), rather than independent renames. After the
swap, call `PerchpubClient::reload()` to pick up the new leaf in-process; in-flight
uploads finish under the old leaf, new ones use the new.

### Approaching-expiry surfacing

Add a remaining-lifetime / "expiring soon" reading to the enrollment projection
in `observability/status.rs` (a new `EnrollmentState` variant or an added
`expires_in`/`warn` field) plus a journal warning event
(`ENROLLMENT_EXPIRING_SOON`). This is independent of renewal success and is the
first, cheapest slice to land. `status` already loads `cert_not_after`, so this
is a pure projection change.

### Config: `[renewal]`

`enabled` (default on, but inert without the endpoint), `renew_before_*` (when to
start renewing — e.g. a fraction of validity or a fixed window), `warn_before_*`
(when to warn), and renewal retry/backoff bounds. All defaulted and
numeric-range-validated at startup like the existing `[queue]`/`[retry]`/`[capture]`.

## Risks / Trade-offs

- **Cross-repo dependency**: the station feature is dormant until perchpub ships
  `/enrollment/renew`. Mitigated by the `enabled` flag, the documented contract,
  and the `fakepub` handler that lets the station side be fully tested first.
- **Renewal during a backend outage near expiry**: if perchpub is unreachable as
  expiry nears, the station cannot renew and falls back to halt. The warn-before
  window (sized larger than plausible outages) gives the operator lead time; this
  is the residual risk renewal cannot remove.
- **Clock skew**: thresholds and validity checks depend on correct time; the unit
  already orders after `time-sync.target`, and validation reuses the same
  injected clock as enrollment.
- **Rotation correctness**: a botched swap could brick the station's identity.
  Mitigated by validating before swap and by the all-or-nothing staged swap, with
  crash-safety covered by tests.
- **Fleet thundering herd**: many stations enrolled together could renew
  together; mitigated by jittering the trigger.
- **Security**: renewal is mTLS-only and never falls back to an unauthenticated
  path; the new cert is validated before adoption; key PEM stays redacted via the
  existing `RedactingWriter`.
