use perchstation_core::config::Config;

use crate::cli::EnrollArgs;

/// `perchstation enroll` — pairs the station with a perchpub instance.
///
/// Wiring lands here in T015; the actual enrollment exchange (QR decode →
/// CSR → mTLS POST → atomic persist) lands in T027.
pub fn run(_args: EnrollArgs, _config: &Config) -> anyhow::Result<()> {
    unimplemented!("`enroll` lands in T027")
}
