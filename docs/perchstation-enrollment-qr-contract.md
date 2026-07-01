# PerchStation enrollment-QR contract

**Audience:** an agent implementing the enrollment **QR generation / display**
in the **perchpub** repo (server side) — the web UI or tooling that renders the
QR an operator points the station's camera at.

**Authority:** this document describes the payload and symbol that
**perchstation decodes** during `enroll`. The source of truth is the
perchstation decoder; where this prose disagrees with the code, the code wins.
Verified against:

- decoder + wire struct — `crates/perchstation-core/src/enrollment/mod.rs:67-129`
- perchpub URL source (**not** the QR) — `crates/perchstation/src/commands/enroll.rs:38-41`
- CA-chain pin / cross-check — `crates/perchstation-core/src/enrollment/confirm.rs:266-405`
- reference generator (dev-only) — `crates/perchstation/src/bin/mint-enroll-qr.rs:58-86`

Keywords **MUST / MUST NOT / SHOULD / SHOULD NOT / MAY** are used per RFC 2119.

This is the QR-side companion to the
[device-certificate contract](./perchstation-csr-contract.md); read §3 and §7
there for how the decoded CA chain is used after enrollment.

---

## 1. Payload — exact shape

The QR symbol **MUST** encode, as its data segment, the **raw UTF-8 bytes of a
single JSON object**. Not a URL, not base64-of-JSON, not CBOR — the JSON text
itself.

```json
{"session_id":"550e8400-e29b-41d4-a716-446655440000","auth_token":"<opaque secret string>","ca_chain_pem":"-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----\n"}
```

| Field (wire name) | Type | Required | Rule perchstation enforces |
|---|---|---|---|
| `session_id` | string | **MUST** | Parsed with `Uuid::parse_str`; MUST be a canonical UUID. Bad value → decode fails (`BadField`). `mod.rs:115` |
| `auth_token` | string | **MUST** | MUST be present and non-empty. `mod.rs:109` |
| `ca_chain_pem` | string | **MUST** | MUST be present and non-empty; PEM device-CA chain (see §3). `mod.rs:112` |
| `expires_at` | string | MAY | Accepted and **ignored** — perchstation does not enforce TTL, perchpub does. `mod.rs:72` |
| *(any other field)* | — | MAY | **Silently ignored** — the struct has no `deny_unknown_fields`. Safe to add fields; they will not be read. |

- Field **order is irrelevant** (JSON object). Whitespace / pretty-printing is
  tolerated by the decoder, but you **SHOULD** emit compact JSON to conserve QR
  capacity (§4).
- PEM newlines are ordinary JSON `\n` escapes inside the string.
- Keep the payload **ASCII** (UUID + token + base64 PEM naturally are). It MAY be
  UTF-8; the decoder treats the QR bytes as UTF-8 and fails with `NotUtf8`
  otherwise.

---

## 2. What is **not** in the QR

- **No `perchpub_url`.** perchstation ignores it if present and reads the base
  URL from `config.toml::perchpub_url` instead (`enroll.rs:38-41`,
  `confirm.rs:90`). You **MAY** include a `perchpub_url` field for humans / other
  clients, but the station will **not** read it. Making the station take the URL
  from the QR is a **perchstation** change (a new `QrPayload` field) — out of
  scope for the display code; flag it separately if desired.
- **No keypair, no CSR.** The station generates those locally after decoding.

---

## 3. `ca_chain_pem` semantics — the interop constraint

`ca_chain_pem` is the **device-CA chain** (step-ca): the issuing **intermediate**,
optionally **+ root**. It is **device-CA only** — do **NOT** put the
public / Let's Encrypt `:443` edge intermediate in it (`mint-enroll-qr.rs:38-43`,
`confirm.rs:8-15`). The `:443` enrollment edge is validated by perchstation
against system / public trust; the QR's CA is added only as an *additive* anchor
and as the pin for the issued leaf.

perchstation cross-checks the QR chain against the `confirm` response
(`validate_chain`, `confirm.rs:329-405`):

1. **Superset rule.** Every cert perchpub returns in the `confirm` response's
   `ca_chain_pem` **MUST** also appear in the QR's `ca_chain_pem` (response ⊆ QR,
   compared by DER). A response cert absent from the QR → `ChainMismatch`,
   enrollment aborts. `confirm.rs:345-351`
