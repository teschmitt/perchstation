# perchstation

A Rust client that runs on a Raspberry Pi inside a bird feeder. It watches
a motion sensor, records short video clips of visitors, and uploads each
clip over mTLS to a [perchpub](https://codeberg.org/perchpub/perchpub)
backend where the owner can browse them in a friendly UI.

The device is designed to live outdoors for months at a time, unattended,
behind home Wi-Fi, owned by people who are not engineers. Everything in
this codebase exists to honour that: bounded queues, bounded retries,
camera off when idle, structured logs to the journal, no telemetry, no
phone-home.

## Status

**Early — v0.1.0 / pre-release.** Two subsystems have landed:

- `001-clip-delivery` — enrollment, bounded on-disk queue, mTLS upload,
  classify-task polling, operator status surface.
- `002-capture-subsystem` — motion-triggered recording, sensor liveness
  tracking, staging-purge on boot, hand-off to delivery via a single
  `Inbox::submit` call.

Deferred to follow-up iterations: enrollment certificate renewal,
pre-roll / post-roll, weight sensor, multi-deployment failover, and a
companion UI. The on-device release smoke test in
[`deploy/RELEASE-CHECKLIST.md`](deploy/RELEASE-CHECKLIST.md) is the
release gate; no build ships without it.

## How it fits together

`perchstation` is a single binary running as a systemd service on the Pi.
Inside `perchstation serve` two cooperating tasks share one Tokio runtime:

```text
  motion sensor ─┐
                 ▼
            ┌─────────┐   Inbox::submit   ┌──────────┐   mTLS POST   ┌──────────┐
   camera ──┤ capture ├──────────────────▶│ delivery ├──────────────▶│ perchpub │
            └─────────┘                   └──────────┘               └──────────┘
                                                │
                                                ▼
                                     /var/lib/perchstation/queue/
                                        pending/ inflight/ delivered/
```

The only data-plane contact between the two halves is the `Inbox` trait
(`crates/perchstation-core/src/queue/inbox.rs`). The capture loop never
touches queue directories directly; the delivery loop never knows where
clips come from. Either half can panic and the other keeps running.

The workspace is a deliberate split along the hardware boundary:

| Crate | Role |
| --- | --- |
| `crates/perchstation-core` | Platform-agnostic: delivery state machine, capture state machine, queue, perchpub client, enrollment, observability. Compiles on any host. |
| `crates/perchstation-hw` | The only place hardware lives (Linux-gated): `GpioMotionSensor`, `LibcameraVidCamera`, camera QR source, monotonic `Clock`. |
| `crates/perchstation` | The operator-facing binary (`enroll` / `serve` / `status`) and the dev-only `fakepub` binary. |

This split is a constitutional rule, not a stylistic one — see
[`.specify/memory/constitution.md`](.specify/memory/constitution.md)
Principle II ("Hardware at the Boundary"). The practical consequence is
that almost the entire codebase is testable from a developer's laptop
with no Pi, no camera, and no GPIO line.

## Operator quick start (on a Pi)

You will need:

- A Pi 4 or Pi Zero 2 W running Pi OS Bookworm (64-bit), with the Pi
  Camera module on the CSI ribbon and a motion sensor wired to a GPIO
  line. `libcamera-apps` (or `rpicam-apps`) must be installed.
- A reachable perchpub deployment.
- A perchpub-issued enrollment QR rendered on a phone screen.

1. Cross-build the binary on your dev host
   (see [Development](#development) for `cargo-zigbuild` setup) and
   copy it to the Pi:

   ```sh
   cargo zigbuild --release -p perchstation \
       --target aarch64-unknown-linux-gnu
   scp target/aarch64-unknown-linux-gnu/release/perchstation \
       pi@<host>:/usr/local/bin/
   ```

2. Install the config template and the systemd unit:

   ```sh
   sudo install -d -o root -g root -m 0755 /etc/perchstation
   sudo install -m 0644 deploy/config.example.toml /etc/perchstation/config.toml
   sudo install -m 0644 deploy/systemd/perchstation.service /etc/systemd/system/
   ```

   Edit `/etc/perchstation/config.toml` — at minimum set `perchpub_url`.
   Every other field has a documented default; see the comments in
   [`deploy/config.example.toml`](deploy/config.example.toml) before
   tuning queue, retry, or capture bounds.

3. Provision a `perchstation` user and enrol once (camera-based, holds
   the QR ~20 cm from the lens under normal indoor lighting):

   ```sh
   sudo -u perchstation perchstation \
       --config /etc/perchstation/config.toml \
       enroll --qr-source camera
   ```

   The keypair is generated on-device and never leaves it. Re-enrollment
   is refused unless `--force` is passed; the event is always logged.

4. Enable the service and watch the journal:

   ```sh
   sudo systemctl enable --now perchstation
   journalctl -u perchstation -f
   ```

This is the condensed path. For a fuller day-to-day dev-loop walkthrough
— DNS/TLS sanity, cross-build, a troubleshooting table, and a clean-reset
recipe — see [`deploy/DEPLOYMENT.md`](deploy/DEPLOYMENT.md). For the full
release-gate procedure (including the sensor-disconnect / stuck-sensor /
7-day soak checks), follow
[`deploy/RELEASE-CHECKLIST.md`](deploy/RELEASE-CHECKLIST.md).

## Operator commands

```sh
perchstation [--config <path>] [--log-format json|text] [--log-level <filter>]
             <enroll|serve|status>
```

| Command | What it does |
| --- | --- |
| `enroll` | One-shot, interactive. Decodes a perchpub enrollment QR (camera by default, `--qr-source file` accepted as a recovery path), generates a keypair, persists the issued cert and CA chain. |
| `serve` | Long-running. Runs the delivery loop, the classify-task poller, and the capture loop under a single Tokio runtime. Speaks `sd_notify` so systemd reaches `active (running)` only after readiness. |
| `status` | Snapshot. Prints enrollment health, queue depth, last success / last failure, recent deliveries, and the capture-side block (last recording, last failure, sensor liveness). `--json` for tooling. |

Exit codes follow the contract in
[`specs/001-clip-delivery/contracts/cli.md`](specs/001-clip-delivery/contracts/cli.md):
`0` ok, `64` usage, `70` config, `74` I/O, `75` transient, `76`
unrecoverable.

## Configuration

The full annotated configuration template lives in
[`deploy/config.example.toml`](deploy/config.example.toml). The
highlights:

- `perchpub_url` — the mTLS endpoint; fixed at enrollment time.
- `data_dir` — root for `credentials/`, `queue/`, and `capture-staging/`.
  Defaults to `/var/lib/perchstation`, populated by systemd via
  `StateDirectory=perchstation`.
- `[queue]` — `max_clips`, `max_bytes`, and an explicit `eviction`
  policy (`drop_oldest_undelivered` or `refuse_new`).
- `[retry]` — exponential backoff with jitter, capped per-attempt delay,
  per-clip attempt ceiling and wall-clock budget.
- `[capture]` — clip duration, cooldown, sensor liveness threshold,
  capture-side disk ceiling, and the GPIO + camera parameters passed to
  the hardware adapters.

All fields have built-in defaults; override only what you need.

## Development

Tested on Linux and macOS dev hosts. Rust stable, MSRV **1.95**, edition
2024. Cross-compiling to the Pi additionally needs
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)
(`cargo install cargo-zigbuild`).

```sh
# Lints + tests — run all three before sending a PR.
cargo fmt --check
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
```

`cargo test --workspace` runs the host-runnable integration suite under
[`tests/integration/`](tests/integration/) end-to-end: a fake perchpub
(axum), an in-memory `QrFrameSource` fed from PNG fixtures minted in
process, a `FakeMotionSensor`, and a `FakeCamera`. Every functional
requirement in the two feature specs maps to at least one test file —
the file-level docstring on each test names the spec ID it covers.

For host-side smoke testing without a Pi, see the two quickstarts:

- [`specs/001-clip-delivery/quickstart.md`](specs/001-clip-delivery/quickstart.md)
  — enrol a synthetic station against a local fake perchpub, hand it a
  clip, watch it upload.
- [`specs/002-capture-subsystem/quickstart.md`](specs/002-capture-subsystem/quickstart.md)
  — drive a synthetic motion edge into the capture loop and watch a
  clip appear in the delivery queue.

For the dev-only `fakepub` perchpub stand-in:

```sh
cargo run -p perchstation --bin fakepub -- --listen 127.0.0.1:8443 \
    --tls-cert <pem> --tls-key <pem> --ca <pem> --ca-key <pem>
```

Cross-build for the Pi:

```sh
rustup target add aarch64-unknown-linux-gnu
cargo zigbuild --release -p perchstation --target aarch64-unknown-linux-gnu
```

### Design documents

Every non-trivial feature is spec-driven. Each `specs/<feature>/`
directory contains `spec.md` (what), `plan.md` (how), `research.md`
(decisions and alternatives), `data-model.md`, `contracts/`, and a
`quickstart.md`. The project constitution that governs all of this is
[`.specify/memory/constitution.md`](.specify/memory/constitution.md);
post-mortems for shipped features live in [`docs/`](docs/).

## Privacy & networking posture

The station emits **no telemetry**. The only outbound traffic it makes
is to the configured perchpub authority, plus the system-configured time
and name-resolution services it depends on. This is enforced as a
tested invariant (`tests/integration/outbound_allowlist.rs`) and
spot-checked by hand on real hardware via Step 4 of the release
checklist.

Private keys are generated on-device during enrollment and never leave
it. The systemd unit hardens the process with `ProtectSystem=strict`,
`PrivateDevices=true`, a narrow `SystemCallFilter=@system-service`, and
`MemoryDenyWriteExecute=true`; see
[`deploy/systemd/perchstation.service`](deploy/systemd/perchstation.service).

## License

AGPL-3.0-or-later. See individual crate `Cargo.toml` files for the
authoritative license declaration.

## Links

- Source: <https://github.com/teschmitt/perchstation>
- Companion backend: <https://codeberg.org/perchpub/perchpub>
- Specs and design docs: [`specs/`](specs/)
- Operator deploy artefacts: [`deploy/`](deploy/)
- Contributor guidance for AI agents: [`CLAUDE.md`](CLAUDE.md)
