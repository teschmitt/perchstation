# Quickstart: Clip Delivery Subsystem (dev host)

**Audience**: a developer with a clean clone of `perchstation` and no Pi
attached. The constitution requires that "the bulk of the codebase MUST be
testable without a Pi"; this quickstart is how you verify that on day one.

End-to-end goal: with a fake perchpub running locally, enrol a synthetic
station from a QR PNG, hand a `.mp4` to delivery, and watch it complete
end-to-end — all on your laptop.

## Prerequisites

- Rust stable, MSRV ≥ 1.75 (check with `rustup show`).
- A POSIX-ish dev host (Linux or macOS).
- `cargo-zigbuild` (only needed for the optional aarch64 cross-build at the
  end). Install with `cargo install cargo-zigbuild`.

No Pi, no camera, no real perchpub.

## 1. Build and test

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

`cargo test --workspace` runs:

- Unit tests inside each crate.
- The integration tests in `tests/integration/` against a fake perchpub
  (axum) and an in-memory `QrFrameSource` populated from PNG fixtures
  under `tests/fixtures/`.
- The contract-drift test that diff's `references/openapi.json` against
  the schemas the station mirrors.

Expected: green. If anything is red, **stop** and fix before moving on.

## 2. Smoke-run the binary against a fake perchpub

In one terminal, start the fake perchpub bundled in the dev tools:

```sh
cargo run -p perchstation --bin fakepub -- --listen 127.0.0.1:8443 \
    --tls-key tests/fixtures/fakepub.key \
    --tls-cert tests/fixtures/fakepub.crt \
    --ca tests/fixtures/fakepub-ca.pem
```

In another terminal, pick a temporary data dir and run enrollment off a
fixture QR PNG (this is the `--qr-source=file` recovery path, since you
have no camera):

```sh
export PERCHSTATION_DATA="$(mktemp -d)"
cat > "$PERCHSTATION_DATA/config.toml" <<'EOF'
perchpub_url = "https://localhost:8443"
data_dir     = "/REPLACE/ME"
EOF
sed -i "s|/REPLACE/ME|$PERCHSTATION_DATA|" "$PERCHSTATION_DATA/config.toml"

cargo run -p perchstation -- \
    --config "$PERCHSTATION_DATA/config.toml" \
    enroll --qr-source file --qr-file tests/fixtures/enroll-session.png
```

You should see (text format default in TTY mode):

```text
Enrolled: station 7f3e... — credentials written to $PERCHSTATION_DATA/credentials/
```

Verify on disk:

```sh
ls "$PERCHSTATION_DATA/credentials/"
# identity.json  station.crt  station.key  ca_chain.pem
stat -c '%a %n' "$PERCHSTATION_DATA/credentials/station.key"
# 600 .../station.key
```

## 3. Hand the delivery loop a clip

Drop a clip into the queue's `pending/` directory the same way the capture
subsystem will:

```sh
mkdir -p "$PERCHSTATION_DATA/queue/pending"
cp tests/fixtures/sample.mp4 "$PERCHSTATION_DATA/queue/pending/20260527T100000Z-001.mp4"
cat > "$PERCHSTATION_DATA/queue/pending/20260527T100000Z-001.json" <<'EOF'
{
  "clip_id": "20260527T100000Z-001",
  "captured_at": "2026-05-27T10:00:00Z",
  "enqueued_at": "2026-05-27T10:00:00Z",
  "byte_size": 0,
  "attempts": 0
}
EOF
```

(The sidecar writer in production fills `byte_size`; the delivery loop
verifies it. For the manual smoke, leaving it `0` exercises the validation
path — expect the delivery loop to overwrite it on first inspection. To
exercise the happy path, set it to the actual size.)

Run the delivery loop in the foreground:

```sh
cargo run -p perchstation -- \
    --config "$PERCHSTATION_DATA/config.toml" \
    --log-format text \
    serve
```

You should observe (within seconds, against the fake perchpub):

```text
INFO  service.ready              pending_at_start=1
INFO  delivery.attempt_started   clip_id=20260527T100000Z-001 attempt=1
INFO  delivery.upload_succeeded  clip_id=… classify_task_id=… attempt=1 duration_ms=…
INFO  classify.polled            clip_id=… status=Queued
INFO  classify.terminal          clip_id=… status=Success observation_id=…
```

The clip's `.mp4` is gone from `pending/`/`inflight/`; the sidecar is in
`delivered/` with `outcome: "Delivered"` and a `classify_task_id`.

## 4. Inspect with `status`

Leave `serve` running. In a third terminal:

```sh
cargo run -p perchstation -- --config "$PERCHSTATION_DATA/config.toml" status
```

Expected:

```text
Enrollment:    OK (station 7f3e..., cert expires …)
Queue depth:   0 clips (0 B on disk)
Last success:  … UTC  (… ago)
Last failure:  (none)
Last 3 deliveries:
  …  20260527T100000Z-001  classify=Success
```

JSON form:

```sh
cargo run -p perchstation -- --config "$PERCHSTATION_DATA/config.toml" status --json | jq .
```

## 5. Re-enrollment is refused

```sh
cargo run -p perchstation -- \
    --config "$PERCHSTATION_DATA/config.toml" \
    enroll --qr-source file --qr-file tests/fixtures/enroll-session.png
# exit code 76, message names the existing station_id and cert expiry.
```

`--force` is required to overwrite; the event log records
`enrollment.refused_overwrite` either way.

## 6. (Optional) Cross-build for a Pi

```sh
rustup target add aarch64-unknown-linux-gnu
cargo zigbuild --release -p perchstation --target aarch64-unknown-linux-gnu
file target/aarch64-unknown-linux-gnu/release/perchstation
```

The resulting binary runs on Pi OS Bookworm. On-device validation
(camera-based enrollment, real perchpub, journald logs) is the release
smoke test, not part of this quickstart.

## 7. systemd unit (production deployment)

Reference unit lives at `deploy/systemd/perchstation.service`. Install it
with:

```sh
sudo install -m 0644 deploy/systemd/perchstation.service /etc/systemd/system/
sudo install -d -o root -g root -m 0755 /etc/perchstation
sudo install -m 0644 deploy/config.example.toml /etc/perchstation/config.toml
# After editing config, and after a one-time `perchstation enroll`:
sudo systemctl enable --now perchstation
journalctl -u perchstation -f
```

Journald rotation is configured globally; for a Pi we recommend a
`SystemMaxUse=200M` in `/etc/systemd/journald.conf.d/perchstation.conf` to
keep the journal off the SD card's wear path (constitution Principle III).

## What this quickstart does *not* prove

- Real camera QR capture. Covered by the release-only manual smoke test
  documented in `deploy/RELEASE-CHECKLIST.md`.
- Real perchpub interop. Covered by the same release smoke test against a
  staging perchpub deployment.
- Long-run resource usage on real hardware. Covered by the 7-day soak
  documented under SC-005.
