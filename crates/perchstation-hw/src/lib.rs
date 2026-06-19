pub mod clock;

/// Hardware-specific capture configuration (`sensor_*` / `camera_*`), decoded
/// from the opaque table `perchstation-core` carries (PS-29/PS-30). Available
/// on all targets so the wiring layer can decode it even where the production
/// adapters below are cfg-gated out.
pub mod capture_config;

#[cfg(target_os = "linux")]
pub mod camera_qr;

#[cfg(target_os = "linux")]
pub mod camera_recorder;

#[cfg(target_os = "linux")]
pub mod motion_sensor;
