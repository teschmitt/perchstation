//! `perchstation status` — prints a delivery-health snapshot and exits.
//!
//! Pure read against `data_dir`; safe to run alongside `serve`. Behaviour is
//! described in `specs/001-clip-delivery/contracts/cli.md` §`perchstation
//! status`. The snapshot itself is computed by
//! [`perchstation_core::observability::status::snapshot`] (T056).

use anyhow::anyhow;
use chrono::Utc;
use perchstation_core::config::Config;
use perchstation_core::observability::status;

use crate::cli::StatusArgs;
use crate::commands::CommandError;

#[allow(
    clippy::unused_async,
    reason = "uniform subcommand signature with `enroll` and `serve`, both of which need async"
)]
pub async fn run(args: StatusArgs, config: &Config) -> Result<(), CommandError> {
    let snapshot = status::snapshot(&config.data_dir, Utc::now())
        .map_err(|err| CommandError::Io(anyhow!("status snapshot failed: {err}")))?;

    let rendered = if args.json {
        serde_json::to_string_pretty(&snapshot).map_err(|err| {
            CommandError::Io(anyhow!("could not serialise status snapshot: {err}"))
        })?
    } else {
        snapshot.render_text()
    };

    println!("{rendered}");
    Ok(())
}
