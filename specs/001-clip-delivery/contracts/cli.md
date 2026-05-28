# Contract: `perchstation` operator CLI

**Direction**: operator → station (local, via SSH or console).
**Binary**: `perchstation` (single binary, `clap`-defined subcommands).

The CLI is the operator's only first-class interface to the station. It
covers the three things an owner ever needs to do: enrol the device once,
run the delivery service, and ask the device what state it is in.

```text
perchstation [--config <path>] [--log-format <json|text>] [--log-level <lvl>] <subcommand>

Subcommands:
  enroll    Pair this station with a perchpub instance (one-shot, interactive).
  serve     Run the delivery loop and classify-task poller (long-lived daemon).
  status    Print a snapshot of delivery health and exit.
  help      Print help.
```

Global flags:

| Flag             | Type    | Default                          | Notes                                                                  |
| ---------------- | ------- | -------------------------------- | ---------------------------------------------------------------------- |
| `--config`       | path    | `/etc/perchstation/config.toml`  | Optional in dev (uses defaults if missing); required by `serve` in prod. |
| `--log-format`   | enum    | `json`                           | `text` is intended for interactive SSH use.                              |
| `--log-level`    | string  | `info`                           | Standard `tracing` filter syntax.                                      |
| `-h`/`--help`    | —       | —                                |                                                                        |
| `-V`/`--version` | —       | —                                |                                                                        |

Exit codes (global):

| Code | Meaning                                                  |
| ---- | -------------------------------------------------------- |
| 0    | Success                                                  |
| 64   | Usage error (bad flags, missing required arg)            |
| 70   | Configuration error (config file unreadable, invalid)    |
| 74   | I/O error reading/writing on-disk state                  |
| 75   | Transient subsystem failure (only meaningful from `serve` exit) |
| 76   | Unrecoverable state (e.g. enrollment cert expired)       |

---

## `perchstation enroll`

```text
perchstation enroll [--qr-source <camera|file>] [--qr-file <path>] [--force]
```

Pairs an unprovisioned station with perchpub. The operator initiates an
enrollment session from the perchpub web UI; perchpub displays a QR code.

Flags:

| Flag           | Type | Default    | Notes                                                                                |
| -------------- | ---- | ---------- | ------------------------------------------------------------------------------------ |
| `--qr-source`  | enum | `camera`   | `camera` uses the on-board camera; `file` reads a PNG/JPEG (recovery path).          |
| `--qr-file`    | path | —          | Required when `--qr-source=file`.                                                    |
| `--force`      | bool | false      | Permit overwriting existing on-disk credentials; **logs prominently**, never silent. |

Behaviour:

1. Verify no `credentials/identity.json` exists. If one does and `--force`
   is not set, exit `76` with a clear message naming the existing
   `station_id` and `cert_not_after`.
2. Acquire one QR frame from the configured source.
3. Decode the QR; parse the JSON payload to `EnrollmentSessionMaterial`.
4. Generate an Ed25519 keypair in memory; build a CSR; serialise the CSR
   to PEM.
5. Call `POST /api/v1/enrollment/confirm/{session_id}` with `auth_token`
   and `csr_pem`.
6. On `success == true`, validate that the returned `certificate_pem`
   chains to `ca_chain_pem` and matches the held private key. Persist the
   four files in `credentials/` atomically.
7. Print a single human-readable confirmation line including the
   `station_id`. Exit `0`.

Failures, in priority order:
- Existing identity present, no `--force`: exit `76`.
- QR not found / undecodable: exit `74`.
- Network/TLS error reaching perchpub: retry per the enrollment-tier
  schedule in `contracts/perchpub-api.md`; after exhaustion, exit `75`.
- `EnrollmentResponse.success == false`: exit `76`, print `reason`.
- Cert/CA validation failure: exit `74`, do not write credentials.

---

## `perchstation serve`

```text
perchstation serve
```

Runs the delivery loop and the classify-task poller as a single long-lived
process. Designed to be launched by systemd (`Type=notify`). No flags
beyond the global ones.

Behaviour:

1. Load config. Bail with exit `70` if perchpub URL is missing.
2. Load `credentials/`. If absent, exit `76` ("not enrolled").
3. Reconcile queue state: rename anything in `inflight/` back to `pending/`.
4. Notify systemd `READY=1`.
5. Loop: pick the oldest `pending/` entry not blocked by
   `next_attempt_after`; transition to `inflight/`; attempt upload;
   transition to `delivered/` (Delivered or Undeliverable) per the
   per-clip rules in `contracts/perchpub-api.md`.
6. In parallel, poll `delivered/` entries whose `last_classify_status` is
   not terminal.
7. On SIGTERM, drain the in-flight upload (if any) for up to 30 s, then
   exit `0`.

The `serve` command is the only long-lived process; everything else is
one-shot.

---

## `perchstation status`

```text
perchstation status [--json]
```

Prints a snapshot of delivery health and exits.

Flags:

| Flag      | Type | Default | Notes                                       |
| --------- | ---- | ------- | ------------------------------------------- |
| `--json`  | bool | false   | Emit a single JSON object instead of text.  |

Default (human-readable) output:

```text
Enrollment:    OK (station 7f3e..., cert expires 2027-04-12)
Queue depth:   3 clips (12.4 MB on disk)
Last success:  2026-05-27 06:31:08 UTC  (4m ago)
Last failure:  2026-05-26 22:14:55 UTC  perchpub 503
Last 3 deliveries:
  2026-05-27 06:31  clip_00214.mp4  classify=Success
  2026-05-27 06:28  clip_00213.mp4  classify=Processing
  2026-05-27 06:25  clip_00212.mp4  classify=Queued
```

JSON output schema (`--json`):

```json
{
  "enrollment": {
    "state": "ok",                              // "ok" | "missing" | "expired"
    "station_id": "7f3e...",                    // null if state != "ok"
    "cert_not_after": "2027-04-12T00:00:00Z",   // null if state != "ok"
    "perchpub_url": "https://perchpub.example.org"
  },
  "queue": {
    "pending": 3,
    "inflight": 0,
    "bytes_on_disk": 13_000_000
  },
  "last_success": {
    "at": "2026-05-27T06:31:08Z",
    "clip_id": "20260527T063108Z-001",
    "classify_task_id": "1a2b...",
    "classify_status": "Success"
  },
  "last_failure": {
    "at": "2026-05-26T22:14:55Z",
    "clip_id": "20260526T221455Z-007",
    "kind": "http_status",
    "status": 503,
    "message": "Service Unavailable"
  },
  "recent": [
    { "clip_id": "20260527T063108Z-001", "classify_status": "Success",   "delivered_at": "2026-05-27T06:31:08Z" },
    { "clip_id": "20260527T062800Z-001", "classify_status": "Processing","delivered_at": "2026-05-27T06:28:00Z" },
    { "clip_id": "20260527T062500Z-001", "classify_status": "Queued",    "delivered_at": "2026-05-27T06:25:00Z" }
  ]
}
```

Behaviour:

- `status` is read-only with respect to `data_dir`; safe to run alongside
  `serve`.
- If `credentials/` is missing, exit `0` with `enrollment.state="missing"`
  and zero counts.
- Exits `74` only on filesystem errors reading the data dir.

---

## Test obligations

Each subcommand has at least one black-box integration test that invokes
the binary via `assert_cmd` against a temporary `data_dir` and a fake
perchpub. The acceptance scenarios in `spec.md` US3 map directly to
`status`-output assertions in `tests/integration/status_surface.rs`.
