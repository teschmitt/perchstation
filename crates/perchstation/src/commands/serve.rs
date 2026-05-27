use perchstation_core::config::Config;

/// `perchstation serve` — runs the delivery loop and classify-task poller.
///
/// Wiring lands here in T015; the delivery loop body lands in T038/T039.
pub fn run(_config: &Config) -> anyhow::Result<()> {
    unimplemented!("`serve` lands in T038/T039")
}