2. **Issuer present.** The issued leaf **MUST** verify against a cert in the QR
   chain that is a *usable CA* (`cA=TRUE` **and** `keyUsage.keyCertSign`). step-ca
   signs leaves with the **intermediate**, so the QR chain **MUST contain that
   intermediate**. A root-only QR chain fails. `confirm.rs:391-401`

**Simplest correct implementation: embed in the QR the exact same device-CA chain
your `confirm` endpoint returns in `ca_chain_pem`.** Equal always satisfies both
rules. (This is what the reference generator and its round-trip test do.)

---

## 4. QR symbol parameters

- **One symbol only.** The decoder detects grids and takes the **first**
  (`img.detect_grids().into_iter().next()`, `mod.rs:102`). Multi-part / animated
  QR is **not** supported ("frame" = one camera still, not a QR sequence). Render
  exactly one QR and keep other QRs out of the camera's view.
- **Capacity ceiling: ~2953 bytes** (single QR, byte mode, EC-L, version 40). The
  whole JSON **MUST** fit in one symbol. The CA chain dominates the size:
  - Use **Ed25519 / EC-P256** CA certs (a few hundred bytes PEM each). Two **RSA**
    CA certs can exceed the budget.
  - Prefer **intermediate-only** (still satisfying §3) or intermediate+root with
    small keys.
  - The reference generator errors if the payload exceeds capacity
    (`mint-enroll-qr.rs:73-78`); mirror that guard server-side and surface a clear
    "CA chain too large" error rather than emitting an unscannable symbol.
- **Error correction:** the decoder (`rqrr`) imposes **no** EC-level or version
  requirement — it reads whatever you produce. The reference uses **EC-L** to
  maximise capacity. Because your QR is scanned off a **screen by the Pi camera**
  (not a clean file), you **SHOULD** use the *highest* EC level that still fits
  (`M`, ideally `Q`) for glare / blur robustness, falling back toward `L` only
  when the payload is too large.
- **Quiet zone:** include the standard ≥4-module quiet zone.
- **Contrast & size:** black-on-white, high contrast, generous module size on
  screen. If you also expose a downloadable image (for `enroll --qr-source file`),
  emit **PNG or JPEG** and keep dimensions well under **8192×8192** — perchstation
  rejects larger frames as a decompression-bomb guard (`mod.rs:89`). The reference
  renders 600×600 minimum.

---

## 5. Security

The QR embeds a **live single-use credential** (`auth_token`; perchpub session
TTL ≈ 300 s, ≤3 attempts — see [csr-contract §4.1](./perchstation-csr-contract.md)).
Treat the rendered image as a secret: do not log it, do not cache it in
shared / CDN layers, and let it expire with the session. perchstation registers
`auth_token` in its log-redaction registry on decode (`mod.rs:121`); do the
equivalent server-side.

---

## 6. Reference generator (authoritative, Rust)

This is `crates/perchstation/src/bin/mint-enroll-qr.rs:66-85` — the exact recipe
whose output is proven to round-trip through the real decoder (test
`mint-enroll-qr.rs:181-196`). Port this to your server stack:

```rust
let payload = serde_json::json!({
    "session_id": session_id,      // Uuid → serialises to the canonical string
    "auth_token": auth_token,
    "ca_chain_pem": ca_chain_pem,  // device-CA chain == what /confirm returns
});
let payload_bytes = serde_json::to_vec(&payload)?;          // compact UTF-8 JSON
let code = QrCode::with_error_correction_level(&payload_bytes, EcLevel::L)?; // errors if > ~2953 B
let image = code.render::<Luma<u8>>().min_dimensions(600, 600).quiet_zone(true).build();
// → PNG (grayscale) for the file path; for a web page render the same modules as <svg>/<img>.
```

Language-agnostic recipe:
`bytes = utf8(compact_json({session_id, auth_token, ca_chain_pem}))` → QR-encode
in **byte mode**, EC level per §4, one symbol, quiet zone on.

---

## 7. Conformance tests

Server-side, assert the generated symbol decodes back to the inputs — mirror
`mint-enroll-qr.rs:181-196`: build the QR, decode with any standard QR reader,
parse JSON, and check `session_id` / `auth_token` / `ca_chain_pem` survive the
round-trip.

End-to-end: the CA chain you embed **MUST** equal (or be a superset of, per §3)
the `ca_chain_pem` your `POST /api/v1/enrollment/confirm/{session_id}` returns,
and **MUST** contain the intermediate that signs the issued leaf.
