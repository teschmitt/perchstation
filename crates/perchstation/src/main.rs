#![deny(unsafe_code)]

mod cli;
mod commands;

fn main() -> anyhow::Result<()> {
    // Real CLI dispatch lands in T015.
    eprintln!("perchstation: CLI dispatch not yet wired up (T015)");
    std::process::exit(64);
}
