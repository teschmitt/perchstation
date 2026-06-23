# PerchStation device-certificate contract

**Audience:** an agent implementing certificate handling in the **perchstation** repo (the Rust station client).
**Authority:** this is the contract enforced by the **perchpub** server + its step-ca CA. Where this document and the perchpub code disagree, the perchpub code wins — but it is current as of perchpub commit `4de1f30`.

Keywords **MUST / MUST NOT / SHOULD / SHOULD NOT / MAY** are used per RFC 2119.

---

## 1. Background — why the cert is shaped the way it is

A station proves its identity to perchpub with an **mTLS client certificate**. The flow:

1. An operator creates an enrollment session on perchpub and hands the station a **QR code** carrying `session_id`, `auth_token`, the perchpub base URL, and the device-CA chain.
2. The station **generates its own keypair**, builds a **CSR**, and POSTs the CSR to perchpub's enrollment endpoint.
3. perchpub asks its step-ca CA to sign the CSR and returns the **leaf certificate** + CA chain.
4. Forever after, the station presents that leaf (with its private key) on every media upload over mTLS.

Two facts drive every requirement below:

- **The keypair *is* the station's identity.** perchpub does **not** identify a station by name, serial, or CN. It pins `SHA256(SubjectPublicKeyInfo)` of the leaf (`Station.spki_sha256`) at enrollment and matches it on every upload (`backend/app/api/deps.py:get_current_station`). Lose or rotate the private key ⇒ you are a different, unknown station.
- **The identity name lives in a DNS SAN, not the CN.** perchpub derives the step-ca authorization subject from the **first DNS SubjectAltName** of the CSR, falling back to the CommonName only if there is no DNS SAN (`backend/app/ca/step_ca_client.py:_build_provisioner_jwt` / `_csr_dns_sans`). step-ca then authorizes **exactly** the DNS SANs the CSR requests. A CSR that puts the real name only in the CN — or whose CN and SAN disagree — used to be rejected by step-ca with `403 "certificate request does not contain the valid DNS names"`, surfacing to the station as `502 {"detail":"certificate issuance failed"}`. See §10.

---

## 2. Keypair requirements

- The station **MUST** generate its keypair **on the device**. The private key **MUST NOT** leave the device, be transmitted, or be logged.
- Algorithm: the keypair **MUST** be one of:
  - **Ed25519** — REQUIRED default. (Curve25519 / EdDSA; OID `1.3.101.112`.)
  - **EC P-256** (`prime256v1` / `secp256r1`) — MAY be used where Ed25519 is unavailable.
  - RSA and other curves **MUST NOT** be used.
- The station **MUST** generate the keypair **once** and persist it. The same keypair is reused for the life of the station, including across certificate renewals (§8). The station **MUST NOT** rotate the keypair except when deliberately re-enrolling as a *new* station.
- The private key file **MUST** be stored with owner-only permissions (`0600`) under the station's data directory.

---

## 3. CSR requirements

The station builds a PKCS#10 CertificationRequest. Normative rules:

### 3.1 Subject Alternative Name (the identity)
- The CSR **MUST** contain a **SubjectAltName** extension with **at least one `dNSName`** entry.
- The first `dNSName` is the station's authoritative identity. It **MUST** be a stable, unique, DNS-valid string (RFC 1123 hostname syntax: lowercase letters, digits, hyphens, dots; no spaces). Recommended form: `station-<stable-id>` (e.g. `station-7f3a9c`) or an FQDN like `station-7f3a9c.perchpub.local`.
- The SAN value **MUST** be stable for the life of the station — it ends up as the leaf's CN/SAN and is what appears in logs and dashboards. (It is **not** a security boundary; identity is the SPKI pin. But it must be present and DNS-typed.)
- SAN entries of other types (IP, URI, email) **MUST NOT** be used to carry the identity — perchpub extracts `dNSName` entries only; a non-DNS SAN would be ignored and issuance would fall back to the CN.
- Multiple `dNSName` entries **MAY** be supplied; step-ca will authorize all of them and the **first** becomes the leaf subject.

