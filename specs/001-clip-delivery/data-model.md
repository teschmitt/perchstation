# Phase 1 Data Model: Clip Delivery Subsystem

**Feature**: 001-clip-delivery
**Date**: 2026-05-27

The delivery subsystem persists four kinds of state on disk and surfaces
one ephemeral view in the operator CLI. Storage is the local filesystem
(see `research.md` R-6); there is no database. All on-disk JSON is written
as `tmp` files and atomically renamed into place.

```text
<data_dir>/
├── credentials/
│   ├── station.key          # PEM, Ed25519 private key, mode 0600
│   ├── station.crt          # PEM, enrollment-issued cert
│   ├── ca_chain.pem         # PEM, perchpub CA chain
│   └── identity.json        # StationIdentity metadata
└── queue/
    ├── pending/
    │   ├── <clip-id>.mp4
    │   └── <clip-id>.json   # ClipQueueEntry
    ├── inflight/
    │   ├── <clip-id>.mp4
    │   └── <clip-id>.json
    └── delivered/
        └── <clip-id>.json   # ClipQueueEntry (terminal), no media
```

`<clip-id>` is `<capture_utc_rfc3339_basic>-<seq>` where `seq` is a
monotonically increasing zero-padded counter from a per-process atomic
that survives nothing — collisions across reboots are resolved by the
timestamp prefix, and within a millisecond by the seq.

---

## Entity: StationIdentity

**File**: `credentials/identity.json`

**Lifecycle**: written once at end of enrollment; mutated only by an
operator-initiated re-enroll (which logs the conflict and refuses to
overwrite unless `--force` was passed — see FR-003).

**Fields**:

| Field             | Type                     | Notes                                            |
| ----------------- | ------------------------ | ------------------------------------------------ |
| `station_id`      | UUID                     | Returned by perchpub at enrollment time          |
| `enrolled_at`     | RFC 3339 UTC timestamp   | When the cert+key were written                   |
| `perchpub_url`    | string (HTTPS URL)       | The URL enrollment was performed against         |
| `cert_not_after`  | RFC 3339 UTC timestamp   | Parsed once at write time, surfaced in `status`  |

Companion PEM files: `station.key` (mode `0600`, never read by `status`),
`station.crt`, `ca_chain.pem`.

**Invariants**:
- `station.key`, `station.crt`, `ca_chain.pem`, and `identity.json` all
  exist together or none of them do. Enrollment writes the four atomically
  by staging them in `credentials.tmp/` and renaming the directory into
  place (`renameat2`).
- The private key is loaded exactly once at process start and held in
  memory inside `rustls::sign::SigningKey` for the lifetime of the
  process.

---

## Entity: ClipQueueEntry

**File**: `queue/{pending|inflight|delivered}/<clip-id>.json`

**Lifecycle state machine**:

```text
                  ┌─────────┐  retry budget OK
captured  ─────► │ Pending │ ◄──────────────┐
                  └────┬────┘                 │
                       │ delivery loop picks  │
                       ▼                      │
                  ┌──────────┐  transient err │
                  │ Inflight │ ───────────────┘
                  └────┬─────┘
                       │ 200 OK         │ terminal err
                       ▼                ▼
                  ┌────────────┐   ┌───────────────┐
                  │ Delivered  │   │ Undeliverable │
                  └────────────┘   └───────────────┘
```

State is encoded by which directory the file lives in (`Pending`/`Inflight`)
plus, for entries that have reached `delivered/`, the `outcome` field
distinguishing `Delivered` from `Undeliverable`. Crash recovery on boot
re-queues `inflight/` → `pending/`.

**Fields**:

| Field                 | Type                                     | Required | Notes                                                                       |
| --------------------- | ---------------------------------------- | -------- | --------------------------------------------------------------------------- |
| `clip_id`             | string                                   | ✓        | Matches the filename stem                                                   |
| `captured_at`         | RFC 3339 UTC timestamp                   | ✓        | From capture; best-effort if clock unreliable                               |
| `enqueued_at`         | RFC 3339 UTC timestamp                   | ✓        | When delivery first saw the clip                                            |
| `byte_size`           | u64                                      | ✓        | Validated against actual file size before upload                            |
| `attempts`            | u32                                      | ✓        | Incremented on every transition Pending → Inflight                          |
| `first_attempt_at`    | RFC 3339 UTC timestamp                   | optional | Set on first Inflight transition                                            |
| `last_attempt_at`     | RFC 3339 UTC timestamp                   | optional | Updated on every Inflight transition                                        |
| `last_error`          | object `{kind, status?, message}`        | optional | Cleared on success; populated on every failed attempt                       |
| `next_attempt_after`  | RFC 3339 UTC timestamp                   | optional | Set by the backoff scheduler on transient failure                           |
| `outcome`             | enum `Delivered`/`Undeliverable`         | optional | Present only in `delivered/`                                                |
| `classify_task_id`    | UUID                                     | optional | Present on `outcome=Delivered`                                              |
| `delivered_at`        | RFC 3339 UTC timestamp                   | optional | Present on `outcome=Delivered`                                              |
| `last_classify_status`| enum (`ClassifyTaskStatus`)              | optional | Latest observed terminal status from polling `GET /classify-task/{id}`      |

