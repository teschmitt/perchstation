//! Staging directory layout + startup purge (FR-017).
//!
//! `<data_dir>/capture-staging/` holds at most one in-progress recording at
//! a time. The directory is purged before `capture.ready` on every boot so
//! a crash mid-record can never accumulate junk across reboots
//! (data-model.md §Staging layout / spec FR-017 / SC-003).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::observability::tracing as obs_tracing;

/// The capture-side staging directory. Newtype rather than a bare
/// `PathBuf` so the supervisor cannot accidentally hand the queue a
/// non-staging path.
#[derive(Debug, Clone)]
pub struct StagingDir(PathBuf);

impl StagingDir {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }
}

impl AsRef<Path> for StagingDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Outcome of a [`purge`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeReport {
    pub removed_files: u32,
    pub removed_bytes: u64,
}

/// Create the staging directory if it does not exist, then remove every
/// file inside it. Emits `capture.staging_purged` with the counts. Returns
/// `(removed_files, removed_bytes)` so the caller can echo them on the
/// `capture.ready` event (T018).
pub fn purge(staging_dir: &Path) -> io::Result<PurgeReport> {
    fs::create_dir_all(staging_dir)?;

    let mut removed_files: u32 = 0;
    let mut removed_bytes: u64 = 0;
    for entry in fs::read_dir(staging_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if !metadata.is_file() {
            continue;
        }
        let len = metadata.len();
        match fs::remove_file(&path) {
            Ok(()) => {
                removed_files = removed_files.saturating_add(1);
                removed_bytes = removed_bytes.saturating_add(len);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }

    tracing::debug!(
        event = obs_tracing::events::CAPTURE_STAGING_PURGED,
        removed_files,
        removed_bytes,
        "capture staging purge complete",
    );

    Ok(PurgeReport { removed_files, removed_bytes })
}

/// Sum the size of every regular file directly under `staging_dir`.
///
/// Used by the supervisor's pre-record disk-pressure gate (T032) to
/// compare the current capture-side footprint against
/// `CaptureConfig::max_staging_bytes` before invoking the camera.
/// Returns `0` if the directory does not exist yet (the supervisor
/// creates it during the startup purge).
pub fn staging_bytes(staging_dir: &Path) -> io::Result<u64> {
    // PS-31: share the one `read_dir` + saturating-fold scanner. Each regular
    // file contributes its size; a file that vanished mid-scan, and any
    // non-file entry, contributes 0.
    crate::fsutil::sum_dir(staging_dir, |entry| match entry.metadata() {
        Ok(m) if m.is_file() => Ok(m.len()),
        Ok(_) => Ok(0),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn purge_creates_missing_directory_and_reports_zero() {
        let dir = TempDir::new().unwrap();
        let staging = dir.path().join("capture-staging");
        let report = purge(&staging).expect("purge");
        assert!(staging.is_dir());
        assert_eq!(report.removed_files, 0);
        assert_eq!(report.removed_bytes, 0);
    }

    #[test]
    fn purge_removes_files_and_reports_counts() {
        let dir = TempDir::new().unwrap();
        let staging = dir.path().join("capture-staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("a.mp4"), vec![0u8; 100]).unwrap();
        fs::write(staging.join("b.mp4"), vec![0u8; 250]).unwrap();

        let report = purge(&staging).expect("purge");
        assert_eq!(report.removed_files, 2);
        assert_eq!(report.removed_bytes, 350);
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
    }

    #[test]
    fn purge_leaves_subdirectories_alone() {
        let dir = TempDir::new().unwrap();
        let staging = dir.path().join("capture-staging");
        fs::create_dir_all(staging.join("nested")).unwrap();
        fs::write(staging.join("on-top.mp4"), b"x").unwrap();

        let report = purge(&staging).expect("purge");
        assert_eq!(report.removed_files, 1);
        assert!(staging.join("nested").is_dir());
    }

    #[test]
    fn staging_bytes_returns_zero_for_missing_directory() {
        let dir = TempDir::new().unwrap();
        let staging = dir.path().join("does-not-exist");
        let total = staging_bytes(&staging).expect("staging_bytes");
        assert_eq!(total, 0);
    }

    #[test]
    fn staging_bytes_sums_file_sizes_ignoring_subdirectories() {
        let dir = TempDir::new().unwrap();
        let staging = dir.path().join("capture-staging");
        fs::create_dir_all(staging.join("nested")).unwrap();
        fs::write(staging.join("a.mp4"), vec![0u8; 100]).unwrap();
        fs::write(staging.join("b.mp4"), vec![0u8; 250]).unwrap();
        // A nested file is not counted (we only sum top-level files).
        fs::write(staging.join("nested").join("c.mp4"), vec![0u8; 999]).unwrap();
        let total = staging_bytes(&staging).expect("staging_bytes");
        assert_eq!(total, 350);
    }
}
