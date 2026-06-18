//! Operator configuration loaded once at process start.
//!
//! The TOML schema is documented in `specs/001-clip-delivery/research.md` R-10.
//! Field-level defaults make the file optional for a development run; only
//! `perchpub_url` is required at runtime (and that is enforced by
//! [`Config::ensure_runtime_ready`], not by `Deserialize`, so `status` can be
//! invoked without a fully-specified config).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file `{path}` could not be read: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config file `{path}` is not valid TOML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("config field `{field}` is required but missing")]
    MissingRequired { field: &'static str },
    #[error("config field `{field}` is out of range: {reason}")]
    OutOfRange { field: &'static str, reason: String },
}

/// Parsed operator configuration. See `research.md` R-10 for the canonical
/// schema and defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Required at runtime: the perchpub origin the station talks to.
    /// Optional in the deserialised struct so `status` can run without it.
    #[serde(default)]
    pub perchpub_url: Option<String>,

    /// Filesystem root for credentials and the queue.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    #[serde(default)]
    pub queue: QueueConfig,

    #[serde(default)]
    pub retry: RetryConfig,

    #[serde(default)]
    pub capture: CaptureConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueConfig {
    #[serde(default = "default_max_clips")]
    pub max_clips: u32,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    #[serde(default)]
    pub eviction: EvictionPolicy,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_clips: default_max_clips(),
            max_bytes: default_max_bytes(),
            eviction: EvictionPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvictionPolicy {
    #[default]
    DropOldestUndelivered,
    RefuseNew,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    #[serde(default = "default_initial_delay_secs")]
    pub initial_delay_secs: u64,
    #[serde(default = "default_max_attempt_delay_secs")]
    pub max_attempt_delay_secs: u64,
    #[serde(default = "default_per_clip_max_attempts")]
    pub per_clip_max_attempts: u32,
    #[serde(default = "default_per_clip_max_wallclock_hours")]
    pub per_clip_max_wallclock_hours: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            initial_delay_secs: default_initial_delay_secs(),
            max_attempt_delay_secs: default_max_attempt_delay_secs(),
            per_clip_max_attempts: default_per_clip_max_attempts(),
            per_clip_max_wallclock_hours: default_per_clip_max_wallclock_hours(),
        }
    }
}

/// `[capture]` section — knobs that tune the motion-triggered capture loop.
///
/// Defaults from research.md R-4 (cooldown, clip duration), R-7
/// (`max_staging_bytes`), and R-10 (assembled view). The hardware-specific
/// `sensor_*` and `camera_*` fields are only consumed by the production
/// adapters in `perchstation-hw`; the platform-agnostic capture supervisor
/// in `perchstation-core` does not see them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    #[serde(default = "default_clip_duration_secs")]
    pub clip_duration_secs: u64,
    #[serde(default = "default_hang_margin_secs")]
    pub hang_margin_secs: u64,
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    #[serde(default = "default_liveness_stuck_secs")]
    pub liveness_stuck_secs: u64,
    #[serde(default = "default_liveness_poll_secs")]
    pub liveness_poll_secs: u64,
    #[serde(default = "default_max_staging_bytes")]
    pub max_staging_bytes: u64,
    #[serde(default = "default_sensor_gpiochip")]
    pub sensor_gpiochip: PathBuf,
    #[serde(default = "default_sensor_line")]
    pub sensor_line: u32,
    #[serde(default = "default_sensor_active_high")]
    pub sensor_active_high: bool,
    #[serde(default = "default_camera_width")]
    pub camera_width: u32,
    #[serde(default = "default_camera_height")]
    pub camera_height: u32,
    #[serde(default = "default_camera_framerate")]
    pub camera_framerate: u32,
    #[serde(default = "default_camera_bitrate_bps")]
    pub camera_bitrate_bps: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            clip_duration_secs: default_clip_duration_secs(),
            hang_margin_secs: default_hang_margin_secs(),
            cooldown_secs: default_cooldown_secs(),
            liveness_stuck_secs: default_liveness_stuck_secs(),
            liveness_poll_secs: default_liveness_poll_secs(),
            max_staging_bytes: default_max_staging_bytes(),
            sensor_gpiochip: default_sensor_gpiochip(),
            sensor_line: default_sensor_line(),
            sensor_active_high: default_sensor_active_high(),
            camera_width: default_camera_width(),
            camera_height: default_camera_height(),
            camera_framerate: default_camera_framerate(),
            camera_bitrate_bps: default_camera_bitrate_bps(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            perchpub_url: None,
            data_dir: default_data_dir(),
            queue: QueueConfig::default(),
            retry: RetryConfig::default(),
            capture: CaptureConfig::default(),
        }
    }
}

