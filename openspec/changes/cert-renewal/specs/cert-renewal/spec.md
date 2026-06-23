## ADDED Requirements

### Requirement: Proactive renewal before expiry

The station SHALL attempt to renew its enrollment certificate automatically, before that certificate expires, driven by its remaining validity rather than by operator action.

Renewal SHALL be attempted once the remaining lifetime crosses a configurable
"renew-before" threshold, and SHALL NOT be attempted while the certificate is
comfortably within its validity window. Attempts SHALL be spread with jitter so a
fleet of stations does not renew in lockstep.

#### Scenario: Renewal triggers as expiry approaches

- **WHEN** the station's certificate remaining lifetime falls below the
  configured renew-before threshold and a renewal endpoint is reachable
- **THEN** the station attempts a renewal without operator intervention

#### Scenario: Healthy certificate is left alone

- **WHEN** the certificate's remaining lifetime is above the renew-before
  threshold
- **THEN** the station makes no renewal attempt

### Requirement: Authenticated renewal exchange and response validation

The station SHALL perform renewal over an mTLS-authenticated channel using its current station identity, and SHALL validate the renewed certificate before adopting it.

Renewal SHALL present the current `station.crt` / `station.key` as the mTLS
client identity (never the one-shot QR session `auth_token`), and SHALL submit a
CSR built over the station's **existing** keypair — the same key, so the renewed
leaf keeps the same `SHA256(SubjectPublicKeyInfo)` (device-cert contract §8), not
a freshly generated key. The renewed certificate SHALL be validated — it MUST
chain to a trusted CA, its validity window MUST contain the current time, and its
subject public key MUST match the station's private key — before it is adopted. A
response that fails validation SHALL be rejected and treated as a failed attempt.

#### Scenario: Valid renewal is accepted

- **WHEN** the renewal endpoint returns a certificate that chains to a trusted
  CA, is currently valid, and matches the submitted key
- **THEN** the station accepts it for rotation

#### Scenario: Invalid renewal response is rejected

- **WHEN** the renewal endpoint returns a certificate that fails chain,
  validity, or key-match validation
- **THEN** the station rejects it, retains its current credentials, and records a
  failed attempt

### Requirement: Atomic credential rotation with in-process reload

On a validated renewal the station SHALL replace its leaf certificate and CA chain atomically — the reused keypair (§8) is unchanged — such that it never operates with a leaf that mismatches its key.

The rotation SHALL be crash-safe: an interruption at any point MUST leave a
complete, usable credential set — either the previous one or the renewed one,
never a mixture. After a successful rotation the station SHALL begin using the
renewed identity for subsequent uploads without a process restart; in-flight
uploads MAY complete under the previous identity.

#### Scenario: Renewed identity takes effect without restart

- **WHEN** a renewal succeeds and the credential set is rotated
- **THEN** subsequent uploads use the renewed certificate without restarting the
  service

#### Scenario: Interrupted rotation leaves a usable credential set

- **WHEN** the process is interrupted during credential rotation
- **THEN** on restart the station loads either the complete previous set or the
  complete renewed set, with key and certificate matching

### Requirement: Graceful failure and fallback to expiry behavior

A failed renewal SHALL NOT disrupt delivery, and SHALL NOT alter the station's existing behavior when a certificate ultimately expires.

When a renewal attempt fails (endpoint unreachable, transient error, or rejected
response) the station SHALL retain its current credentials, continue delivering
clips under them, and retry later with bounded backoff. If renewal never succeeds
and the certificate expires, the station SHALL fall back to the existing
behavior: halt the delivery loop, log the cert-expired event, and keep surfacing
the expired state until the operator re-enrolls.

#### Scenario: Renewal failure keeps the station running

- **WHEN** a renewal attempt fails while the certificate is still valid
- **THEN** delivery continues under the current certificate and renewal is
  retried later with backoff

#### Scenario: Expiry without successful renewal preserves today's behavior

- **WHEN** renewal never succeeds and the certificate reaches its expiry
- **THEN** the station halts delivery and surfaces the expired enrollment state,
  exactly as it does today

### Requirement: Approaching-expiry warning surface

The station SHALL surface an approaching-expiry warning once the certificate's remaining lifetime crosses a configurable "warn-before" threshold, independent of whether automatic renewal succeeds.

The warning SHALL appear both as a distinct reading in `perchstation status`
(reporting that the certificate is expiring and roughly when) and as a journal
warning event, so an operator with only local shell access can act before
uploads stop.

#### Scenario: Warning is surfaced before expiry

- **WHEN** the certificate's remaining lifetime falls below the warn-before
  threshold
- **THEN** `perchstation status` reports an approaching-expiry warning and a
  warning event is written to the journal

#### Scenario: No warning while the certificate is healthy

- **WHEN** the certificate's remaining lifetime is above the warn-before
  threshold
- **THEN** `perchstation status` reports normal enrollment health and no
  approaching-expiry warning is emitted
