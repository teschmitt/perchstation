## ADDED Requirements

### Requirement: Conformant CSR subject identity

The station's enrollment CSR SHALL carry a Subject CommonName **equal to its first
DNS SubjectAltName**, and that identity SHALL be a stable, unique, DNS-valid
string. The CommonName MUST NOT be left at a library default (in particular MUST
NOT be the rcgen placeholder `"rcgen self signed cert"`), and the identity MUST
NOT be carried only in the CommonName or only in a non-DNS SAN.

The first `dNSName` is the station's authoritative identity: it MUST be present,
MUST follow RFC 1123 hostname syntax (lowercase letters, digits, hyphens, dots),
and MUST be unique per station (the literal `station-enrollment` shared by all
stations does not satisfy this). The CSR MUST be a PKCS#10 request whose public
key is Ed25519 (default) or EC P-256, self-signed with the station's private key
so proof-of-possession verifies.

#### Scenario: CSR CommonName equals the first DNS SAN

- **WHEN** the station generates an enrollment CSR
- **THEN** the CSR's Subject CommonName is present, is not `"rcgen self signed cert"`,
  and is byte-for-byte equal to the first `dNSName` in the SubjectAltName extension

#### Scenario: Identity is DNS-valid and key-bound

- **WHEN** the produced CSR is parsed
- **THEN** it contains at least one RFC 1123-valid `dNSName`, its public-key
  algorithm is Ed25519 or EC P-256, and its self-signature (proof of possession)
  verifies

#### Scenario: Two stations produce distinct identities

- **WHEN** two different stations each generate an enrollment CSR
- **THEN** their first `dNSName` values differ, so perchpub can tell the stations
  apart (the identity is not a shared constant)

#### Scenario: Identity is stable across reloads

- **WHEN** the station rebuilds a CSR from the same persisted keypair on a later
  run (a reload or a renewal)
- **THEN** the first `dNSName` and the CommonName are identical to those used
  before, so the station's identity does not change for the life of its keypair

### Requirement: System-trust validation of the enrollment edge

The station SHALL validate the perchpub enrollment endpoint (`:443`, the
`create`/`confirm` calls) against the **public/system trust store**, because that
edge is served with a publicly trusted (Let's Encrypt) certificate. The device CA
delivered in the QR / enrollment response is the device CA, not the edge CA, and
SHALL NOT be required to anchor the `:443` edge. Certificate verification SHALL
NOT be disabled on this path.

#### Scenario: Enrollment confirm validates a publicly-rooted edge

- **WHEN** the station calls `enrollment/confirm` against a `:443` edge presenting
  a publicly trusted (e.g. Let's Encrypt) certificate
- **THEN** the station validates that certificate against the system trust store
  and proceeds, without the QR needing to carry the edge's public intermediate

#### Scenario: Device CA is not used to validate the edge

- **WHEN** the QR `ca_chain_pem` carries only the device CA (no public/edge
  intermediate)
- **THEN** enrollment still validates the `:443` edge successfully via system
  trust, and the device CA is retained only for the device-issued chain (upload
  server-trust and leaf-chain verification)

### Requirement: Generate-once, reused keypair identity

The station SHALL generate its keypair **once**, persist it at owner-only
permissions (`0600`), and reuse it for the life of the station — including across
certificate renewals, where the renewed leaf MUST carry the **same**
`SHA256(SubjectPublicKeyInfo)` so perchpub recognizes the same station with no
server-side change. The station SHALL be able to load its persisted private key
back into a usable keypair. The station MUST NOT rotate the keypair except when an
operator deliberately re-enrolls it as a *new* station.

#### Scenario: Persisted key reloads to the same identity

- **WHEN** a station's persisted `station.key` is loaded back into a keypair
- **THEN** that keypair's `SHA256(SubjectPublicKeyInfo)` equals the SPKI that was
  enrolled, so a CSR built from it re-presents the same station identity

#### Scenario: A certificate refresh reuses the existing key

- **WHEN** the station refreshes its certificate while remaining the same station
  (renewal or a non-`--force` re-enroll)
- **THEN** it submits a CSR over the **existing** keypair, and the issued leaf has
  the same SPKI as before

#### Scenario: Deliberate new-station enrollment mints a new key

- **WHEN** an operator enrolls the station as a new station (e.g. `--force`)
- **THEN** a fresh keypair is generated and a new SPKI is established, replacing
  the previous identity

### Requirement: Full-chain client identity on upload

On media uploads over mTLS (`:8443`), the station SHALL present its private key
and leaf certificate as the client identity. The contract makes presenting the
full chain a §6 SHOULD; this change adopts it as a firm station requirement: the
station SHALL present the **full client chain** (leaf followed by the issuing
intermediate, selected as the cert that issued the leaf — not the self-signed
root) in the TLS handshake rather than the leaf alone, so perchpub's edge can
verify the chain to the device CA.

#### Scenario: Upload handshake presents leaf and intermediate

- **WHEN** the station builds its upload mTLS client identity from the persisted
  credentials
- **THEN** the presented client chain contains the leaf certificate followed by
  the issuing intermediate (the root is not sent), and uploads authenticate
  against an edge that requires and verifies the client chain
