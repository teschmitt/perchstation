# On-device release smoke test

`quickstart.md` proves the delivery subsystem on a developer's laptop
against a fake perchpub and a PNG-fed QR source. Four classes of
behaviour are out of reach for that quickstart and MUST be exercised on
real hardware against a real perchpub deployment before tagging a
release:

1. Real camera QR capture (`perchstation-hw::camera_qr`).
2. Real mTLS interop against a deployed perchpub.
3. Real motion-sensor edge → `libcamera-vid` recording on a wired Pi
   (`perchstation-hw::motion_sensor` + `camera_recorder`).
4. Long-run resource behaviour under the constitution's wear/RSS budgets.

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
- A motion sensor (PIR module or equivalent) wired to the GPIO line and
  rail named in the installed config's `[capture]` section (defaults:
  `/dev/gpiochip0`, BCM line 17, active-high).
- A Pi Camera module connected via the CSI ribbon and recognised by
  `libcamera-hello --list-cameras` (the OS package `libcamera-apps` /
  `rpicam-apps` MUST be installed).

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

## Step 3 — Capture-side smoke

The capture loop's host-runnable tests exercise it against
`FakeMotionSensor` + `FakeCamera`. This step proves the production
`GpioMotionSensor` + `LibcameraVidCamera` adapters work end-to-end on
real Pi hardware, plus the two real-time liveness paths
(`stuck_asserted`, `unavailable`) that a synthetic test cannot
faithfully reproduce.

The service from Step 2 should still be running for the whole step.
Have a second SSH session open on the Pi with `journalctl -u
perchstation -f --output=json | jq` to watch `capture.*` events.

### 3.1 Real GPIO edge → playable MP4 in `pending/`

1. Empty the queue so a fresh clip is easy to spot:

   ```sh
   sudo find /var/lib/perchstation/queue -maxdepth 2 -type f -delete
   ```

2. Wave a hand in front of the motion sensor once.
3. Expect the following sequence in the journal within ~1 s of the
   edge (SC-001):
   - `capture.trigger_observed`
   - `capture.recording_started recording_id=<uuid>`
   - `capture.recording_completed bytes=<n>`
   - then `delivery.attempt_started` for the freshly-submitted clip.
4. Verify on disk:

   ```sh
   ls -l /var/lib/perchstation/queue/pending/
   # one *.mp4 + matching sidecar, both owned by `perchstation`.
   ls -l /var/lib/perchstation/capture-staging/
   # empty — the recording moved into pending/ via Inbox::submit.
   ```

5. Copy the clip off the Pi and confirm it is a valid, playable MP4
   whose duration is close to `capture.clip_duration_secs`:

   ```sh
   scp pi@<host>:/var/lib/perchstation/queue/pending/*.mp4 ./
   ffprobe -hide_banner -v error -show_entries format=duration *.mp4
   # duration ≈ clip_duration_secs (default 8.0)
   ```

6. **Reject** the build if:
   - No `capture.trigger_observed` event appears within 5 s of the
     hand-wave.
   - No `.mp4` lands in `pending/` (the clip is missing or stuck in
     `capture-staging/`).
   - `ffprobe` reports a corrupt container or duration significantly
     outside `clip_duration_secs ± hang_margin_secs`.
   - The `pending/` clip is owned by anything other than the
     `perchstation` user/group.

### 3.2 Status surface reflects the recording

Within 30 s of step 3.1 (SC-007):

```sh
perchstation --config /etc/perchstation/config.toml status
perchstation --config /etc/perchstation/config.toml status --json | jq .capture
```

