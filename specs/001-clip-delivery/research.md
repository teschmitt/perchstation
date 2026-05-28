# Phase 0 Research: Clip Delivery Subsystem

**Feature**: 001-clip-delivery
**Date**: 2026-05-27
**Status**: complete — no open `NEEDS CLARIFICATION`

This document records the load-bearing technical decisions for the delivery
subsystem and the alternatives that were considered. The constitution and
spec already pinned down language (Rust 2021), single async runtime (Tokio),
no telemetry, bounded queues, mTLS-authenticated calls to perchpub, and
QR-based enrollment via the station's camera. Everything below is downstream
of those givens.

## R-1. HTTP + mTLS client crate

**Decision**: `reqwest` 0.12 with the `rustls-tls` feature, configured with a
client identity built from the enrollment-issued certificate, the
on-device-generated private key, and the CA chain.

**Rationale**:
- Mature, widely deployed, actively maintained, AGPL-compatible (MIT/Apache).
- First-class multipart upload support — required by `POST /api/v1/upload/`,
  which is `multipart/form-data` with a single `file` part.
- First-class client-certificate (`reqwest::Identity`) support against the
  pure-Rust `rustls` backend; no OpenSSL in the dependency tree, which keeps
  cross-compilation to aarch64 painless.
- Streaming request bodies (`reqwest::Body::wrap_stream`) avoid loading clip
  bytes into RAM — important on a Pi Zero 2 W with 512 MB total.

**Alternatives considered**:
- `hyper` directly: more control, but we'd reimplement multipart and identity
  plumbing for no win.
- `ureq`: synchronous, simpler dependency footprint, but mixing with Tokio
  for the rest of the runtime would force blocking-on-async and burn an
  executor thread per upload — incompatible with the single-runtime rule.
- `isahc` (libcurl): pulls a C dependency tree; cross-compile complexity.

## R-2. TLS implementation

**Decision**: `rustls` (via the `rustls-tls` feature flag on `reqwest`),
backed by `ring` for crypto.

**Rationale**:
- Pure Rust → no `pkg-config`/`openssl-sys` cross-compile friction.
- Pinning a specific CA chain (the one perchpub returns at enrollment) is
  ergonomic with `rustls::RootCertStore::add`; we explicitly do **not** want
  the system trust store, because the station only ever talks to its
  enrolled perchpub.
- Client identity (`reqwest::Identity::from_pem`) accepts the cert+key bundle
  produced at enrollment time.

**Alternatives considered**:
- `native-tls` (OpenSSL/SChannel/SecureTransport): system-trust by default,
  but introduces the openssl dependency on Linux targets and works against
  the explicit "talk to one perchpub only" posture.

## R-3. On-device keypair + CSR generation

**Decision**: `rcgen` to generate an Ed25519 keypair and a corresponding
PKCS#10 CSR (PEM) signed with it. The private key never leaves the device:
it is held in memory during enrollment, serialised to PEM, and written to
`credentials/station.key` with mode `0600` (via `OpenOptions` +
`PermissionsExt`).

**Rationale**:
- `rcgen` is the de-facto Rust crate for cert/CSR plumbing on top of
  `rustls`; lightweight, no C dependencies.
- Ed25519 keys are short, generate fast (no entropy stalls on the Pi), and
  rustls supports them in TLS 1.3 client auth.

**Alternatives considered**:
- ECDSA P-256: also fine, marginally wider TLS-1.2 compatibility. Ed25519
  picked for simpler key handling and constant-time signing semantics. If
  perchpub's Traefik front rejects Ed25519 client certs, this is the first
  flag to flip — research note left in the contract doc.
- RSA-2048: 4× longer keygen on a Pi Zero 2 W; larger key material; nothing
  to gain over Ed25519 here.
- Hand-rolling CSR PEM from `ring`: pointless without a strong reason.

## R-4. QR code decoder

**Decision**: `rqrr` for decoding QR symbols from grayscale image frames.
The decoder runs in `perchstation-core` and consumes frames published by a
`QrFrameSource` trait. The trait's only production implementation lives in
`perchstation-hw` and wraps the Pi camera.

**Rationale**:
- Pure Rust, no C deps, MIT licence (AGPL-compatible).
- Operates on an in-memory `image::GrayImage`-equivalent; lets us run the
  decode path on the host against PNG fixtures.

**Alternatives considered**:
- `quircs`: also pure Rust; similar feature set. `rqrr` chosen for being the
  more actively maintained of the two and having a simpler API.
- `bardecoder`: heavier (supports many barcode formats); we only need QR.