### 3.2 Subject CommonName
- The CSR's Subject **MUST** set a **CommonName equal to the first `dNSName`** from §3.1.
- The CommonName **MUST NOT** be left at a library default. In particular it **MUST NOT** be the rcgen placeholder `"rcgen self signed cert"`. (This was the original enrollment bug; see §10.)

### 3.3 Extensions
- The CSR **SHOULD** contain only the SubjectAltName extension. It **SHOULD NOT** request `keyUsage`, `extendedKeyUsage`, basicConstraints, etc. — step-ca's leaf template sets those on the issued certificate (the leaf comes back with `keyUsage=digitalSignature`, `extendedKeyUsage=serverAuth,clientAuth`); any such request in the CSR is ignored.

### 3.4 Signature / proof of possession
- The CSR **MUST** be self-signed with the station's private key (PKCS#10 proof-of-possession). step-ca verifies this signature.
- Signature pairing **MUST** match the key:
  - Ed25519 key → Ed25519 signature, **no separate hash** (PureEdDSA).
  - EC P-256 key → ECDSA-with-SHA-256.

### 3.5 Encoding / transport
- The CSR **MUST** be **PEM**-encoded with `-----BEGIN CERTIFICATE REQUEST-----` / `-----END CERTIFICATE REQUEST-----` armor, UTF-8.
- It is sent verbatim as the JSON string field `csr_pem` (§4.2). Newlines inside the PEM are preserved (standard JSON string escaping).

---

## 4. Enrollment HTTP exchange

The station performs **only** the *confirm* step. The operator/QR tooling performs `create`; the station receives `session_id` + `auth_token` out-of-band via the QR.

### 4.1 Session constraints (from the QR / perchpub)
- The session is **single-use**, has a **~5-minute TTL**, and allows **at most 3 attempts** (`TTL_SECONDS=300`, `MAX_ENROLLMENT_ATTEMPTS=3` in `backend/app/api/routes/enrollment.py`).
- The station **MUST** generate its keypair+CSR and POST `confirm` promptly after decoding the QR. Generating the keypair does **not** consume the session; only `confirm` does.

### 4.2 Request
```
POST {perchpub_url}/api/v1/enrollment/confirm/{session_id}
Content-Type: application/json

{
  "auth_token": "<auth_token from the QR>",
  "csr_pem":    "-----BEGIN CERTIFICATE REQUEST-----\n...\n-----END CERTIFICATE REQUEST-----\n"
}
```
- `{session_id}` is a UUID from the QR; it goes in the **path**, not the body.
- No `Authorization` header is sent on `confirm` — the `auth_token` is the credential (HMAC-verified server-side).

### 4.3 Success response — `200 OK`
```json
{
  "success": true,
  "reason": "",
  "certificate_pem": "-----BEGIN CERTIFICATE-----\n...(the issued leaf)...\n-----END CERTIFICATE-----\n",
  "ca_chain_pem":   "-----BEGIN CERTIFICATE-----\n...(device CA chain: intermediate [+ root])...\n-----END CERTIFICATE-----\n",
  "station_id": "<uuid>"
}
```

### 4.4 Error responses
All errors are HTTP error statuses with body `{"detail": "<message>"}` (no `success:false` envelope). The station **MUST** treat these as follows:

| Status | Meaning | Station behavior |
|---|---|---|
| `400 invalid CSR` | CSR failed to parse | **Non-retryable** — fix CSR generation. |
| `403` (`Invalid auth token` / `Session expired.` / `Too many retries.` / `Already enrolled.`) | session/credential problem | **Non-retryable** with this session — request a fresh QR. |
| `404 Unknown enrollment session` | bad/expired `session_id` | **Non-retryable** — request a fresh QR. |
| `409` | session missing coordinates | **Non-retryable** — operator must recreate the session. |
| `502 certificate issuance failed` | perchpub→step-ca signing failed | **Non-retryable** — server-side; surface clearly. A **well-formed** CSR per §3 must not produce this. |

The station **MUST NOT** silently retry the same `session_id` after a 4xx — attempts are capped at 3 and the session is single-use.

---

## 5. What the station persists after a successful enrollment

