//! Production [`Camera`] backed by the Pi camera CLI — `rpicam-vid` by
//! default, configurable via `[capture].camera_command`.
//!
//! The adapter spawns `libcamera-vid` for each call, writes the encoded
//! H.264-in-MP4 output to `<staging>/<recording-id>.mp4`, and awaits the
//! child's exit. The supervisor wraps this call in an outer
//! `tokio::time::timeout(clip_duration + hang_margin)`; if the future is
//! dropped before the child exits the adapter sends `SIGTERM`, waits for
//! a short grace, sends `SIGKILL` if necessary, and removes the partial
//! staging file (the trait's drop-cancellation cleanup contract).
//!
//! Cfg-gated to `target_os = "linux"` because `libcamera-vid` is a
//! Linux-only Pi binary; integration tests use the in-memory fake under
//! `tests/integration/support/fake_camera.rs`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use perchstation_core::hw_traits::{Camera, CameraError, RecordedClip};
use tokio::process::{Child, Command};

const TERMINATE_GRACE: Duration = Duration::from_millis(500);
const DEFAULT_BINARY: &str = "rpicam-vid";

pub struct LibcameraVidCamera {
    binary: PathBuf,
    staging_dir: PathBuf,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate_bps: u64,
}

impl LibcameraVidCamera {
    #[must_use]
    pub fn new(
        staging_dir: impl Into<PathBuf>,
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_bps: u64,
    ) -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            staging_dir: staging_dir.into(),
            width,
            height,
            framerate,
            bitrate_bps,
        }
    }

    /// Override the binary path (wired from `[capture].camera_command`;
    /// also used in tests and on hosts with a non-standard prefix).
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    fn build_command(&self, output: &Path, max_duration_ms: u64) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("--timeout")
            .arg(max_duration_ms.to_string())
            .arg("--codec")
            .arg("h264")
            .arg("--inline")
            .arg("--width")
            .arg(self.width.to_string())
            .arg("--height")
            .arg(self.height.to_string())
            .arg("--framerate")
            .arg(self.framerate.to_string())
            .arg("--bitrate")
            .arg(self.bitrate_bps.to_string())
            .arg("--nopreview")
            .arg("-o")
            .arg(output)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // We do the SIGTERM/SIGKILL dance ourselves so we can also
            // remove the partial staging file on drop-cancellation.
            .kill_on_drop(false);
        cmd
    }
}

/// RAII guard: on drop, terminates the child (SIGTERM → grace → SIGKILL)
/// and removes the partial staging file. Disarmed by [`Self::disarm`]
/// after a clean recording.
struct ChildGuard {
    child: Option<Child>,
    output: Option<PathBuf>,
}

impl ChildGuard {
    fn new(child: Child, output: PathBuf) -> Self {
        Self { child: Some(child), output: Some(output) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child must be present until disarmed or dropped")
    }

    /// Consume the guard after a clean recording: keep the staging file
    /// on disk and stop tracking the (already-exited) child.
    fn disarm(mut self) {
        self.child.take();
        self.output.take();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate_child(&mut child);
        }
        if let Some(path) = self.output.take() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn terminate_child(child: &mut Child) {
    let Some(pid) = child.id() else { return };
    let pid = nix::unistd::Pid::from_raw(pid.cast_signed());
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);

    let deadline = std::time::Instant::now() + TERMINATE_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                    let _ = child.start_kill();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[async_trait]
impl Camera for LibcameraVidCamera {
    async fn record_clip(
        &mut self,
        recording_id: &str,
        max_duration: Duration,
    ) -> Result<RecordedClip, CameraError> {
        tokio::fs::create_dir_all(&self.staging_dir)
            .await
            .map_err(|source| CameraError::Io { source })?;
        let started_at = Utc::now();
        let output = self.staging_dir.join(format!("{recording_id}.mp4"));

        let max_duration_ms = u64::try_from(max_duration.as_millis()).unwrap_or(u64::MAX);
        let mut command = self.build_command(&output, max_duration_ms);
        let child = command.spawn().map_err(|err| {
            CameraError::OpenFailed(format!("spawn `{}` failed: {err}", self.binary.display()))
        })?;
        let mut guard = ChildGuard::new(child, output.clone());

        let status = guard.child_mut().wait().await.map_err(|source| {
            // `child` may have been killed by us during drop; surface the
            // wait error so the supervisor can log it cleanly.
            CameraError::Io { source }
        })?;
        if !status.success() {
            // Guard's Drop will remove the partial file.
            let code = status.code().unwrap_or(-1);
            return Err(CameraError::Aborted(format!(
                "{} exited with status {code}",
                self.binary.display()
            )));
        }

        let metadata = match tokio::fs::metadata(&output).await {
            Ok(m) => m,
            Err(err) => {
                // Guard's Drop removes whatever fragment may exist.
                return Err(CameraError::Io { source: err });
            }
        };
        let byte_size = metadata.len();
        if byte_size == 0 {
            return Err(CameraError::EmptyOutput);
        }

        let ended_at = Utc::now();
        // Success: keep the file on disk, stop tracking the child.
        guard.disarm();
        Ok(RecordedClip { clip_path: output, started_at, ended_at, byte_size })
    }
}
