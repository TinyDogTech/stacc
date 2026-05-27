//! stacc — a stacked-diff CLI for AI coding agents.
//!
//! The CLI logic lives in this library so the `stacc` and `st` binaries can
//! both be thin wrappers around [`run`].

use std::process::ExitCode;

use clap::Parser;

mod cli;
mod commands;
mod error;

use cli::{Cli, Command, OutputFormat};
use error::Error;

/// Parse the command line, dispatch, and return the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    // Unknown subcommands are proxied straight to git.
    if let Command::External(args) = &cli.command {
        return proxy_to_git(args);
    }

    match dispatch(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report(err, cli.global.format);
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: &Cli) -> Result<(), Error> {
    match &cli.command {
        Command::Init(args) => commands::init(args, cli.global.format),
        Command::Track(args) => commands::track(args, cli.global.format),
        Command::Log => commands::log(cli.global.format),
        Command::Status => commands::status(cli.global.format),
        Command::Submit(args) => commands::submit(args, cli.global.format),
        Command::Sync(args) => commands::sync(args, cli.global.format),
        Command::External(_) => unreachable!("external subcommands are proxied in run"),
    }
}

fn report(err: Error, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", err.as_json()),
        OutputFormat::Pretty => eprintln!("{:?}", miette::Report::new(err)),
    }
}

/// Run `git <args>`, inheriting this process's stdio, and return git's exit
/// code. Deliberately *not* stacc's git wrapper: the editor and credential
/// prompts should behave exactly as if `git` were invoked directly.
fn proxy_to_git(args: &[String]) -> ExitCode {
    match std::process::Command::new("git").args(args).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(err) => {
            eprintln!("stacc: failed to run git: {err}");
            ExitCode::FAILURE
        }
    }
}