The station **MUST** durably store, under its data directory:

1. **The private key** (already generated in §2) — the identity; guard at `0600`.
2. **`certificate_pem`** — the issued leaf (the client certificate presented on uploads).
3. **`ca_chain_pem`** — the device-CA chain (root + intermediate), used to build/verify the trust chain.
4. **`station_id`** — for logging/correlation.

The leaf and key together form the mTLS client identity for §6.

---

## 6. Using the certificate (media uploads over mTLS)

- Uploads go to the **mTLS entrypoint on port `:8443`**:
  ```
  POST https://{perchpub_host}:8443/api/v1/upload/
  (multipart/form-data, field name: file)
  ```
- The station **MUST** present its **private key + leaf certificate** as the TLS client identity. It **SHOULD** present the full chain (leaf **+** intermediate) in the handshake; perchpub's edge verifies `RequireAndVerifyClientCert` against the device CA (root + intermediate), and the backend additionally checks `leaf.verify_directly_issued_by(intermediate)`.
- perchpub authenticates the station by recomputing `SHA256(SubjectPublicKeyInfo)` of the presented leaf and matching the pinned value. Therefore:
  - The same keypair always yields the same identity. ✔
  - Presenting a leaf whose key was never enrolled ⇒ `401 client certificate required` (generic; perchpub does not distinguish failure modes).
- The leaf's **validity window is enforced** on every upload (`not_valid_before/after`). An expired leaf ⇒ `401`. See §8.

---

## 7. TLS verification of the perchpub edge

