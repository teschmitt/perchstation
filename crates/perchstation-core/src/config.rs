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

impl Default for Config {
    fn default() -> Self {
        Self {
            perchpub_url: None,
            data_dir: default_data_dir(),
            queue: QueueConfig::default(),
            retry: RetryConfig::default(),
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

    /// Reject a config that cannot drive `serve` or `enroll`. `status` can
    /// tolerate a missing [`Config::perchpub_url`] and so does not call this.
    pub fn ensure_runtime_ready(&self) -> Result<(), ConfigError> {
        if self.perchpub_url.as_deref().is_none_or(str::is_empty) {
            return Err(ConfigError::MissingRequired { field: "perchpub_url" });
        }
        Ok(())
    }
}

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
}