impl Config {
    /// Load from a TOML file. If the path does not exist, returns
    /// `Config::default()` — the caller is expected to gate the
    /// "missing `perchpub_url`" case via [`Config::ensure_runtime_ready`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|source| ConfigError::Parse { path: path.to_path_buf(), source }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io { path: path.to_path_buf(), source }),
        }
    }

    /// Parse a config from an in-memory TOML string. Useful for tests and
    /// callers that already have the file contents. (Named
    /// `from_toml_str` to avoid colliding with [`std::str::FromStr`].)
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text)
            .map_err(|source| ConfigError::Parse { path: PathBuf::from("<inline>"), source })
    }

    /// Reject a config that cannot drive `serve` or `enroll`: requires
    /// `perchpub_url` *and* numerically valid bounds. `status` tolerates a
    /// missing [`Config::perchpub_url`] and so calls [`Config::validate`]
    /// directly (or not at all) rather than this.
    pub fn ensure_runtime_ready(&self) -> Result<(), ConfigError> {
        if self.perchpub_url.as_deref().is_none_or(str::is_empty) {
            return Err(ConfigError::MissingRequired { field: "perchpub_url" });
        }
        self.validate()
    }

    /// Validate every operator-tunable numeric bound, independent of the
    /// `perchpub_url` requirement so a caller that tolerates a missing URL
    /// (e.g. `status`) can still reject a malformed config. The bounds are
    /// chosen so the downstream arithmetic in the capture, retry, and queue
    /// subsystems can never overflow or panic (PS-03, PS-08): a clip plus
    /// its hang margin stays well inside `Duration`, `hours * 3600` cannot
    /// overflow `u64`, and the backoff math stays inside `from_secs_f64`'s
    /// representable range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let oor = |field: &'static str, reason: &str| ConfigError::OutOfRange {
            field,
            reason: reason.to_owned(),
        };

        // Queue ceilings (PS-03): a zero ceiling bricks the queue.
        if self.queue.max_clips == 0 {
            return Err(oor("queue.max_clips", "must be >= 1"));
        }
        if self.queue.max_bytes == 0 {
            return Err(oor("queue.max_bytes", "must be >= 1"));
        }

        // Capture timing. Bounding `clip_duration_secs` to `[1, 3600]` and
        // `hang_margin_secs` to `<= 600` guarantees their sum (the outer
        // record timeout) never overflows `Duration`.
        if !(1..=MAX_CLIP_DURATION_SECS).contains(&self.capture.clip_duration_secs) {
            return Err(oor("capture.clip_duration_secs", "must be in 1..=3600"));
        }
        if self.capture.hang_margin_secs > MAX_HANG_MARGIN_SECS {
            return Err(oor("capture.hang_margin_secs", "must be <= 600"));
        }
        if self.capture.cooldown_secs == 0 {
            return Err(oor("capture.cooldown_secs", "must be >= 1"));
        }
        if self.capture.liveness_stuck_secs == 0 {
            return Err(oor("capture.liveness_stuck_secs", "must be >= 1"));
        }
        if self.capture.liveness_poll_secs == 0 {
            return Err(oor("capture.liveness_poll_secs", "must be >= 1"));
        }

        // Retry / backoff. `per_clip_max_wallclock_hours` is capped so the
        // `* 3600` conversion cannot overflow `u64`; the delay ceiling is
        // capped so the backoff math stays inside `from_secs_f64`'s range.
        if self.retry.per_clip_max_attempts == 0 {
            return Err(oor("retry.per_clip_max_attempts", "must be >= 1"));
        }
        if !(1..=MAX_WALLCLOCK_HOURS).contains(&self.retry.per_clip_max_wallclock_hours) {
            return Err(oor("retry.per_clip_max_wallclock_hours", "must be in 1..=8760"));
        }
        if self.retry.max_attempt_delay_secs > MAX_RETRY_DELAY_SECS {
            return Err(oor("retry.max_attempt_delay_secs", "must be <= 604800 (7 days)"));
        }
        if self.retry.initial_delay_secs > self.retry.max_attempt_delay_secs {
            return Err(oor("retry.initial_delay_secs", "must be <= retry.max_attempt_delay_secs"));
        }

        Ok(())
    }
}