- The enrollment endpoint (`{perchpub_url}`, port `:443`) is served with a **publicly trusted (Let's Encrypt)** certificate. Standard system-trust TLS verification is sufficient for the `create`/`confirm` calls; the station **MUST** verify it (no `--insecure`).
- The `ca_chain_pem` from the QR / enrollment response is the **device CA** (step-ca), not the edge CA. It is used to build the client chain for §6 and to verify device-issued certificates — **not** to validate the `:443` edge cert.

---

## 8. Certificate lifetime & renewal

- The issued leaf is **short-lived**: step-ca's default validity is **~24 hours** (observed `valid-from`→`valid-to` span of one day). Plan for this explicitly; it is not a once-and-done certificate.
- The station **MUST** track `notAfter` and renew **before** expiry (e.g. when ≥ ⅔ of the lifetime has elapsed). An expired leaf fails all uploads with `401`.
- Renewal **MUST reuse the existing keypair**. Because perchpub pins by SPKI, a renewed leaf over the same key has the **same SPKI** and is recognized as the same station with no server-side change.
- **Open dependency (coordinate with perchpub, do not invent):** as of this writing the only issuance path is the single-use enrollment `confirm` endpoint, which is HMAC-gated and operator-initiated. There is **no dedicated unattended renewal endpoint yet**. The perchstation agent **MUST NOT** assume one exists; design the renewal hook behind an abstraction and flag this gap so perchpub can add a same-key renewal endpoint (e.g. mTLS-authenticated reissue). Until then, renewal = re-enroll with a fresh session **reusing the stored keypair**.

---

## 9. Security requirements (summary)

- Private key: device-only, `0600`, never logged, never transmitted.
- `auth_token`: treat as a short-lived secret; do not log it.
- Verify the `:443` edge TLS cert against system trust; never disable verification.
- Do not embed long-lived CA credentials on the station — the station only ever holds its own key + leaf + the public CA chain.

---

## 10. Anti-patterns (MUST NOT) — the bug this contract closes

The original failure: perchstation built the CSR with rcgen and **left the CommonName at rcgen's default `"rcgen self signed cert"`** while putting the real identity (`station-enrollment`) only in a DNS SAN. step-ca rejected it:

```
error="certificate request does not contain the valid DNS names
       - got [station-enrollment], want [rcgen self signed cert]"
→ HTTP 403 → perchpub returns 502 {"detail":"certificate issuance failed"}
```

Do not reproduce any of these:

- ❌ CN left at the rcgen default (`"rcgen self signed cert"`) or any library placeholder.
- ❌ CN and the first DNS SAN disagree.
- ❌ Identity placed only in the CN with no DNS SAN.
- ❌ Identity placed in a non-DNS SAN (URI/IP/email).
- ❌ Regenerating the keypair on every enrollment/renewal (changes the SPKI → new/unknown station).

(perchpub now prefers the SAN over the CN when authorizing, so a correct DNS SAN alone unblocks issuance — but §3.2's CN==SAN rule is still required so the CSR is self-consistent and the leaf carries a meaningful CN.)

---

## 11. Reference implementation (rcgen)

perchstation uses **rcgen**. The exact API differs across rcgen versions; the **invariants** (§3) are what matter. A representative builder:

```rust
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType, PKCS_ED25519};

fn build_csr(identity: &str) -> anyhow::Result<(String /* csr_pem */, KeyPair /* persist this */)> {
    // 1. Keypair — Ed25519 (default). For P-256 use &PKCS_ECDSA_P256_SHA256.
    let key_pair = KeyPair::generate_for(&PKCS_ED25519)?;

    // 2. Params: DNS SAN == identity, and CommonName == the same identity.
    let mut params = CertificateParams::new(vec![identity.to_string()])?; // sets the dNSName SAN
    // Defensive: ensure exactly the SAN we intend (some versions seed extras).
    params.subject_alt_names = vec![SanType::DnsName(identity.try_into()?)];

    // 3. Overwrite rcgen's default DN so CN is NOT "rcgen self signed cert".
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, identity);
    params.distinguished_name = dn;

    // 4. Serialize the CSR (self-signed PoP with the key). PEM out.
    let csr = params.serialize_request(&key_pair)?;
    Ok((csr.pem()?, key_pair))
}
```

> **Version note.** On older rcgen (≤0.12) the shape is `Certificate::from_params(params)` + `cert.serialize_request_pem()`, with `params.alg = &PKCS_ED25519`. Whatever the version: (a) set `distinguished_name` CN, (b) set a `DnsName` SAN equal to the CN, (c) pick the Ed25519/P-256 algorithm. **Never rely on the default DN.**

Persist `key_pair` (its private key) — it is the station identity (§2, §5).

---

## 12. Conformance / acceptance tests

A CSR produced by perchstation **MUST** pass all of these. Use these as automated tests in the perchstation repo.

### 12.1 Structural assertions (parse the CSR)
- Parses as a valid PKCS#10 request.
- Public key algorithm ∈ { Ed25519, EC P-256 }.
- Subject CommonName is present, is **not** `"rcgen self signed cert"`, and **equals** the first DNS SAN.
- SubjectAltName contains ≥1 `dNSName`; the identity is DNS-valid (RFC 1123).
- Self-signature (proof of possession) verifies.

### 12.2 OpenSSL reference checks
```bash
# Inspect:
openssl req -in device.csr -noout -text
#   Subject: CN = station-7f3a9c
#   Requested Extensions: X509v3 Subject Alternative Name: DNS:station-7f3a9c
#   Public Key Algorithm: ED25519        (or id-ecPublicKey / prime256v1)
#   Signature Algorithm: ED25519         (or ecdsa-with-SHA256)

# Proof of possession:
openssl req -in device.csr -noout -verify
#   → "Certificate request self-signature verify OK"
```

### 12.3 Golden reference CSR (equivalent to the rcgen output)
The rcgen CSR must be structurally equivalent to this OpenSSL-built one:
```bash
openssl genpkey -algorithm ed25519 -out device.key
openssl req -new -key device.key \
  -subj "/CN=station-7f3a9c" \
  -addext "subjectAltName=DNS:station-7f3a9c" \
  -out device.csr
```

### 12.4 End-to-end (against a perchpub/step-ca dev stack)
- POST the CSR through `confirm` (§4) → expect `200` with `certificate_pem`.
- The returned leaf MUST: have public-key algorithm matching the CSR; carry SAN `DNS:<identity>`; chain to the device intermediate; have `SHA256(SPKI)` equal to the CSR key's SPKI hash.
- Present the leaf+key to `:8443 /api/v1/upload/` → expect a non-`401` response (auth accepted).
```