## R-5. Camera capture on the Pi (deferred surface)

**Decision**: Defer the concrete camera implementation behind the
`QrFrameSource` trait. The delivery subsystem's MVP ships with a *file-watch*
implementation (drop a PNG/JPEG into a `qr-inbox/` directory and the
enrollment subcommand decodes it) plus a stub `perchstation-hw` adapter that
shells out to `libcamera-still --immediate --width 800 --height 600` and
reads the resulting JPEG. Replacing the shell-out with a proper libcamera
binding is a follow-up.

**Rationale**:
- Keeps the MVP unblocked. The shell-out works on every supported Pi today
  because Bookworm ships `libcamera-still`.
- Honours the constitution's "hardware at the boundary" principle: the
  decision can change without touching delivery code.
- The file-drop path is invaluable for development and operator recovery
  (an owner can scan the QR on a separate device and `scp` the PNG over).

**Alternatives considered**:
- Direct libcamera Rust bindings (`libcamera-rs`): immature, evolving API,
  not worth the dependency churn for a one-shot capture path.
- V4L2 via `nokhwa` or `v4l`: works on Pi 4 but Pi Zero 2 W's libcamera
  stack is the supported path on Bookworm; V4L2 compatibility shim is
  flaky.

## R-6. On-disk queue layout

**Decision** (user-confirmed): directory-per-state with atomic renames.

```text
<data_dir>/queue/
├── pending/    # waiting to upload (clip + sidecar JSON)
├── inflight/   # currently being uploaded
└── delivered/  # uploaded, classify-task id recorded; sidecar only
```

Clip lifecycle:
1. Capture or operator drops `clip-XXXX.mp4` into `pending/` along with a
   `clip-XXXX.json` sidecar (`tmp` write + `rename` for atomicity).
2. Delivery loop picks the oldest, `rename`s the pair into `inflight/`.
3. On upload success, the clip file is unlinked, the sidecar is updated with
   the `classify_task_id` and renamed into `delivered/`.
4. On terminal failure, the clip is unlinked and the sidecar is renamed
   into `delivered/` with `outcome: undeliverable` and the error context.

**Rationale**:
- `rename(2)` within a single filesystem is atomic on Linux. Crash at any
  point leaves the queue in a recoverable state: on boot, anything still in
  `inflight/` is re-queued by renaming it back into `pending/`.
- No DB → no schema migrations, no WAL tuning, no surprise journal-write
  amplification on the SD card. The total write volume per delivered clip
  is the clip file itself plus a couple of small sidecar writes — well
  inside Principle III's wear budget.
- Operator can inspect queue state with `ls`.

**Alternatives considered** (and rejected, per user decision): SQLite, sled.

## R-7. Retry and backoff strategy

**Decision**: Per-clip exponential backoff with jitter, bounded by both
attempt count and wall-clock budget.

| Knob                       | Default        | Configurable | Rationale                              |
| -------------------------- | -------------- | ------------ | -------------------------------------- |
| Initial delay              | 10 s           | yes          | Don't hammer perchpub on a blip        |
| Max single-attempt delay   | 1 h            | yes          | Wakes up at least hourly during outage |
| Backoff multiplier         | 2.0            | no           | Standard binary backoff                |
| Jitter                     | ±20 %          | no           | Avoid thundering herd if many stations |
| Per-clip attempt ceiling   | 12             | yes          | ~24 h of attempts at 1-h cap           |
| Per-clip wall-clock budget | 24 h           | yes          | After this, clip → undeliverable        |

Transient triggers (retry, do not consume terminal budget):
- Network errors (DNS, connect, TLS, IO timeout)
- HTTP 408, 425, 429, 500, 502, 503, 504
- HTTP 200 with malformed body

Terminal triggers (mark clip undeliverable):
- HTTP 4xx other than the transient list above
- Clip file unreadable / zero-length detected before upload (skipped, never
  retried)

**Rationale**: Satisfies FR-007, FR-008, FR-015. Caps cover both the "perchpub
down for a day" and the "perchpub permanently refuses this clip" pathologies.

## R-8. Eviction policy

**Decision**: `drop_oldest_undelivered` is the default and ships first.
`refuse_new` (backpressure) is the alternate strategy and is configurable.

**Rationale**:
- A backyard owner whose feeder is recording while perchpub is offline for a
  week will gladly trade the oldest clips for the most recent ones; the most
  recent clips are the ones the owner will look at first when service
  resumes.
