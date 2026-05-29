//! Shared integration-test support.
//!
//! Each integration test under `tests/integration/<name>.rs` brings these
//! modules into scope with:
//!
//! ```ignore
//! #[path = "support/mod.rs"]
//! mod support;
//! ```
//!
//! The pieces:
//! - [`fakepub`] — axum-server perchpub double with internal CA, mintable
//!   station certs, and recorded request state.
//! - [`fixtures`] — primitive helpers: CA + leaf cert generation, QR PNG
//!   rendering, on-disk credentials writer, sample MP4 bytes.
//! - [`fake_qr`] — in-memory [`QrFrameSource`] for in-process tests.
//! - [`fake_clock`] — settable [`Clock`] for tests that need to control
//!   "now" (backoff schedules, cert expiry).
//! - [`harness`] — thin wrappers for the boilerplate every test repeats:
//!   `write_config_toml`, `perchstation_bin`, and the path to the built
//!   binary for `tokio::process::Command` callers.
//! - [`logs`] — parse the station's JSON-on-stderr stream into structured
//!   events plus helpers to match by `event` code.
//!
//! Each top-level module compiles standalone — individual tests may
//! `#[allow(dead_code)]` the modules they don't touch.

#![allow(dead_code)]

pub mod fake_camera;
pub mod fake_clock;
pub mod fake_motion_sensor;
pub mod fake_qr;
pub mod fakepub;
pub mod fixtures;
pub mod harness;
pub mod logs;
