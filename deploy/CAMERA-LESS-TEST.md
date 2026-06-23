# Camera-less enroll + upload test (against live perchpub)

End-to-end smoke of a station against the production perchpub at
`https://api.perchpub.net` **with no camera on the Pi**: enroll from a
file-based QR, then upload a video by hand-injecting it into the queue.

**Enrollment verified 2026-06-23** (device-CA-only QR + system-trust edge): the
station's contract-conformant CSR (CN == DNS SAN) is accepted by live
perchpub/step-ca and a leaf is issued (`enrollment.persisted`) — **the §10 `502`
is cleared**. The CSR is built by `enroll` at runtime (the QR carries only
`session_id` / `auth_token` / `ca_chain_pem`), so this confirms the
`cert-contract-conformance` fix regardless of which session minted the QR. The
**upload half (§4)** still wants one live run; do it before the issued leaf
expires (step-ca default ≈ 24 h, §8).

---

## Topology you're testing against

Traefik edge (`docker-compose.prod.yml`):

- `:443` — HTTPS via **Let's Encrypt**. Handles `enrollment/create` and
  `enrollment/confirm`. The station validates this edge against **system
  trust** (public roots), like any HTTPS client — the QR does **not** carry
  the LE intermediate.
- `:8443` — station **mTLS** for `/api/v1/upload/`
  (`RequireAndVerifyClientCert` against the **device CA**).

Station certs are signed by perchpub's internal **device CA**
(`PerchPub CA Intermediate CA`). The `confirm` client validates the public
`:443` edge against system trust and adds the QR's CA only as an extra anchor,
so the QR carries the **device CA only**:

```
QR ca_chain_pem  =  device CA  (verifies the issued station leaf in
                    validate_chain, and additively anchors :8443 upload trust)
```

No LE intermediate (`YR2`) in the QR, and nothing breaks when Let's Encrypt
rotates its intermediates (device-cert contract §7).

---

## Prereqs

- `jq`, `openssl`, `csplit`, `curl` on the Pi (default on Raspberry Pi OS).
- Your compiled `perchstation` client on the Pi.
- A test `.mp4` on the Pi, `>0` and `≤ 50 MiB` (`MAX_UPLOAD_BYTES`).

---

## 1. One-time: put `mint-enroll-qr` on the Pi

`cargo zigbuild -p perchstation` builds **all** its binaries, so this also
ships with your normal cross-build. On the dev host:

```sh
cd /path/to/perchstation
cargo zigbuild --release -p perchstation --bin mint-enroll-qr --target aarch64-unknown-linux-gnu
scp target/aarch64-unknown-linux-gnu/release/mint-enroll-qr perchpi:~/ps-test/mint-enroll-qr
```

Mint **on the Pi** so the QR, inputs, and `enroll` all live on one box — the
create-token lives ~5 min and must not be spent on a file transfer.

---

## 2. One-time: save the device CA  (`~/ps-test/ca-chain.pem`)

The **device CA is stable**, so fetch it once and keep it. Only redo this if
the device CA itself changes — the `:443` LE intermediate no longer matters
(the station validates that edge against system trust).

```sh
TOKEN=$(curl -s -X POST https://api.perchpub.net/api/v1/login/access-token --data-urlencode 'grant_type=password' --data-urlencode 'username=teschmitt@gmail.com' --data-urlencode 'password=12345678901234567890' | jq -r .access_token)

mkdir -p ~/ps-test

# device CA = ca_chain_pem from a THROWAWAY confirm (burns one session;
# :443 is publicly trusted so plain curl reaches it). Use a CONFORMANT CSR
# (CN == DNS SAN) so perchpub/step-ca issues rather than 502'ing (§3/§10).
S=$(curl -s -X POST https://api.perchpub.net/api/v1/enrollment/create \
     -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"latitude":49.898,"longitude":8.837}')
SID=$(echo "$S" | jq -r .session_id); ATOK=$(echo "$S" | jq -r .auth_token)
openssl genpkey -algorithm ED25519 -out /tmp/t.key
openssl req -new -key /tmp/t.key -subj "/CN=station-throwaway" \
  -addext "subjectAltName=DNS:station-throwaway" -out /tmp/t.csr
curl -s -X POST "https://api.perchpub.net/api/v1/enrollment/confirm/$SID" \
  -H 'Content-Type: application/json' \
  --data "$(jq -n --arg t "$ATOK" --arg c "$(cat /tmp/t.csr)" '{auth_token:$t,csr_pem:$c}')" \
  | jq -r .ca_chain_pem > ~/ps-test/ca-chain.pem
#  expect a real cert (subject O=PerchPub CA, CN=PerchPub CA Intermediate CA);
#  "null" here means perchpub's issuance is broken — fix that first.
```

---

## 3. Each run: write config (once), mint QR, enroll — back to back

