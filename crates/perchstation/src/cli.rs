//! Operator CLI surface — `clap`-derived. The contract lives in
//! `specs/001-clip-delivery/contracts/cli.md`.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Exit codes shared with the operator and with systemd unit definitions.
/// Mirrors `contracts/cli.md` §Exit codes verbatim.
#[allow(
    dead_code,
    reason = "contract surface; subcommands wire up the rest in their landing tasks"
)]
pub mod exit {
    pub const OK: i32 = 0;
    pub const USAGE: i32 = 64;
    pub const CONFIG: i32 = 70;
    pub const IO: i32 = 74;
    pub const TRANSIENT: i32 = 75;
    pub const UNRECOVERABLE: i32 = 76;
}

/// `perchstation` — the operator's only first-class interface to the
/// station. Three subcommands cover the entire surface: `enroll` (one-shot
/// provisioning), `serve` (long-lived delivery loop), `status` (snapshot).
#[derive(Debug, Parser)]
#[command(name = "perchstation", version, about = "perchstation — clip delivery to perchpub")]
pub struct Cli {
    /// Path to the operator config file. Optional in development; required
    /// by `serve` in production.
    #[arg(
        long,
        global = true,
        default_value = "/etc/perchstation/config.toml",
        value_name = "PATH"
    )]
    pub config: PathBuf,

    /// `json` (default, journald-friendly) or `text` (human-friendly for
    /// interactive SSH use).
    #[arg(long, global = true, default_value = "json", value_name = "FORMAT")]
    pub log_format: LogFormatArg,

    /// Standard `tracing` `EnvFilter` syntax. Defaults to `info`.
    #[arg(long, global = true, default_value = "info", value_name = "FILTER")]
    pub log_level: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub enum LogFormatArg {
    Json,
    Text,
}

impl From<LogFormatArg> for perchstation_core::observability::tracing::LogFormat {
    fn from(value: LogFormatArg) -> Self {
        match value {
            LogFormatArg::Json => Self::Json,
            LogFormatArg::Text => Self::Text,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Pair this station with a perchpub instance (one-shot, interactive).
    Enroll(EnrollArgs),
    /// Run the delivery loop and classify-task poller.
    Serve,
    /// Print a snapshot of delivery health and exit.
    Status(StatusArgs),
}

#[derive(Debug, clap::Args)]
pub struct EnrollArgs {
    /// `camera` (default) uses the on-board camera; `file` reads a
    /// PNG/JPEG (recovery path).
    #[arg(long, default_value = "camera", value_name = "SOURCE")]
    pub qr_source: QrSourceArg,

    /// Required when `--qr-source=file`.
    #[arg(long, value_name = "PATH")]
    pub qr_file: Option<PathBuf>,

    /// Permit overwriting existing on-disk credentials. Logged prominently;
    /// never silent.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub enum QrSourceArg {
    Camera,
    File,
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Emit a single JSON object instead of the human-readable text form.
    #[arg(long)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_internally_consistent() {
        // `clap::CommandFactory::command().debug_assert()` catches
        // overlapping short flags, broken value-enum derives, etc.
        Cli::command().debug_assert();
    }

    #[test]
    fn enroll_default_flags() {
        let cli = Cli::try_parse_from(["perchstation", "enroll"]).expect("parse");
        match cli.command {
            Command::Enroll(args) => {
                assert_eq!(args.qr_source, QrSourceArg::Camera);
                assert!(args.qr_file.is_none());
                assert!(!args.force);
            }
            _ => panic!("expected Enroll"),
        }
    }

    #[test]
    fn enroll_file_source_with_path() {
        let cli = Cli::try_parse_from([
            "perchstation",
            "enroll",
            "--qr-source",
            "file",
            "--qr-file",
            "/tmp/qr.png",
            "--force",
        ])
        .expect("parse");
        match cli.command {
            Command::Enroll(args) => {
                assert_eq!(args.qr_source, QrSourceArg::File);
                assert_eq!(args.qr_file.as_deref(), Some(std::path::Path::new("/tmp/qr.png")));
                assert!(args.force);
            }
            _ => panic!("expected Enroll"),
        }
    }

    #[test]
    fn status_json_flag() {
        let cli = Cli::try_parse_from(["perchstation", "status", "--json"]).expect("parse");
        match cli.command {
            Command::Status(args) => assert!(args.json),
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn global_flags_are_picked_up_on_any_subcommand() {
        let cli = Cli::try_parse_from([
            "perchstation",
            "--config",
            "/etc/foo.toml",
            "--log-format",
            "text",
            "--log-level",
            "debug",
            "serve",
        ])
        .expect("parse");
        assert_eq!(cli.config, PathBuf::from("/etc/foo.toml"));
        assert_eq!(cli.log_format, LogFormatArg::Text);
        assert_eq!(cli.log_level, "debug");
        assert!(matches!(cli.command, Command::Serve));
    }

    #[test]
    fn exit_codes_match_contract() {
        assert_eq!(exit::OK, 0);
        assert_eq!(exit::USAGE, 64);
        assert_eq!(exit::CONFIG, 70);
        assert_eq!(exit::IO, 74);
        assert_eq!(exit::TRANSIENT, 75);
        assert_eq!(exit::UNRECOVERABLE, 76);
    }
}
