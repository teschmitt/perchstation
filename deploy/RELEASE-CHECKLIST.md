# On-device release smoke test

`quickstart.md` proves the delivery subsystem on a developer's laptop
against a fake perchpub and a PNG-fed QR source. Three classes of
behaviour are out of reach for that quickstart and MUST be exercised on
real hardware against a real perchpub deployment before tagging a
release:

1. Real camera QR capture (`perchstation-hw::camera_qr`).
2. Real mTLS interop against a deployed perchpub.
3. Long-run resource behaviour under the constitution's wear/RSS budgets.

This document is the operator checklist for those steps.

## Prerequisites

- Raspberry Pi OS Bookworm (64-bit) on a Pi 4 or Pi Zero 2 W.
- A perchpub staging deployment reachable from the Pi.
- A perchpub enrollment session in hand: session ID, auth token, CA chain
  PEM, rendered as a QR PNG on a phone/tablet screen.
- The release candidate `perchstation` binary cross-built per
  `quickstart.md` §6 and copied to `/usr/local/bin/perchstation`.
- `deploy/config.example.toml` adapted for the staging perchpub URL and
  installed at `/etc/perchstation/config.toml`.
- `deploy/systemd/perchstation.service` installed at
  `/etc/systemd/system/perchstation.service` (not yet enabled).
- A `perchstation` system user/group provisioned.

## Step 1 — Camera QR enrollment

1. Hold the QR PNG on a phone screen ~20 cm from the Pi camera under
   normal indoor lighting.
2. As `perchstation` (or via `sudo -u perchstation`):

   ```sh
   perchstation --config /etc/perchstation/config.toml \
       enroll --qr-source camera
   ```

3. Expect: a single `enrollment.qr_decoded` event followed by
   `enrollment.csr_generated`, `enrollment.sent`, and
   `enrollment.persisted`. Total wall-clock from invocation to credentials
   on disk SHOULD be under 3 minutes (SC-004).
4. Verify on disk:

   ```sh
   ls -l /var/lib/perchstation/credentials/
   # identity.json  station.crt  station.key (mode 0600)  ca_chain.pem
   ```

5. **Reject** the build if:
   - Decoding takes more than 30 s under reasonable lighting.
   - `station.key` is world- or group-readable.
   - Any `enrollment.refused*`, `enrollment.failed`, or
     `enrollment.session_invalid` event appears.

## Step 2 — Real perchpub interop

1. Enable + start the service:

   ```sh
   sudo systemctl enable --now perchstation
   journalctl -u perchstation -f
   ```

2. Confirm `service.ready` fires followed by `sd_notify(READY=1)` (visible
   via `systemctl status perchstation` reaching `active (running)` rather
   than `activating`).
3. Drop a known-good `sample.mp4` into `/var/lib/perchstation/queue/pending/`
   with a matching sidecar (mirror `quickstart.md` §3).
4. Expect the sequence:
   - `delivery.attempt_started attempt=1`
   - `delivery.upload_succeeded` with a real `classify_task_id`
   - `classify.polled` non-terminal then `classify.terminal status=Success`
     (or `Failed` — the classify outcome is perchpub-side and either is
     acceptable here; what matters is reaching a terminal status).
5. Verify `perchstation status` reports `Last success: <timestamp>` within
   one minute of the upload completing.
6. **Reject** the build if:
   - Any `delivery.upload_terminal` or `delivery.attempts_exhausted`
     appears for a clip the operator knows is good.
   - The on-the-wire perchpub URL is anything other than the configured
     authority (spot-check with `ss -tnp` on the Pi).

## Step 3 — Network allowlist (manual)

While the service is running, observe outbound connections:

```sh
# In a second SSH session, with `ss` from iproute2 installed.
sudo ss -tnp | awk '$NF ~ /perchstation/ {print}'
```

Only connections to the configured perchpub authority's IP+port should
appear. DNS resolution traffic from `systemd-resolved` is expected; NTP
traffic from `systemd-timesyncd` is expected. Any other destination
attributed to the `perchstation` PID is a failure of SC-007 and must be
investigated before release.

## Step 4 — Journal hygiene

After at least one upload cycle:

```sh
sudo journalctl -u perchstation --output=json-pretty | head -100
```

- Every line MUST be valid JSON.
- No line MUST contain a PEM `BEGIN`/`END` marker for a private key, nor
  any base64 fragment of one.
- `auth_token` from the enrollment QR MUST NOT appear in any line.

If any of these violations appear, reject the build — the redaction layer
(`crates/perchstation-core/src/observability/tracing.rs::redact`) has
regressed and needs to be patched before shipping.

## Step 5 — Resume after power-cycle

1. While `serve` is uploading a clip, pull the Pi's power.
2. Re-apply power. Wait for the boot to complete.
3. Confirm `journalctl -b -u perchstation` shows:
   - `queue.recovered_inflight` for any clip that was mid-upload at the
     crash, and
   - `service.ready` within 60 s of power-on (SC-003 — measure with
     `systemd-analyze` or by hand against `journalctl -b`'s timestamps).
4. **Reject** the build if `service.ready` takes longer than 60 s on a Pi
   Zero 2 W with a populated queue (≥ 50 clips), or if an
   `inflight/*.mp4` orphan remains after the first delivery cycle.

## Step 6 — 7-day soak (SC-005)

1. Configure the Pi against the staging perchpub for at least 7 calendar
   days with a synthetic capture cadence that mimics expected production
   load (a handful of clips per minute at peak, dozens per day typical).
2. At day 7:
   - `perchstation status` reports `enrollment.state = ok`.
   - `journalctl --since "7 days ago"` contains zero `service.shutdown`
     events with `reason != "sigterm"` (i.e., no panics, no OOMs).
   - `systemctl show perchstation --property=MainPID` followed by
     `ps -o rss= -p <pid>` reports RSS under 50 MB on a Pi Zero 2 W
     (Principle III).
   - `du -sh /var/lib/perchstation/queue` reports a value bounded by the
     configured `queue.max_bytes`.
   - `journalctl --disk-usage` does not exceed the configured
     `SystemMaxUse` (default 200 MB for a Pi, see quickstart §7).
3. **Reject** the build if any of the above fails.

## Step 7 — Tear-down

1. `sudo systemctl disable --now perchstation`
2. Optional: `sudo rm -rf /var/lib/perchstation/` to clean the staging
   identity before re-running this checklist against a different release.

## Recording the result

For each release tag, record in the release notes:

- The git SHA tested.
- The Pi model and OS version used.
- The perchpub staging deployment commit SHA.
- The dates the 7-day soak ran.
- Any deviation from the checklist above, with a link to the follow-up
  issue tracking the deviation.

If any step is skipped, the release MUST NOT be promoted from staging to
production firmware.
