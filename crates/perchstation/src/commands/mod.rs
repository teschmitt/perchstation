pub mod enroll;
pub mod serve;
pub mod status;

use thiserror::Error;

use crate::cli::exit;

/// Failure mode of a subcommand. The exit-code mapping is the contract
/// surface enumerated in `contracts/cli.md` §Exit codes.
#[derive(Debug, Error)]
pub enum CommandError {
    /// Configuration was unreadable or missing required fields.
    #[error(transparent)]
    Config(anyhow::Error),
    /// Local I/O failure (filesystem unreadable, QR file missing, cert
    /// validation rejected — anything the operator can fix locally).
    #[error(transparent)]
    Io(anyhow::Error),
    /// Transient subsystem failure (network, perchpub 5xx) that retries
    /// could plausibly resolve. Mapped to exit `75` so systemd `Restart=`
    /// policies distinguish this from a permanent state.
    #[error(transparent)]
    Transient(anyhow::Error),
    /// Unrecoverable state — typically "already enrolled, no --force",
    /// "perchpub refused the session", or "cert expired". Exit `76`
    /// signals to the operator that re-enrolling (or otherwise mutating
    /// state by hand) is required.
    #[error(transparent)]
    Unrecoverable(anyhow::Error),
}

impl CommandError {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) => exit::CONFIG,
            Self::Io(_) => exit::IO,
            Self::Transient(_) => exit::TRANSIENT,
            Self::Unrecoverable(_) => exit::UNRECOVERABLE,
        }
    }
}
