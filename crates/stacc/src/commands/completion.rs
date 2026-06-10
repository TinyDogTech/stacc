//! `stacc completion`: emit a shell tab-completion script.

use clap::CommandFactory;

use crate::cli::{Cli, CompletionArgs};

/// `stacc completion`: write the completion script for the chosen shell to
/// stdout. Pure output: no state, no git. The script itself IS the output, so
/// `--format json` is deliberately ignored rather than wrapping the script.
///
/// The script is generated for the `stacc` binary name; `st` users can extend
/// it to the alias (see the subcommand help). Dynamic completion of tracked
/// branch names is a possible follow-up once clap_complete's dynamic API
/// stabilizes.
pub fn completion(args: &CompletionArgs) {
    let mut cmd = Cli::command();
    clap_complete::generate(args.shell, &mut cmd, "stacc", &mut std::io::stdout());
}
