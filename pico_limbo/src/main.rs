#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
mod cli;
mod configuration;
// `forge` is wired up incrementally across multiple PRs (see
// FORGE_PROTOCOL_DESIGN.md). Mirror the suppression applied in lib.rs:
// data structures and persistence layer are exercised from
// start_server and the status handler; the recorder / replay state
// machines arrive in later steps.
#[allow(dead_code)]
mod forge;
mod forwarding;
mod handlers;
mod kick_messages;
mod server;
mod server_brand;
mod server_state;

use crate::cli::Cli;
use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    server::start_server::start_server(cli.config_path, cli.verbose, None).await
}
