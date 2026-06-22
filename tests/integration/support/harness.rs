//! Cross-test helpers that wrap subprocess invocation of the `perchstation`
//! binary and the boilerplate of writing a runnable `config.toml` into a
//! per-test temp directory.
//!
//! Kept thin on purpose — the tests stay readable when the harness only
//! covers the bits every test repeats verbatim.

use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin;

/// Write a minimal `<data_dir>/config.toml` that points the station at
/// the supplied perchpub URL and at `data_dir` itself. Returns the path
/// to pass to `--config`.
///
/// Schema mirrors `crates/perchstation-core/src/config.rs::Config` —
/// `perchpub_url` is required at runtime by `ensure_runtime_ready`, and
/// `data_dir` overrides the production default of `/var/lib/perchstation`.
///
/// The single-origin `FakePerchpub` serves enrollment *and* uploads on one
/// ephemeral port, so `upload_url` is pinned to the same URL — otherwise the
/// upload base would derive the production `:8443` entrypoint (PRV-2/UPL-1)
/// and miss the fake's port.
pub fn write_config_toml(data_dir: &Path, perchpub_url: &str) -> PathBuf {
    let path = data_dir.join("config.toml");
    let body = format!(
        "perchpub_url = \"{perchpub_url}\"\nupload_url = \"{perchpub_url}\"\ndata_dir = \"{}\"\n",
        data_dir.display()
    );
    std::fs::write(&path, body).expect("write config.toml");
    path
}

/// `assert_cmd::Command` pre-configured for one-shot invocations
/// (`enroll`, `status`). The binary path comes from cargo's metadata so
/// the test always runs the build matching `cargo test`.
#[must_use]
pub fn perchstation_bin() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("perchstation").expect("locate perchstation binary")
}

/// Path to the freshly-built `perchstation` binary — for tests like
/// `delivery_happy` that drive `serve` via `tokio::process::Command`
/// (so they can SIGKILL it once the on-disk state settles).
#[must_use]
pub fn perchstation_bin_path() -> PathBuf {
    cargo_bin("perchstation")
}