```sh
# config (once is enough; data_dir + perchpub_url, nothing else needed for file-source enroll)
printf 'perchpub_url = "https://api.perchpub.net"\ndata_dir = "%s/ps-test/data"\n' "$HOME" \
  > ~/ps-test/config.toml
mkdir -p ~/ps-test/data
chmod +x ~/ps-test/mint-enroll-qr

# Delete any stale QR first: if `create`/`mint` fail, a leftover /tmp/qr.png
# would be silently re-used by `enroll` below (it happily enrolls an old,
# still-valid session — a confusing footgun).
rm -f /tmp/qr.png

# fresh session (token ~5 min, single-use; minting does NOT spend it). If this
# prints `{"detail":"Could not validate credentials"}`, your $TOKEN expired —
# re-run the login/access-token step in §2 to refresh it, then retry.
curl -s -X POST https://api.perchpub.net/api/v1/enrollment/create \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"latitude":49.898,"longitude":8.837}' | tee /tmp/create.json

# mint the QR, then ENROLL immediately — chained with && so a mint failure
# (e.g. a token error above) aborts instead of enrolling with a stale QR.
~/ps-test/mint-enroll-qr --create-response /tmp/create.json \
  --ca-chain ~/ps-test/ca-chain.pem --out /tmp/qr.png \
&& perchstation --config ~/ps-test/config.toml --log-format text \
     enroll --qr-source file --qr-file /tmp/qr.png
```

Success log: `enrollment.qr_decoded → csr_generated → sent → persisted`.
Creds land in `~/ps-test/data/credentials/` (`station.key` 0600, `station.crt`,
`ca_chain.pem`, `identity.json`).

---

## 4. Each run: serve + inject the video + verify

```sh
# terminal A — delivery loop. A 'capture.init_failed' warning is EXPECTED with
# no camera/GPIO; delivery runs anyway (FR-012).
perchstation --config ~/ps-test/config.toml --log-format text serve

# terminal B — hand-inject your clip (media FIRST, sidecar SECOND; the runner
# keys on the .json and ignores a sidecar-less .mp4)
SRC=/path/to/your.mp4
Q=~/ps-test/data/queue/pending
ID="$(date -u +%Y%m%dT%H%M%SZ)-001"          # format is sort-order only, not validated
cp "$SRC" "$Q/$ID.mp4"
B=$(stat -c%s "$Q/$ID.mp4"); N=$(date -u +%Y-%m-%dT%H:%M:%SZ)
printf '{"clip_id":"%s","captured_at":"%s","enqueued_at":"%s","byte_size":%s,"attempts":0}\n' \
  "$ID" "$N" "$N" "$B" > "$Q/$ID.json"
```

Within ~50 ms (`IDLE_TICK`): `delivery.attempt_started →
delivery.upload_succeeded (classify_task_id) → classify.terminal`. Confirm:

```sh
perchstation --config ~/ps-test/config.toml status      # Last success + queue depth 0
ls ~/ps-test/data/queue/delivered/                      # <clip_id>.json present, .mp4 gone
```
plus the matching upload + classify task on the perchpub side.

---

## Troubleshooting

| Symptom (from `enroll`/`serve`) | Cause | Fix |
|---|---|---|
| `enrollment.failed … 502 {"detail":"certificate issuance failed"}` | **perchpub** accepted the CSR but failed to sign it (app-level error, not the station). The station now sends a conformant CSR (CN == DNS SAN), so the §10 CN/SAN mismatch is ruled out — a remaining 502 is perchpub-side. | On the perchpub host: `grep -rn "certificate issuance failed" <repo>`, read the traceback in its logs, and check the device-CA **signing key** loads and its public key matches `PerchPub CA Intermediate CA` (`openssl x509 -pubkey` vs `openssl pkey -pubout`, hashes must match). |
| TLS / verify error at `sent` | the public `:443` edge cert failed **system-trust** validation (e.g. wrong system clock, or a corporate MITM/proxy in front) | fix system time / system trust store; the QR no longer pins the edge, so a rotated LE intermediate is *not* the cause anymore. |
| `502` with a generic/HTML body | Traefik can't reach the perchpub backend | perchpub app down / mis-routed; bring it up. |
| `session invalid (422)` | create-token expired or already used | Re-run step 3 (fresh `create.json`), enroll promptly. |
| `create` prints `{"detail":"Could not validate credentials"}`; mint then errors `did not return a session …` | the login bearer `$TOKEN` expired/invalid (not a station fault) | Re-run the login/access-token step in §2 to refresh `$TOKEN`, then redo step 3. **Beware**: if you skip the `rm -f /tmp/qr.png` guard, `enroll` will silently re-use a leftover QR from a prior run. |
| mint errors "payload too large" | the CA chain is too big for the QR | the QR carries the **device CA only** (intermediate [+ root]); drop any public/edge certs. |
| upload never starts | clip is 0-byte or `>50 MiB`, or sidecar missing/invalid | Check size; ensure `.mp4` then `.json` both present and the JSON parses. |

## Why these choices (so future-you doesn't relearn it)

- **Device-CA-only QR**: `confirm` validates the public `:443` edge against
  system trust (public roots on) and adds the QR's CA only as an extra anchor,
  so the QR carries just the device CA — used by `validate_chain` to verify the
  issued station leaf and (after enrollment) as the additive `:8443` upload
  server-trust anchor. No LE intermediate, no breakage on LE rotation. See
  `crates/perchstation-core/src/enrollment/confirm.rs` +
  `src/tls.rs::rustls_builder_for_upload`.
- **Mint on the Pi**: avoids shuttling a PNG between machines inside the
  ~5-minute token window.
- **Queue injection, no code**: `serve`'s delivery loop uploads anything in
  `queue/pending/` (`<id>.mp4` + `<id>.json`); the capture/camera path is never
  needed. Upload trust = public roots + the enrolled device CA
  (`src/tls.rs::rustls_builder_for_upload`), station presents its `station.crt`
  for mTLS.
