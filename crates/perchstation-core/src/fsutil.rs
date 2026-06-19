//! Small shared filesystem-scan helper (PS-31).
//!
//! The status surface and the capture staging gate each reimplemented the
//! same `read_dir` + per-entry filter + `saturating_add` loop (summing media
//! bytes, counting sidecars, summing staging bytes). This centralises that
//! loop; the per-entry filtering and per-entry error tolerance stay the
//! caller's job via the `value` closure.

use std::fs::{self, DirEntry};
use std::io;
use std::path::Path;

/// Fold a `u64` over the top-level entries of `dir`, adding `value(entry)` for
/// each via `saturating_add`. A missing directory yields `0`. The caller's
/// `value` closure decides what each entry contributes (`0` to skip it) and
/// how to tolerate a per-entry error.
pub(crate) fn sum_dir<F>(dir: &Path, mut value: F) -> io::Result<u64>
where
    F: FnMut(&DirEntry) -> io::Result<u64>,
{
    let read = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut total = 0_u64;
    for entry in read {
        total = total.saturating_add(value(&entry?)?);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_directory_sums_to_zero() {
        let dir = TempDir::new().unwrap();
        assert_eq!(sum_dir(&dir.path().join("nope"), |_| Ok(1)).unwrap(), 0);
    }

    #[test]
    fn sums_value_over_entries() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), b"x").unwrap();
        fs::write(dir.path().join("b"), b"y").unwrap();
        assert_eq!(sum_dir(dir.path(), |_| Ok(1)).unwrap(), 2);
    }
}
