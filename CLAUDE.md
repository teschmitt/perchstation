<!-- SPECKIT START -->
For additional context about technologies to be used, project structure,
shell commands, and other important information, read the current plan
at `specs/002-capture-subsystem/plan.md`.
<!-- SPECKIT END -->

## Workspace layout

Cargo workspace with three crates and one binary:

- `crates/perchstation-core` — platform-agnostic delivery, enrollment,
  queue, perchpub client, and observability.
- `crates/perchstation-hw` — the only place hardware lives (cfg-gated to
  Linux); production `Clock` and `QrFrameSource` implementations.
- `crates/perchstation` — the operator-facing binary with `clap`
  subcommands (`enroll`, `serve`, `status`) and the dev-only `fakepub`
  binary used by the quickstart.

## Common commands

```sh
# Lints + tests (run all three before sending a PR).
cargo fmt --check
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace

# Run the operator CLI against an ad-hoc config.
cargo run -p perchstation -- --config <path> enroll --qr-source file --qr-file <png>
cargo run -p perchstation -- --config <path> serve
cargo run -p perchstation -- --config <path> status [--json]

# Run the dev-only fake perchpub used by quickstart.md §2.
cargo run -p perchstation --bin fakepub -- --listen 127.0.0.1:8443 \
    --tls-cert <pem> --tls-key <pem> --ca <pem> --ca-key <pem>

# Cross-compile for Raspberry Pi (quickstart.md §6).
rustup target add aarch64-unknown-linux-gnu
cargo zigbuild --release -p perchstation --target aarch64-unknown-linux-gnu
```

## Deploy artefacts

- `deploy/systemd/perchstation.service` — systemd unit (Type=notify).
- `deploy/config.example.toml` — commented operator config template.
- `deploy/RELEASE-CHECKLIST.md` — on-device manual smoke test.
