//! Hardware-specific capture configuration (PS-29/PS-30).
//!
//! `perchstation-core` carries the `sensor_*` / `camera_*` `[capture]` keys
//! opaquely (as `config.capture.hardware`, a `toml::Table`) so the
//! platform-agnostic core never interprets them. This crate — the hardware
//! boundary — owns their schema, their defaults (the `/dev/gpiochip0` device
//! node and the `rpicam-*` binary names), and their validation. The wiring
//! layer (`perchstation serve` / `enroll`) calls [`CaptureHwConfig::from_table`]
//! to decode the opaque table into typed knobs used to build the GPIO sensor
//! and camera adapters.

use std::path::PathBuf;

use serde::Deserialize;

/// Hardware knobs decoded from the opaque `[capture]` hardware table.
///
/// Every field defaults so an empty table (the shipped default, or a config
/// with no hardware overrides) yields the production Pi defaults. Unknown keys
/// are rejected (`deny_unknown_fields`) so a typo in a hardware key — or in an
/// agnostic key that flattened into the table — surfaces at decode time rather
/// than being silently ignored.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureHwConfig {
    /// Path to the Linux gpiochip character device the motion sensor is wired
    /// to. Almost always `/dev/gpiochip0` on a Pi.
    #[serde(default = "default_sensor_gpiochip")]
    pub sensor_gpiochip: PathBuf,
    /// BCM line number the motion sensor's output is wired to.
    #[serde(default = "default_sensor_line")]
    pub sensor_line: u32,
    /// Whether the sensor's asserted state is electrical HIGH (`true`) or LOW.
    #[serde(default = "default_sensor_active_high")]
    pub sensor_active_high: bool,
    /// Camera frame width in pixels passed to the camera command.
    #[serde(default = "default_camera_width")]
    pub camera_width: u32,
    /// Camera frame height in pixels passed to the camera command.
    #[serde(default = "default_camera_height")]
    pub camera_height: u32,
    /// Camera frame rate (frames per second) passed to the camera command.
    #[serde(default = "default_camera_framerate")]
    pub camera_framerate: u32,
    /// Camera target bitrate in bits per second passed to the camera command.
    #[serde(default = "default_camera_bitrate_bps")]
    pub camera_bitrate_bps: u64,
    /// External binary the recorder shells out to for motion clips
    /// (`perchstation serve`). Defaults to the current Pi OS name `rpicam-vid`;
    /// set to `libcamera-vid` on older images.
    #[serde(default = "default_camera_command")]
    pub camera_command: PathBuf,
    /// External binary the enrollment QR still-capture shells out to
    /// (`enroll --qr-source camera`). Defaults to `rpicam-still`; set to
    /// `libcamera-still` on older images.
    #[serde(default = "default_camera_still_command")]
    pub camera_still_command: PathBuf,
}

impl Default for CaptureHwConfig {
    fn default() -> Self {
        Self {
            sensor_gpiochip: default_sensor_gpiochip(),
            sensor_line: default_sensor_line(),
            sensor_active_high: default_sensor_active_high(),
            camera_width: default_camera_width(),
            camera_height: default_camera_height(),
            camera_framerate: default_camera_framerate(),
            camera_bitrate_bps: default_camera_bitrate_bps(),
            camera_command: default_camera_command(),
            camera_still_command: default_camera_still_command(),
        }
    }
}

impl CaptureHwConfig {
    /// Decode the typed hardware knobs from the opaque `[capture]` hardware
    /// table that `perchstation-core` carries. An empty table yields all
    /// defaults; an unknown or mistyped key is rejected.
    ///
    /// # Errors
    /// Returns a [`toml::de::Error`] if a hardware key has the wrong type or
    /// the table contains an unknown key.
    pub fn from_table(table: &toml::Table) -> Result<Self, toml::de::Error> {
        toml::Value::Table(table.clone()).try_into()
    }

    /// Reject an unusable hardware config — an empty camera-binary name can
    /// only ever fail at spawn time, so the wiring layer rejects it up front
    /// (the check core's `validate()` carried before PS-29 moved the fields).
    ///
    /// # Errors
    /// Returns a message naming the offending field.
    pub fn validate(&self) -> Result<(), String> {
        if self.camera_command.as_os_str().is_empty() {
            return Err("capture.camera_command must not be empty".to_owned());
        }
        if self.camera_still_command.as_os_str().is_empty() {
            return Err("capture.camera_still_command must not be empty".to_owned());
        }
        Ok(())
    }
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

fn default_camera_command() -> PathBuf {
    PathBuf::from("rpicam-vid")
}

fn default_camera_still_command() -> PathBuf {
    PathBuf::from("rpicam-still")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_yields_pi_defaults() {
        let hw = CaptureHwConfig::from_table(&toml::Table::new()).expect("empty table decodes");
        assert_eq!(hw.sensor_gpiochip, PathBuf::from("/dev/gpiochip0"));
        assert_eq!(hw.sensor_line, 17);
        assert!(hw.sensor_active_high);
        assert_eq!(hw.camera_width, 1280);
        assert_eq!(hw.camera_height, 720);
        assert_eq!(hw.camera_framerate, 30);
        assert_eq!(hw.camera_bitrate_bps, 4_000_000);
        assert_eq!(hw.camera_command, PathBuf::from("rpicam-vid"));
        assert_eq!(hw.camera_still_command, PathBuf::from("rpicam-still"));
        assert_eq!(hw, CaptureHwConfig::default());
    }

    #[test]
    fn decodes_overridden_keys_with_prior_field_names() {
        // The same field names / defaults operators used before PS-29 still
        // parse, so existing `[capture]` configs keep working.
        let table: toml::Table = toml::from_str(
            "sensor_gpiochip = \"/dev/gpiochip4\"\nsensor_line = 22\nsensor_active_high = false\ncamera_width = 1920\ncamera_height = 1080\ncamera_framerate = 25\ncamera_bitrate_bps = 8000000\ncamera_command = \"libcamera-vid\"\ncamera_still_command = \"libcamera-still\"\n",
        )
        .unwrap();
        let hw = CaptureHwConfig::from_table(&table).expect("decodes");
        assert_eq!(hw.sensor_gpiochip, PathBuf::from("/dev/gpiochip4"));
        assert_eq!(hw.sensor_line, 22);
        assert!(!hw.sensor_active_high);
        assert_eq!(hw.camera_width, 1920);
        assert_eq!(hw.camera_bitrate_bps, 8_000_000);
        assert_eq!(hw.camera_command, PathBuf::from("libcamera-vid"));
        assert_eq!(hw.camera_still_command, PathBuf::from("libcamera-still"));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let table: toml::Table = toml::from_str("sensor_lin = 22\n").unwrap();
        assert!(CaptureHwConfig::from_table(&table).is_err());
    }

    #[test]
    fn validate_rejects_empty_camera_command() {
        let hw = CaptureHwConfig { camera_command: PathBuf::new(), ..CaptureHwConfig::default() };
        assert!(hw.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_camera_still_command() {
        let hw =
            CaptureHwConfig { camera_still_command: PathBuf::new(), ..CaptureHwConfig::default() };
        assert!(hw.validate().is_err());
    }

    #[test]
    fn validate_accepts_defaults() {
        CaptureHwConfig::default().validate().expect("defaults validate");
    }
}
