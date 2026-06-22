## Why

The station's enrollment certificate has a fixed validity window. When it
expires, `delivery/runner.rs` logs `DELIVERY_CERT_EXPIRED` and halts uploads
until an operator physically re-enrolls with a fresh QR
(`crates/perchstation-core/src/delivery/runner.rs:139`); there is no renewal path
and not even an "expiring soon" signal (`EnrollmentState` is only
`Ok`/`Missing`/`Expired`). For a device whose whole premise is to "live outdoors
for months at a time, unattended, owned by people who are not engineers", a
silent stop at cert expiry — recoverable only by a person at the feeder — is the
first scenario the product fails unattended. This is the "004" feature; renewal
was explicitly deferred by `001`/`002`.

## What Changes

- The station renews its certificate **automatically** before expiry, on a
  schedule driven by remaining lifetime, with no operator action.
- Renewal authenticates with the **current station mTLS identity** (not a
  one-shot QR session token), submits a freshly generated Ed25519 keypair + CSR,
  and receives a new certificate (and updated CA chain when provided).
- **BREAKING (wire contract, perchpub-side)**: introduces a new mTLS-authenticated
  perchpub endpoint, `POST /api/v1/enrollment/renew`. The station side is inert
  until perchpub ships it; the contract is specified here (the maintainer owns
  both repos).
- On a validated renewal the station **atomically rotates** `station.key`,
  `station.crt`, and `ca_chain.pem`, then hot-reloads the upload client's mTLS
  identity in-process (reusing the PS-18 `PerchpubClient::reload()` path) — no
  restart, no upload disruption.
- Renewal **degrades gracefully**: on failure the current credentials are kept
  and retried with bounded backoff; delivery is never disrupted; if the cert
  expires anyway the existing halt-and-await-re-enrollment behavior is unchanged.
- A **pre-expiry warning** (a distinct `status` reading plus a journal warning
  event) fires once remaining lifetime crosses a configurable threshold, so an
  operator gets lead time even where automatic renewal is unavailable. This is
  the cheapest, first increment of the change.
- New `[renewal]` config block (enable flag, renew-before / warn-before
  thresholds, retry bounds); all with defaults.

## Capabilities

### New Capabilities
- `cert-renewal`: the station-side certificate-renewal lifecycle — when renewal
  is attempted, the mTLS renewal exchange and response validation, atomic
  credential rotation with in-process reload, graceful failure and fallback to
  the existing expiry behavior, and the approaching-expiry warning surface.

### Modified Capabilities
<!-- None: openspec/specs/ is empty (OpenSpec was just adopted), so this is
     captured as a new capability spec rather than a delta against an existing
     one. The existing expiry-halt behavior is referenced as the fallback. -->

## Impact

- **Wire contract / perchpub (cross-repo)**: new `POST /api/v1/enrollment/renew`
  (mTLS-auth; body `{ csr_pem }`; returns `{ certificate_pem, ca_chain_pem }`).
  Documented in `specs/001-clip-delivery/contracts/perchpub-api.md`; a matching
  handler added to the dev `fakepub` for tests.
- **Code**: a new renewal runner/task in `perchstation-core` (alongside
  `delivery`), reusing `enrollment::csr::generate` and `confirm::validate_chain`;
  atomic credential rotation in `identity.rs`; `[renewal]` in `config.rs`;
  approaching-expiry in `observability/status.rs`; wiring + post-rotation
  `PerchpubClient::reload()` in `serve.rs`.
- **On-disk**: credential files are rotated in place (no new persistent files);
  rotation must be crash-safe.
- **Config / docs**: `deploy/config.example.toml` `[renewal]`; `README.md` moves
  certificate renewal out of the Deferred list; new log events documented in
  `contracts/log-events.md`.
- **Tests**: integration coverage for happy renewal, failure retention, expiry
  fallback, and the warning surface, against a `fakepub` renew endpoint.
