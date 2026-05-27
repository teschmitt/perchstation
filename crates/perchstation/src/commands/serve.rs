use perchstation_core::config::Config;

use crate::commands::CommandError;

/// `perchstation serve` — runs the delivery loop and classify-task poller.
///
/// Wiring lands here in T015; the delivery loop body lands in T038/T039.
#[allow(clippy::unused_async, reason = "T038/T039 will introduce the awaits")]
pub async fn run(_config: &Config) -> Result<(), CommandError> {
    unimplemented!("`serve` lands in T038/T039")
}
