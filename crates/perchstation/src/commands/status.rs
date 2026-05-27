use perchstation_core::config::Config;

use crate::cli::StatusArgs;

/// `perchstation status` — prints a snapshot and exits.
///
/// Wiring lands here in T015; the snapshot computation lands in T057.
pub fn run(_args: StatusArgs, _config: &Config) -> anyhow::Result<()> {
    unimplemented!("`status` lands in T057")
}
