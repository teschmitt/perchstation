# Deploying perchstation to a Pi (dev loop)

This walkthrough takes a freshly cross-built binary, installs it on a
Raspberry Pi, and points it at a perchpub deployment on your home
network. It is the middle ground between the two existing docs:

- [`specs/001-clip-delivery/quickstart.md`](../specs/001-clip-delivery/quickstart.md)
  and [`specs/002-capture-subsystem/quickstart.md`](../specs/002-capture-subsystem/quickstart.md)
  exercise the codebase on a developer laptop with no Pi, no camera,
  and a fake perchpub — fast iteration, narrow scope.
- [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) is the on-device
  release gate: full hardware coverage, sensor-failure injection, a
  7-day soak. Required before tagging, far too heavyweight for a
  dev loop.

This document is what you actually do day-to-day: get the bits onto a
Pi, talk to your own perchpub, watch the journal, iterate.

The running example assumes your dev perchpub is reachable at the
hostname `perchpub`. Substitute your own hostname (or IP) throughout if
yours differs.

## 0. DNS sanity (do this first)

From the Pi:

```sh
getent hosts perchpub
curl -kv https://perchpub/api/v1/health   # or whatever port perchpub listens on
```

If `getent` returns nothing, the Pi can't resolve `perchpub`. Either
set up mDNS on the perchpub host (`avahi-daemon` advertising
`perchpub.local` — and use `perchpub.local` in the config below), or
drop a static line in `/etc/hosts` on the Pi:

```sh
echo "192.168.x.y  perchpub" | sudo tee -a /etc/hosts
```

**TLS gotcha.** perchpub's dev server certificate must include the
hostname you configure on the station (exact SAN match), and the CA
chain perchpub bakes into the enrollment QR must be the same chain
that signs that server cert. The station validates perchpub's server
certificate against the `ca_chain.pem` it received at enrollment time
— a mismatch is the most likely thing to bite you on the first run.

## 1. Cross-build on the dev host

```sh
# One-time setup if you haven't done it yet.
rustup target add aarch64-unknown-linux-gnu
cargo install cargo-zigbuild

cargo zigbuild --release -p perchstation --target aarch64-unknown-linux-gnu
file target/aarch64-unknown-linux-gnu/release/perchstation
# ELF 64-bit LSB pie executable, ARM aarch64, …
```

## 2. Copy artefacts to the Pi

```sh
PI=pi@<pi-host>
scp target/aarch64-unknown-linux-gnu/release/perchstation "$PI:/tmp/"
scp deploy/config.example.toml                            "$PI:/tmp/"
scp deploy/systemd/perchstation.service                   "$PI:/tmp/"
```

Then on the Pi:

```sh
sudo install -m 0755 /tmp/perchstation                /usr/local/bin/perchstation
sudo install -d -o root -g root -m 0755               /etc/perchstation
sudo install -m 0644 /tmp/config.example.toml         /etc/perchstation/config.toml
sudo install -m 0644 /tmp/perchstation.service        /etc/systemd/system/

# System user the unit runs as (StateDirectory= will create /var/lib/perchstation).
sudo useradd --system --no-create-home --shell /usr/sbin/nologin perchstation
```

Make sure `libcamera-apps` (or `rpicam-apps`) is installed and the
camera is recognised:

```sh
libcamera-hello --list-cameras
```

## 3. Point the config at your dev perchpub

Edit `/etc/perchstation/config.toml`. Minimum change:

```toml
perchpub_url = "https://perchpub"          # or "https://perchpub:8443" if non-default port
data_dir     = "/var/lib/perchstation"     # leave as-is; systemd StateDirectory handles it
```

Everything else in the example config is a sensible default. Worth a
glance for your dev setup:

- `[capture] sensor_gpiochip`, `sensor_line`, `sensor_active_high` —
  match the GPIO line you actually wired the motion sensor to.
- `[capture] camera_width`/`height`/`framerate`/`bitrate_bps` — fine
  to leave; drop them only if your camera module struggles at 720p30.

## 4. Enrol against perchpub once

In your perchpub UI, initiate a new station enrollment session and
render the QR on a phone screen. Then on the Pi:

```sh
sudo -u perchstation /usr/local/bin/perchstation \
    --config /etc/perchstation/config.toml \
    --log-format text \
    enroll --qr-source camera
```

Hold the phone ~20 cm from the camera under normal indoor light.
Expect the event sequence `enrollment.qr_decoded` →
`enrollment.csr_generated` → `enrollment.sent` →
`enrollment.persisted`, ending with credentials on disk:

```sh
sudo ls -l /var/lib/perchstation/credentials/
# identity.json  station.crt  station.key (mode 0600)  ca_chain.pem
```

If you'd rather skip the camera path while iterating (perfectly fine
for a dev loop), `scp` the QR PNG to the Pi and run
`enroll --qr-source file --qr-file /tmp/enroll.png` instead.

## 5. Start the service

```sh
sudo systemctl enable --now perchstation
journalctl -u perchstation -f --output=cat
```

Within a few seconds you should see `service.ready`, then idle. Wave
your hand in front of the motion sensor — within ~1 second you should
see:

```text
capture.trigger_observed
capture.recording_started recording_id=…
capture.recording_completed bytes=…
delivery.attempt_started clip_id=…
delivery.upload_succeeded clip_id=… classify_task_id=…
classify.terminal status=Success observation_id=…
```

And from a second SSH session:

```sh
perchstation --config /etc/perchstation/config.toml status
```

Should show enrollment OK, last recording timestamp, last success
timestamp, and `Sensor: healthy`.

## 6. When something goes wrong

A few common dev-loop failures, in order of likelihood:

| Symptom | Likely cause |
| --- | --- |
| `delivery.upload_terminal` with TLS error | Pi doesn't trust perchpub's server cert; the `ca_chain.pem` in `credentials/` doesn't match the chain perchpub presents. Verify with `openssl s_client -showcerts -connect perchpub:443 </dev/null` on the Pi (match the host:port in `perchpub_url`) and compare against `ca_chain.pem`. |
| `enrollment.session_invalid` | QR session expired (perchpub-side TTL) or already consumed — regenerate. |
| Re-enrollment refused (exit 76) | Expected — pass `--force` if you genuinely want to overwrite, or `sudo rm -rf /var/lib/perchstation/credentials/` for a clean dev reset. |
| `capture.sensor_degraded kind=unavailable` | GPIO line wiring or `sensor_gpiochip`/`sensor_line` doesn't match the physical setup. |
| No `capture.trigger_observed` on wave | `sensor_active_high` polarity is inverted, or the sensor is wired to a different BCM line than the config says. |
| Camera errors on first record | `libcamera-vid` not on `PATH` for the `perchstation` user, or the CSI ribbon isn't seated. |

For a clean dev reset between runs:

```sh
sudo systemctl stop perchstation
sudo rm -rf /var/lib/perchstation/credentials \
            /var/lib/perchstation/queue \
            /var/lib/perchstation/capture-staging
sudo systemctl start perchstation
```

## 7. When you are ready to tag a release

This document is **not** the release gate. Before tagging, run the
full procedure in [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md):
sensor-disconnect, stuck-asserted, journal hygiene, 7-day soak, and
the rest. A dev-loop deployment is not a substitute.
