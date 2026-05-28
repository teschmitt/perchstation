//! Pure-read snapshot of delivery health, surfaced by `perchstation status`
//! (T057) and consumed verbatim by `tests/integration/status_surface.rs`.
//!
//! Everything here is **read-only** with respect to `data_dir` — `status`
//! is documented to be safe to run alongside `serve` (`contracts/cli.md`).
//!
//! Layout follows the JSON schema in `contracts/cli.md` §`perchstation
//! status` so external tooling can parse the output without a station-side
//! library dependency.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::identity::{CREDENTIALS_DIR, IDENTITY_FILE, IdentityError, StationIdentity};
use crate::perchpub::types::ClassifyTaskStatus;
use crate::queue::{ClipQueueEntry, Outcome};

const QUEUE_DIR: &str = "queue";
const PENDING: &str = "pending";
const INFLIGHT: &str = "inflight";
const DELIVERED: &str = "delivered";
const RECENT_LIMIT: usize = 3;

#[derive(Debug, Error)]
pub enum StatusError {
    #[error("could not read `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse sidecar `{path}`: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("identity load failed: {0}")]
    Identity(#[source] IdentityError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EnrollmentState {
    Ok,
    Missing,
    Expired,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrollmentSnapshot {
    pub state: EnrollmentState,
    pub station_id: Option<Uuid>,
    #[serde(serialize_with = "serialize_opt_rfc3339_z")]
    pub cert_not_after: Option<DateTime<Utc>>,
    pub perchpub_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueSnapshot {
    pub pending: u32,
    pub inflight: u32,
    pub bytes_on_disk: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuccessSnapshot {
    #[serde(serialize_with = "serialize_rfc3339_z")]
    pub at: DateTime<Utc>,
    pub clip_id: String,
    pub classify_task_id: Option<Uuid>,
    pub classify_status: Option<ClassifyTaskStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureSnapshot {
    #[serde(serialize_with = "serialize_rfc3339_z")]
    pub at: DateTime<Utc>,
    pub clip_id: String,
    pub kind: String,
    pub status: Option<u16>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentEntry {
    pub clip_id: String,
    pub classify_status: Option<ClassifyTaskStatus>,
    #[serde(serialize_with = "serialize_rfc3339_z")]
    pub delivered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    pub enrollment: EnrollmentSnapshot,
    pub queue: QueueSnapshot,
    pub last_success: Option<SuccessSnapshot>,
    pub last_failure: Option<FailureSnapshot>,
    pub recent: Vec<RecentEntry>,
}

/// Capture-side projection rendered into `perchstation status` (text +
/// JSON). See `specs/002-capture-subsystem/contracts/cli.md` §`status`
/// for the field schema and rendering rules.
///
/// The default value represents "the capture task has not run in this
/// process" — used by `status` invocations outside of `serve`. It is
/// deliberately distinct from `Healthy`: `NeverObserved` is the
/// explicit "no data yet" signal, while `Healthy` is only published
/// after the supervisor's first successful liveness probe.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CaptureSnapshot {
    #[serde(serialize_with = "serialize_opt_rfc3339_z")]
    pub last_recording_at: Option<DateTime<Utc>>,
    pub last_clip_id: Option<String>,
    pub last_failure: Option<CaptureFailureSnapshot>,
    pub sensor_liveness: CaptureLivenessSnapshot,
    #[serde(serialize_with = "serialize_opt_rfc3339_z")]
    pub sensor_degraded_since: Option<DateTime<Utc>>,
}

impl Default for CaptureSnapshot {
    fn default() -> Self {
        Self {
            last_recording_at: None,
            last_clip_id: None,
            last_failure: None,
            sensor_liveness: CaptureLivenessSnapshot::NeverObserved,
            sensor_degraded_since: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CaptureFailureSnapshot {
    #[serde(serialize_with = "serialize_rfc3339_z")]
    pub at: DateTime<Utc>,
    pub kind: String,
    pub message: String,
}

/// Sensor-liveness projection. The `serde` representation is
/// `lower_snake_case` to match the JSON contract in `cli.md`.
///
/// `Default` is `NeverObserved` so [`CaptureSnapshot::default`] and
/// [`crate::capture::state::CaptureState::new`] both produce the
/// explicit "no liveness probe has run yet" reading.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaptureLivenessSnapshot {
    #[default]
    NeverObserved,
    Healthy,
    StuckAsserted,
    Unavailable,
}

/// Build a snapshot of delivery health from the on-disk state at `data_dir`.
/// Pure read; never mutates anything under `data_dir`.
pub fn snapshot(data_dir: &Path, now: DateTime<Utc>) -> Result<StatusSnapshot, StatusError> {
    let enrollment = enrollment_snapshot(data_dir, now)?;
    let queue = queue_snapshot(data_dir)?;
    let delivered = load_delivered(data_dir)?;
    let last_success = pick_last_success(&delivered);
    let last_failure = pick_last_failure(&delivered);
    let recent = build_recent(&delivered);
    Ok(StatusSnapshot { enrollment, queue, last_success, last_failure, recent })
}

fn enrollment_snapshot(
    data_dir: &Path,
    now: DateTime<Utc>,
) -> Result<EnrollmentSnapshot, StatusError> {
    let identity_path = data_dir.join(CREDENTIALS_DIR).join(IDENTITY_FILE);
    if !identity_path.exists() {
        return Ok(EnrollmentSnapshot {
            state: EnrollmentState::Missing,
            station_id: None,
            cert_not_after: None,
            perchpub_url: None,
        });
    }

    let identity = StationIdentity::load(data_dir).map_err(StatusError::Identity)?;
    let state =
        if identity.cert_is_expired(now) { EnrollmentState::Expired } else { EnrollmentState::Ok };
    Ok(EnrollmentSnapshot {
        state,
        station_id: Some(identity.station_id),
        cert_not_after: Some(identity.cert_not_after),
        perchpub_url: Some(identity.perchpub_url),
    })
}

fn queue_snapshot(data_dir: &Path) -> Result<QueueSnapshot, StatusError> {
    let queue_root = data_dir.join(QUEUE_DIR);
    let pending_dir = queue_root.join(PENDING);
    let inflight_dir = queue_root.join(INFLIGHT);

    let pending = count_sidecars(&pending_dir)?;
    let inflight = count_sidecars(&inflight_dir)?;
    let bytes_on_disk = sum_mp4_bytes(&pending_dir)? + sum_mp4_bytes(&inflight_dir)?;

    Ok(QueueSnapshot { pending, inflight, bytes_on_disk })
}

fn count_sidecars(dir: &Path) -> Result<u32, StatusError> {
    let read = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(StatusError::Io { path: dir.to_path_buf(), source }),
    };
    let mut count = 0_u32;
    for entry in read {
        let entry = entry.map_err(|source| StatusError::Io { path: dir.to_path_buf(), source })?;
        if entry.path().extension().is_some_and(|e| e == "json") {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn sum_mp4_bytes(dir: &Path) -> Result<u64, StatusError> {
    let read = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(StatusError::Io { path: dir.to_path_buf(), source }),
    };
    let mut total = 0_u64;
    for entry in read {
        let entry = entry.map_err(|source| StatusError::Io { path: dir.to_path_buf(), source })?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "mp4") {
            let meta = entry
                .metadata()
                .map_err(|source| StatusError::Io { path: path.clone(), source })?;
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

fn load_delivered(data_dir: &Path) -> Result<Vec<ClipQueueEntry>, StatusError> {
    let delivered_dir = data_dir.join(QUEUE_DIR).join(DELIVERED);
    let read = match fs::read_dir(&delivered_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(StatusError::Io { path: delivered_dir, source }),
    };
    let mut entries = Vec::new();
    for entry in read {
        let entry =
            entry.map_err(|source| StatusError::Io { path: delivered_dir.clone(), source })?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let bytes =
            fs::read(&path).map_err(|source| StatusError::Io { path: path.clone(), source })?;
        let parsed: ClipQueueEntry = serde_json::from_slice(&bytes)
            .map_err(|source| StatusError::Parse { path: path.clone(), source })?;
        entries.push(parsed);
    }
    Ok(entries)
}

fn pick_last_success(delivered: &[ClipQueueEntry]) -> Option<SuccessSnapshot> {
    delivered
        .iter()
        .filter(|e| e.outcome == Some(Outcome::Delivered))
        .filter(|e| e.delivered_at.is_some())
        .max_by_key(|e| e.delivered_at.expect("delivered_at filter above"))
        .map(|e| SuccessSnapshot {
            at: e.delivered_at.expect("delivered_at filter above"),
            clip_id: e.clip_id.clone(),
            classify_task_id: e.classify_task_id,
            classify_status: e.last_classify_status,
        })
}

fn pick_last_failure(delivered: &[ClipQueueEntry]) -> Option<FailureSnapshot> {
    delivered
        .iter()
        .filter(|e| e.outcome == Some(Outcome::Undeliverable) || e.last_error.is_some())
        .filter(|e| e.delivered_at.is_some() || e.last_attempt_at.is_some())
        .max_by_key(|e| e.delivered_at.or(e.last_attempt_at).expect("filter above"))
        .map(|e| {
            let at = e.delivered_at.or(e.last_attempt_at).expect("filter above");
            let (kind, status, message) = e.last_error.as_ref().map_or_else(
                || ("undeliverable".to_string(), None, String::new()),
                |le| (le.kind.clone(), le.status, le.message.clone()),
            );
            FailureSnapshot { at, clip_id: e.clip_id.clone(), kind, status, message }
        })
}

fn build_recent(delivered: &[ClipQueueEntry]) -> Vec<RecentEntry> {
    let mut sorted: Vec<&ClipQueueEntry> =
        delivered.iter().filter(|e| e.delivered_at.is_some()).collect();
    sorted.sort_by(|a, b| {
        b.delivered_at.expect("filter above").cmp(&a.delivered_at.expect("filter above"))
    });
    sorted
        .into_iter()
        .take(RECENT_LIMIT)
        .map(|e| RecentEntry {
            clip_id: e.clip_id.clone(),
            classify_status: e.last_classify_status,
            delivered_at: e.delivered_at.expect("filter above"),
        })
        .collect()
}

impl StatusSnapshot {
    /// Human-readable rendering for `perchstation status` (no `--json`).
    /// Matches the example in `contracts/cli.md` §`perchstation status`.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();

        // Enrollment line.
        match self.enrollment.state {
            EnrollmentState::Ok => {
                let sid = short_uuid(self.enrollment.station_id);
                let exp = format_cert_date(self.enrollment.cert_not_after);
                push_line(
                    &mut out,
                    &format!("Enrollment:    OK (station {sid}, cert expires {exp})"),
                );
            }
            EnrollmentState::Missing => {
                push_line(&mut out, "Enrollment:    not enrolled (no credentials on disk)");
            }
            EnrollmentState::Expired => {
                let sid = short_uuid(self.enrollment.station_id);
                let exp = format_cert_date(self.enrollment.cert_not_after);
                push_line(
                    &mut out,
                    &format!("Enrollment:    EXPIRED (station {sid}, cert expired {exp})"),
                );
            }
        }

        // Queue depth line.
        let total_clips = self.queue.pending.saturating_add(self.queue.inflight);
        let bytes_pretty = pretty_bytes(self.queue.bytes_on_disk);
        push_line(
            &mut out,
            &format!("Queue depth:   {total_clips} clips ({bytes_pretty} on disk)"),
        );

        // Last success / failure lines (each optional).
        if let Some(s) = &self.last_success {
            let when = s.at.format("%Y-%m-%d %H:%M:%S UTC");
            let cs = s.classify_status.map(|c| format!(" classify={c:?}")).unwrap_or_default();
            push_line(&mut out, &format!("Last success:  {when}  {}{cs}", s.clip_id));
        }
        if let Some(f) = &self.last_failure {
            let when = f.at.format("%Y-%m-%d %H:%M:%S UTC");
            let tail = match f.status {
                Some(code) => format!("perchpub {code}"),
                None => f.kind.clone(),
            };
            push_line(&mut out, &format!("Last failure:  {when}  {tail}"));
        }

        // Recent deliveries.
        if !self.recent.is_empty() {
            push_line(&mut out, &format!("Last {} deliveries:", self.recent.len()));
            for r in &self.recent {
                let when = r.delivered_at.format("%Y-%m-%d %H:%M");
                let cs = r.classify_status.map_or_else(|| "?".to_string(), |c| format!("{c:?}"));
                push_line(&mut out, &format!("  {when}  {}  classify={cs}", r.clip_id));
            }
        }

        out
    }
}

fn push_line(buf: &mut String, line: &str) {
    buf.push_str(line);
    buf.push('\n');
}

fn short_uuid(id: Option<Uuid>) -> String {
    id.map_or_else(
        || "?".to_string(),
        |u| {
            let s = u.to_string();
            s.chars().take(8).collect::<String>() + ".."
        },
    )
}

fn format_cert_date(dt: Option<DateTime<Utc>>) -> String {
    dt.map_or_else(|| "?".to_string(), |d| d.format("%Y-%m-%d").to_string())
}

/// Pretty-print a byte count as MB / kB / B. The exact thresholds match
/// what the contract example uses (`12.4 MB`); the unit is the SI prefix
/// (10^6) for round numbers the operator recognises rather than the
/// binary IEC prefix.
#[allow(
    clippy::cast_precision_loss,
    reason = "operator-facing display value; one-decimal precision is fine for queue sizes well under exabytes"
)]
fn pretty_bytes(n: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    if n >= GB {
        format!("{:.1} GB", (n as f64) / (GB as f64))
    } else if n >= MB {
        format!("{:.1} MB", (n as f64) / (MB as f64))
    } else if n >= KB {
        format!("{:.1} kB", (n as f64) / (KB as f64))
    } else {
        format!("{n} B")
    }
}

fn serialize_rfc3339_z<S: serde::Serializer>(
    dt: &DateTime<Utc>,
    ser: S,
) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&dt.to_rfc3339_opts(SecondsFormat::Secs, true))
}

#[allow(
    clippy::ref_option,
    reason = "serde's `serialize_with` signature is `fn(&T, S) -> Result<_, _>`, even for Option fields"
)]
fn serialize_opt_rfc3339_z<S: serde::Serializer>(
    dt: &Option<DateTime<Utc>>,
    ser: S,
) -> Result<S::Ok, S::Error> {
    match dt {
        Some(d) => ser.serialize_str(&d.to_rfc3339_opts(SecondsFormat::Secs, true)),
        None => ser.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn instant(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn missing_credentials_yields_missing_state() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot(dir.path(), Utc::now()).unwrap();
        assert_eq!(snap.enrollment.state, EnrollmentState::Missing);
        assert!(snap.enrollment.station_id.is_none());
        assert!(snap.enrollment.cert_not_after.is_none());
        assert_eq!(snap.queue.pending, 0);
        assert_eq!(snap.queue.inflight, 0);
        assert_eq!(snap.queue.bytes_on_disk, 0);
        assert!(snap.last_success.is_none());
        assert!(snap.last_failure.is_none());
        assert!(snap.recent.is_empty());
    }

    #[test]
    fn empty_queue_dirs_count_as_zero() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("queue/pending")).unwrap();
        fs::create_dir_all(dir.path().join("queue/inflight")).unwrap();
        fs::create_dir_all(dir.path().join("queue/delivered")).unwrap();
        let snap = snapshot(dir.path(), Utc::now()).unwrap();
        assert_eq!(snap.queue.pending, 0);
        assert_eq!(snap.queue.inflight, 0);
        assert_eq!(snap.queue.bytes_on_disk, 0);
    }

    #[test]
    fn bytes_on_disk_sums_mp4_sizes_across_pending_and_inflight() {
        let dir = TempDir::new().unwrap();
        let pending = dir.path().join("queue/pending");
        let inflight = dir.path().join("queue/inflight");
        fs::create_dir_all(&pending).unwrap();
        fs::create_dir_all(&inflight).unwrap();
        fs::write(pending.join("a.mp4"), vec![0u8; 1024]).unwrap();
        fs::write(pending.join("a.json"), "{}").unwrap();
        fs::write(inflight.join("b.mp4"), vec![0u8; 2048]).unwrap();
        fs::write(inflight.join("b.json"), "{}").unwrap();

        let snap = queue_snapshot(dir.path()).unwrap();
        assert_eq!(snap.bytes_on_disk, 3072);
    }

    #[test]
    fn pick_last_success_returns_most_recent_delivered() {
        let mut a = ClipQueueEntry::new("a", instant("2026-05-27T06:00:00Z"), Utc::now(), 1);
        a.outcome = Some(Outcome::Delivered);
        a.delivered_at = Some(instant("2026-05-27T06:00:00Z"));
        a.last_classify_status = Some(ClassifyTaskStatus::Queued);
        let mut b = ClipQueueEntry::new("b", instant("2026-05-27T07:00:00Z"), Utc::now(), 1);
        b.outcome = Some(Outcome::Delivered);
        b.delivered_at = Some(instant("2026-05-27T07:00:00Z"));
        b.last_classify_status = Some(ClassifyTaskStatus::Success);
        let mut c = ClipQueueEntry::new("c", instant("2026-05-27T08:00:00Z"), Utc::now(), 1);
        c.outcome = Some(Outcome::Undeliverable);
        c.delivered_at = Some(instant("2026-05-27T08:00:00Z"));

        let picked = pick_last_success(&[a, b, c]).unwrap();
        assert_eq!(picked.clip_id, "b");
        assert_eq!(picked.classify_status, Some(ClassifyTaskStatus::Success));
    }

    #[test]
    fn pretty_bytes_picks_unit_per_threshold() {
        assert_eq!(pretty_bytes(0), "0 B");
        assert_eq!(pretty_bytes(999), "999 B");
        assert_eq!(pretty_bytes(1_000), "1.0 kB");
        assert_eq!(pretty_bytes(1_234_567), "1.2 MB");
        assert_eq!(pretty_bytes(2_500_000_000), "2.5 GB");
    }

    #[test]
    fn cert_not_after_serializes_with_z_suffix() {
        let dt = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).single().unwrap();
        let snap = EnrollmentSnapshot {
            state: EnrollmentState::Expired,
            station_id: None,
            cert_not_after: Some(dt),
            perchpub_url: None,
        };
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["cert_not_after"].as_str(), Some("2024-01-01T00:00:00Z"));
    }
}