- Text output's `Capture:` block reports `Last recording: <recent
  timestamp>` and `Sensor: healthy`, with `Last failure: (none)`.
- JSON output's `capture.last_recording_at` is the trigger time,
  `capture.last_clip_id` matches the `recording_id` in the journal,
  `capture.last_failure` is `null`, and `capture.sensor_liveness` is
  `"healthy"`.

**Reject** the build if either rendering still shows the pre-test
state (`(none)` for `Last recording`, or `sensor_liveness =
"never_observed"`) more than 30 s after the journal confirmed
`capture.recording_completed`.

### 3.3 Sensor disconnected → `unavailable`

1. Physically disconnect the motion sensor's signal wire (or unplug the
   whole module) while leaving the Pi powered.
2. Wait at most 60 s (the SC-005 budget; in practice
   ~`capture.liveness_poll_secs`).
3. Verify:
   - `journalctl` shows exactly one
     `capture.sensor_degraded kind="unavailable"` event.
   - `perchstation status` reports `Sensor: unavailable (since <ts>)`
     and the JSON `capture.sensor_liveness` is `"unavailable"` with a
     populated `capture.sensor_degraded_since`.
4. Trigger a hand-wave near the (now-disconnected) sensor. No clip
   should appear in `pending/`; if any edge does land, the journal
   should show `capture.degraded_skip` and the staging directory
   should remain empty.
5. Reconnect the sensor.
6. Within ~`capture.liveness_poll_secs` verify:
   - `journalctl` shows `capture.sensor_recovered kind="unavailable"`.
   - `perchstation status` reports `Sensor: healthy`.
   - A subsequent hand-wave produces a clip in `pending/` as in 3.1.
7. **Reject** the build if recovery does not happen within 60 s of
   reconnection, or if any `delivery.*` errors appear that the
   operator did not induce.

### 3.4 Sensor held asserted → `stuck_asserted`

1. Force the sensor's output to the asserted level continuously — the
   exact technique depends on the sensor (jumper the signal to the
   active rail; tape over a PIR's lens; etc.). Take care not to
   short-circuit the rail.
2. Wait at least `capture.liveness_stuck_secs` (default 300 s; SC-004).
3. Verify:
   - `journalctl` shows exactly one
     `capture.sensor_degraded kind="stuck_asserted"` event after the
     threshold.
   - `perchstation status` reports
     `Sensor: stuck_asserted (since <ts>)`; JSON
     `capture.sensor_liveness` is `"stuck_asserted"`.
   - Any natural edge during the held-assertion window produces
     `capture.degraded_skip` and no clip.
4. Release the assertion (let the sensor return to quiescent).
5. Within ~`capture.liveness_poll_secs` verify
   `capture.sensor_recovered kind="stuck_asserted"` and
   `Sensor: healthy`.
6. **Reject** the build if the `stuck_asserted` transition is missed,
   if any clip lands in `pending/` while the sensor is flagged
   degraded, or if recovery does not happen within 60 s of release.

### 3.5 Bounded capture-staging footprint (SC-006 spot-check)

A real 7-day soak for SC-006 is impractical for every release; this
30-minute spot-check confirms the structural property (the pre-record
disk-pressure gate keeps the staging directory bounded).

1. Drive the sensor with a synthetic trigger every ~10 s for 30 min —
   a hand-held actuator, a 555-timer breadboard, or repeatedly waving
   in front of a PIR all work. The cooldown gate will only let through
   one trigger per `clip_duration_secs + cooldown_secs` (default
   38 s), but the test deliberately fires faster to exercise the gate.
2. While the loop runs, in a second SSH session sample the staging
   directory every 60 s and log the maximum observed value:

   ```sh
   while sleep 60; do
     du -sb /var/lib/perchstation/capture-staging/ \
       | awk '{print strftime("%H:%M:%S"), $1}'
   done | tee /tmp/staging-size.log
   ```

3. After 30 min:
   - The maximum value in `staging-size.log` MUST be at or below
     `capture.max_staging_bytes` (default `268_435_456` = 256 MiB).
     In steady state it should be near zero, because completed
     recordings are handed straight to the queue and the staging file
     is removed.
   - `du -sb /var/lib/perchstation/queue/` MUST remain bounded by the
     configured `queue.max_bytes` (the queue's own eviction policy
     handles this, but a violation here points to a leak between the
     two halves).
   - The journal MUST NOT contain any `capture.disk_pressure_skip`
     events unless the operator deliberately pre-filled the staging
     directory to provoke one.
4. **Reject** the build if the staging directory ever exceeds
   `capture.max_staging_bytes`, if `pending/` grows past `queue.max_bytes`,
   or if the capture loop stops accepting fresh triggers before the
   30-minute window completes.

## Step 4 — Network allowlist (manual)

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

## Step 5 — Journal hygiene

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

## Step 6 — Resume after power-cycle

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

## Step 7 — 7-day soak (SC-005)

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

## Step 8 — Tear-down

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