**Invariants**:
- Whenever the entry's sidecar lives in `pending/` or `inflight/`, the
  matching `.mp4` exists alongside it.
- On the transition into `delivered/`, the `.mp4` is unlinked **before**
  the sidecar is renamed — so a crash mid-transition leaves either
  (clip+sidecar in `inflight/`, recover by re-queuing) or (sidecar only in
  `delivered/`, recover by trusting the terminal state).
- `attempts` never decreases.
- `next_attempt_after` is meaningful only while the entry is in
  `pending/`.

---

## Entity: DeliveryOutcome (view)

Not a stored entity. A *projection* of `ClipQueueEntry` records joined
with the most recent `classify-task` poll result, used by `status` and by
the structured-log emitter when a poll observes a terminal transition.

**Computed fields**:

| Field                  | Source                                                                       |
| ---------------------- | ---------------------------------------------------------------------------- |
| `clip_id`              | `ClipQueueEntry.clip_id`                                                     |
| `state`                | Derived from directory + `outcome`                                           |
| `classify_task_id`     | `ClipQueueEntry.classify_task_id` if present                                 |
| `classify_status`      | `last_classify_status` (or `Prepared` immediately after a successful upload) |
| `observation_id`       | From the most recent `GET /classify-task/{id}` response (if non-null)        |

The classify-task poller updates `last_classify_status` on every poll. It
runs alongside the delivery loop, polls `delivered/` entries whose
`classify_status` is not yet terminal, and uses the same retry/backoff
machinery as upload but with longer ceilings (classify processing may
take minutes).

---

## Entity: EnrollmentSessionMaterial

**Lifetime**: in-memory only. Created when the QR is decoded; discarded
immediately after the enrollment exchange completes (success or failure).
Never written to disk.

**Fields**:

| Field        | Type        | Notes                                                  |
| ------------ | ----------- | ------------------------------------------------------ |
| `session_id` | UUID        | From the QR payload; matches perchpub's `EnrollmentSession.session_id` |
| `auth_token` | string      | From the QR payload; passed in the body of `/enrollment/confirm`       |
| `decoded_at` | UTC instant | Used for an in-process "session is stale" sanity check; not authoritative |

**QR payload format** (proposed): `application/json` text embedded in the
QR, exactly the perchpub `EnrollmentSession` shape minus `expires_at` (the
station doesn't need to enforce expiry — perchpub does):

```json
{"session_id":"…","auth_token":"…"}
```

`expires_at` may be present and is ignored by the station.

---

## Configuration (parsed view)

Not persisted by the delivery subsystem (the operator authors it). Loaded
once at process start; not hot-reloaded. See `research.md` R-10 for the
TOML schema and defaults.

| Field                              | Type   | Default                          |
| ---------------------------------- | ------ | -------------------------------- |
| `perchpub_url`                     | URL    | none — required at runtime       |
| `data_dir`                         | path   | `/var/lib/perchstation`          |
| `queue.max_clips`                  | u32    | 500                              |
| `queue.max_bytes`                  | u64    | 2 GiB                            |
| `queue.eviction`                   | enum   | `drop_oldest_undelivered`        |
| `retry.initial_delay_secs`         | u64    | 10                               |
| `retry.max_attempt_delay_secs`     | u64    | 3600                             |
| `retry.per_clip_max_attempts`      | u32    | 12                               |
| `retry.per_clip_max_wallclock_hours`| u64   | 24                               |

---

## Mapping to spec requirements

| Spec entity                  | Maps to                                                       |
| ---------------------------- | ------------------------------------------------------------- |
| Station Identity             | `StationIdentity` + sibling PEMs                              |
| Clip Queue Entry             | `ClipQueueEntry` + its `.mp4` sibling                         |
| Delivery Outcome             | `DeliveryOutcome` view (state machine + classify status)      |
| Enrollment Session Material  | `EnrollmentSessionMaterial` (in-memory)                       |
