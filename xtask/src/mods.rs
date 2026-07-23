pub(crate) mod cargo;
pub(crate) mod config;
pub(crate) mod git;
pub(crate) mod update;

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub(crate) struct ModsArgs {
    #[command(subcommand)]
    command: ModsCommand,
}

#[derive(Debug, Subcommand)]
enum ModsCommand {
    Update(update::UpdateArgs),
}

pub(crate) fn run(args: ModsArgs) -> Result<(), update::UpdateError> {
    match args.command {
        ModsCommand::Update(args) => update::run(args),
    }
}