- Backpressure is the wrong default in a system where there is no upstream
  to backpressure *to* — the capture subsystem will just drop frames anyway.
  But operators with a fixed-quota scenario (e.g., a metered uplink, or a
  research deployment that values older data) can opt in.

Eviction logs a single WARN per evicted clip with the original capture time,
size, and remaining queue depth.

## R-9. Structured logging

**Decision**: `tracing` + `tracing-subscriber` with a JSON formatter writing
one event per line to stderr (which `systemd-journald` consumes verbatim).

**Rationale**:
- Constitution Principle IV mandates structured, single-line-per-event,
  machine-and-human-readable logs to stderr/journald.
- `tracing` is the Rust ecosystem default; it supports spans (useful for the
  per-clip delivery loop), structured fields, and per-target level control.
- Journald handles rotation and size caps; we document the required
  `SystemMaxUse=` setting in `quickstart.md` rather than reimplementing
  rotation in-process.

A `--log-format=text` flag is provided for interactive debugging over SSH.

## R-10. Configuration format

**Decision**: TOML, parsed with `serde` + `toml`. A single file at
`/etc/perchstation/config.toml` by default; the path is overridable with
`--config`. Sensible defaults for every field; the file is optional for
a development run.

**Rationale**: TOML matches the Rust ecosystem's house style, is operator-
editable over SSH without a YAML-indent landmine, and integrates with
`serde::Deserialize` for zero per-field plumbing.

```toml
# /etc/perchstation/config.toml
perchpub_url = "https://perchpub.example.org"
data_dir     = "/var/lib/perchstation"

[queue]
max_clips     = 500
max_bytes     = 2_147_483_648  # 2 GiB
eviction      = "drop_oldest_undelivered"  # or "refuse_new"

[retry]
initial_delay_secs        = 10
max_attempt_delay_secs    = 3600
per_clip_max_attempts     = 12
per_clip_max_wallclock_hours = 24
```

## R-11. Daemon / process model

**Decision**: `perchstation serve` runs as a long-lived foreground process
under systemd. We ship a `systemd/perchstation.service` template (`Type=
notify` with `sd_notify(READY=1)` once the delivery loop has reconciled
state on boot). On a fresh install the operator runs `perchstation enroll`
once (interactive), then `systemctl enable --now perchstation`.

**Rationale**:
- Restart-as-recovery (Principle I): `Restart=always` with backoff makes any
  panic recover automatically.
- Journald integration "for free": our stderr is the journal.
- `Type=notify` lets us report the 60-second resume target (SC-003) honestly.

## R-12. Outbound destination allowlist

**Decision**: Enforce at the rustls layer by constructing a `RootCertStore`
that contains only the CA chain returned at enrollment, *and* by configuring
`reqwest` to refuse any host other than the configured `perchpub_url`'s
authority (a pre-flight check in our HTTP wrapper before each request).

Verified via an integration test (`outbound_allowlist.rs`) that wires the
process behind a userspace network namespace whose default route is a
counting proxy: the test asserts zero connection attempts to any host other
than the test perchpub during a 5-minute simulated workload.

**Rationale**: Spec SC-007 and US3 acceptance #3 are explicit "MUST emit no
other traffic". This needs to be a *tested invariant*, not a code comment.
The CA-pinning prevents talking to a different host even if DNS lies; the
URL check prevents accidentally adding a second outbound destination during
refactors.

(Note: DNS resolution and NTP run as system services; the test allows
loopback DNS resolver calls and NTP traffic from the systemd-timesyncd
process, but not from our PID.)

## R-13. Cross-compilation

**Decision**: `cargo-zigbuild` for aarch64-unknown-linux-gnu, invoked from
the dev host. CI builds both the host-triple binary (for tests) and the
aarch64 binary (as a release artifact).

**Rationale**:
- `cargo-zigbuild` produces musl-or-glibc binaries against an arbitrary
  glibc minimum without needing a sysroot setup on the dev host.
- Simpler than `cross-rs` (no Docker required); equally fast.
- Works identically on Linux and macOS dev hosts.

**Alternatives considered**: `cross` (Docker-based), official aarch64 sysroot
+ linker config (per-machine setup).

## Constitution recheck

After this round of decisions, the Constitution Check section in `plan.md`
remains green: no principle requires an exception, no entry needs to be
added to Complexity Tracking. The hardware boundary stays clean
(R-4/R-5), resource discipline holds (R-1 streaming, R-6 bounded writes,
R-7/R-8 bounded retries), observability without telemetry is a tested
invariant (R-12), and TDD applies to every decision above except the
release-only camera smoke test.