/// Upper bound on a single clip's recording duration (1 hour).
const MAX_CLIP_DURATION_SECS: u64 = 3600;
/// Upper bound on the post-clip hang margin (10 minutes).
const MAX_HANG_MARGIN_SECS: u64 = 600;
/// Upper bound on `per_clip_max_wallclock_hours` (1 year) so `* 3600`
/// cannot overflow `u64`.
const MAX_WALLCLOCK_HOURS: u64 = 24 * 365;
/// Upper bound on retry delays (7 days) so the backoff math stays far
/// below `Duration::from_secs_f64`'s panic threshold.
const MAX_RETRY_DELAY_SECS: u64 = 7 * 24 * 3600;

const fn default_max_clips() -> u32 {
    500
}

const fn default_max_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

const fn default_initial_delay_secs() -> u64 {
    10
}

const fn default_max_attempt_delay_secs() -> u64 {
    3600
}

const fn default_per_clip_max_attempts() -> u32 {
    12
}

const fn default_per_clip_max_wallclock_hours() -> u64 {
    24
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/perchstation")
}

const fn default_clip_duration_secs() -> u64 {
    8
}

const fn default_hang_margin_secs() -> u64 {
    2
}

const fn default_cooldown_secs() -> u64 {
    30
}

const fn default_liveness_stuck_secs() -> u64 {
    300
}

const fn default_liveness_poll_secs() -> u64 {
    5
}

const fn default_max_staging_bytes() -> u64 {
    256 * 1024 * 1024
}

fn default_sensor_gpiochip() -> PathBuf {
    PathBuf::from("/dev/gpiochip0")
}

const fn default_sensor_line() -> u32 {
    17
}

const fn default_sensor_active_high() -> bool {
    true
}

const fn default_camera_width() -> u32 {
    1280
}

const fn default_camera_height() -> u32 {
    720
}

const fn default_camera_framerate() -> u32 {
    30
}

