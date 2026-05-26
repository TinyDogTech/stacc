use std::process::ExitCode;

use clap::Parser;

mod cli;
mod commands;
mod error;

use cli::{Cli, Command, OutputFormat};
use error::Error;

fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report(err, cli.global.format);
            ExitCode::FAILURE
        }
    }
}

/// Dispatch to the chosen subcommand.
fn run(cli: &Cli) -> Result<(), Error> {
    match &cli.command {
        Command::Init(args) => commands::init(args, cli.global.format),
        Command::Track(args) => commands::track(args, cli.global.format),
        Command::Log => commands::log(cli.global.format),
        Command::Status => commands::status(cli.global.format),
        Command::Submit(args) => commands::submit(args, cli.global.format),
        Command::Sync => commands::sync(cli.global.format),
    }
}

/// Render an error in the user's chosen output format.
fn report(err: Error, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", err.as_json()),
        OutputFormat::Pretty => eprintln!("{:?}", miette::Report::new(err)),
    }
}
