use std::process::ExitCode;

use clap::Parser;

mod cli;
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

/// Dispatch to the chosen subcommand. Every command is a stub for now; real
/// behavior lands in STA-7 onward. Listing every variant (rather than `_`)
/// means adding a command later won't compile until it's wired in here.
fn run(cli: &Cli) -> Result<(), Error> {
    let command = &cli.command;
    match command {
        Command::Init
        | Command::Track
        | Command::Submit
        | Command::Sync
        | Command::Log
        | Command::Status => Err(Error::NotImplemented(command.name())),
    }
}

/// Render an error in the user's chosen output format.
fn report(err: Error, format: OutputFormat) {
    match format {
        // Machine-readable: a single JSON object on stdout.
        OutputFormat::Json => println!("{}", err.as_json()),
        // Human-readable: miette draws a graphical diagnostic on stderr.
        OutputFormat::Pretty => eprintln!("{:?}", miette::Report::new(err)),
    }
}