const fn default_camera_bitrate_bps() -> u64 {
    4_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_research_r10() {
        let cfg = Config::default();
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/perchstation"));
        assert_eq!(cfg.queue.max_clips, 500);
        assert_eq!(cfg.queue.max_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(cfg.queue.eviction, EvictionPolicy::DropOldestUndelivered);
        assert_eq!(cfg.retry.initial_delay_secs, 10);
        assert_eq!(cfg.retry.max_attempt_delay_secs, 3600);
        assert_eq!(cfg.retry.per_clip_max_attempts, 12);
        assert_eq!(cfg.retry.per_clip_max_wallclock_hours, 24);
    }

    #[test]
    fn parses_the_example_from_research_r10() {
        let toml = r#"
            perchpub_url = "https://perchpub.example.org"
            data_dir     = "/var/lib/perchstation"

            [queue]
            max_clips = 500
            max_bytes = 2147483648
            eviction  = "drop_oldest_undelivered"

            [retry]
            initial_delay_secs           = 10
            max_attempt_delay_secs       = 3600
            per_clip_max_attempts        = 12
            per_clip_max_wallclock_hours = 24
        "#;
        let cfg = Config::from_toml_str(toml).expect("parses");
        assert_eq!(cfg.perchpub_url.as_deref(), Some("https://perchpub.example.org"));
        cfg.ensure_runtime_ready().expect("runtime ready");
    }

    #[test]
    fn refuse_new_eviction_round_trips() {
        let cfg = Config::from_toml_str("[queue]\neviction = \"refuse_new\"\n").expect("parses");
        assert_eq!(cfg.queue.eviction, EvictionPolicy::RefuseNew);
    }

    #[test]
    fn ensure_runtime_ready_rejects_missing_perchpub_url() {
        let cfg = Config::default();
        let err = cfg.ensure_runtime_ready().unwrap_err();
        assert!(matches!(err, ConfigError::MissingRequired { field: "perchpub_url" }));
    }

    #[test]
    fn ensure_runtime_ready_rejects_empty_perchpub_url() {
        let cfg = Config::from_toml_str("perchpub_url = \"\"\n").expect("parses");
        assert!(cfg.ensure_runtime_ready().is_err());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = Config::from_toml_str("bogus = 42\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn missing_file_returns_defaults() {
        let cfg = Config::load(Path::new("/definitely/does/not/exist.toml")).expect("load ok");
        assert_eq!(cfg.queue.max_clips, 500);
        assert!(cfg.perchpub_url.is_none());
    }

    #[test]
    fn validate_accepts_research_r10_defaults() {
        // The shipped defaults (research.md R-10) must always validate.
        Config::default().validate().expect("R-10 defaults validate");
    }

    #[test]
    fn validate_rejects_zero_max_clips() {
        let mut cfg = Config::default();
        cfg.queue.max_clips = 0;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::OutOfRange { field: "queue.max_clips", .. })
        ));
    }

    #[test]
    fn validate_rejects_zero_max_bytes() {
        let mut cfg = Config::default();
        cfg.queue.max_bytes = 0;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::OutOfRange { field: "queue.max_bytes", .. })
        ));
    }

    #[test]
    fn validate_rejects_clip_plus_margin_overflow() {
        // A clip duration this large would overflow `clip + hang_margin`
        // in `record_into_staging`; the range bound rejects it up front.
        let mut cfg = Config::default();
        cfg.capture.clip_duration_secs = u64::MAX;
        cfg.capture.hang_margin_secs = 1;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::OutOfRange { field: "capture.clip_duration_secs", .. })
        ));
    }

    #[test]
    fn validate_rejects_huge_wallclock_hours() {
        let mut cfg = Config::default();
        cfg.retry.per_clip_max_wallclock_hours = u64::MAX;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::OutOfRange { field: "retry.per_clip_max_wallclock_hours", .. })
        ));
    }

    #[test]
    fn validate_rejects_huge_max_attempt_delay_secs() {
        let mut cfg = Config::default();
        cfg.retry.max_attempt_delay_secs = u64::MAX;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::OutOfRange { field: "retry.max_attempt_delay_secs", .. })
        ));
    }

    #[test]
    fn ensure_runtime_ready_rejects_zero_max_clips() {
        let mut cfg =
            Config::from_toml_str("perchpub_url = \"https://p.example\"\n").expect("parses");
        cfg.queue.max_clips = 0;
        // perchpub_url is valid; the offending numeric bound must still fail.
        assert!(cfg.ensure_runtime_ready().is_err());
    }

    #[test]
    fn ensure_runtime_ready_rejects_zero_max_bytes() {
        let mut cfg =
            Config::from_toml_str("perchpub_url = \"https://p.example\"\n").expect("parses");
        cfg.queue.max_bytes = 0;
        assert!(cfg.ensure_runtime_ready().is_err());
    }
}
