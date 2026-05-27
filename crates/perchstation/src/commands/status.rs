use perchstation_core::config::Config;

use crate::cli::StatusArgs;
use crate::commands::CommandError;

/// `perchstation status` — prints a snapshot and exits.
///
/// Wiring lands here in T015; the snapshot computation lands in T057.
#[allow(clippy::unused_async, reason = "T057 will introduce filesystem awaits")]
pub async fn run(_args: StatusArgs, _config: &Config) -> Result<(), CommandError> {
    unimplemented!("`status` lands in T057")
}
