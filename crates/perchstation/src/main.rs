#![deny(unsafe_code)]

mod cli;
mod commands;

use clap::Parser;
use perchstation_core::config::{Config, ConfigError};
use perchstation_core::observability::tracing::{self as obs_tracing, LogFormat};

use crate::cli::{Cli, Command, exit};

fn main() {
    let args = Cli::parse();

    // Initialise tracing before we try to load config so config errors are
    // emitted as structured log events themselves.
    let log_format: LogFormat = args.log_format.into();
    if let Err(err) = obs_tracing::init(log_format, &args.log_level) {
        eprintln!("perchstation: could not initialise logging: {err}");
        std::process::exit(exit::USAGE);
    }

    let config = match Config::load(&args.config) {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::error!(
                event = obs_tracing::events::SERVICE_CONFIG_INVALID,
                path = %args.config.display(),
                message = %err,
                "config load failed"
            );
            exit_with(&err);
        }
    };

    let result = match args.command {
        Command::Enroll(enroll_args) => commands::enroll::run(enroll_args, &config),
        Command::Serve => commands::serve::run(&config),
        Command::Status(status_args) => commands::status::run(status_args, &config),
    };

    match result {
        Ok(()) => std::process::exit(exit::OK),
        Err(err) => {
            tracing::error!(message = %err, "command failed");
            // Default failure code is `IO`; subcommands that need to signal
            // a more specific code (CONFIG / TRANSIENT / UNRECOVERABLE)
            // attach a typed error and re-classify here in their landing
            // tasks (T027/T038/T057).
            std::process::exit(exit::IO);
        }
    }
}

fn exit_with(err: &ConfigError) -> ! {
    let code = match err {
        ConfigError::Io { .. } => exit::IO,
        ConfigError::Parse { .. } | ConfigError::MissingRequired { .. } => exit::CONFIG,
    };
    std::process::exit(code);
}
