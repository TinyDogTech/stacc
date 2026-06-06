//! stacc, a stacked-diff CLI.
//!
//! The CLI logic lives in this library so the `stacc` and `st` binaries can
//! both be thin wrappers around [`run`].

use std::collections::{BTreeMap, HashSet};
use std::process::ExitCode;

use clap::Parser;

mod cli;
mod commands;
mod error;
mod interactive;

use cli::{Cli, Command, OutputFormat};
use error::Error;

/// Names that stacc always handles itself. These always shadow any user alias.
const BUILTINS: &[&str] = &[
    "init", "track", "create", "modify", "log", "status", "submit", "sync", "restack", "move",
    "rename", "merge", "continue", "abort", "up", "down", "top", "bottom", "checkout", "pr",
    "auth",
];

/// Short aliases stacc ships with, seeded at the lowest precedence so a user or
/// repo alias of the same name overrides them. These are aliases (not in
/// [`BUILTINS`]), so they expand to the real command name.
const DEFAULT_ALIASES: &[(&str, &str)] = &[
    ("co", "checkout"),
    ("u", "up"),
    ("d", "down"),
    ("l", "log"),
    ("st", "status"),
];

/// Parse the command line, dispatch, and return the process exit code.
pub fn run() -> ExitCode {
    // Aliases load best-effort, lowest precedence first: built-in defaults, then
    // user-global, then repo-local (repo wins).
    let mut aliases: BTreeMap<String, String> = DEFAULT_ALIASES
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    aliases.extend(stacc_config::aliases_from_file(&stacc_config::user_config_path()));
    aliases.extend(stacc_config::aliases_from_file(std::path::Path::new(
        ".stacc.toml",
    )));

    // Rewrite argv through the alias table before clap sees any of it.
    let raw: Vec<String> = std::env::args().collect();
    let args = match expand_aliases(raw, &aliases) {
        Ok(a) => a,
        Err(err) => {
            eprintln!("stacc: {err}");
            return ExitCode::FAILURE;
        }
    };

    let cli = Cli::parse_from(args);

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
        Command::Create(args) => commands::create(args, cli.global.format),
        Command::Modify(args) => commands::modify(args, cli.global.format),
        Command::Log(args) => commands::log(args, cli.global.format),
        Command::Status => commands::status(cli.global.format),
        Command::Pr => commands::pr(cli.global.format),
        Command::Submit(args) => commands::submit(args, cli.global.format),
        Command::Sync(args) => commands::sync(args, cli.global.format),
        Command::Restack(args) => commands::restack(args, cli.global.format),
        Command::Move(args) => commands::move_cmd(args, cli.global.format),
        Command::Rename(args) => commands::rename(args, cli.global.format),
        Command::Merge(args) => commands::merge(args, cli.global.format),
        Command::Continue => commands::continue_cmd(cli.global.format),
        Command::Abort => commands::abort_cmd(cli.global.format),
        Command::Up(args) => commands::up(args, cli.global.format),
        Command::Down(args) => commands::down(args, cli.global.format),
        Command::Top => commands::top(cli.global.format),
        Command::Bottom => commands::bottom(cli.global.format),
        Command::Checkout(args) => {
            commands::checkout(args, cli.global.format, cli.global.no_interactive)
        }
        Command::Auth(args) => commands::auth(args, cli.global.format),
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
        Ok(status) => ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1)),
        Err(err) => {
            eprintln!("stacc: failed to run git: {err}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug)]
enum AliasError {
    Cycle(String),
    BadTokens(String),
}

impl std::fmt::Display for AliasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AliasError::Cycle(name) => {
                write!(f, "alias cycle: `{name}` expands back to itself")
            }
            AliasError::BadTokens(name) => {
                write!(f, "alias `{name}` has invalid shell syntax")
            }
        }
    }
}

/// Expand `args[1]` through the alias table, repeatedly, until it's a built-in
/// stacc command, an unknown name (clap's external_subcommand will route it to
/// git), or empty.
fn expand_aliases(
    mut args: Vec<String>,
    aliases: &BTreeMap<String, String>,
) -> Result<Vec<String>, AliasError> {
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        if args.len() < 2 {
            return Ok(args);
        }
        let cmd = args[1].clone();
        // Built-ins win, never expanded.
        if BUILTINS.contains(&cmd.as_str()) {
            return Ok(args);
        }
        // Not an alias? Leave it; clap's external_subcommand passes it to git.
        let Some(expansion) = aliases.get(&cmd) else {
            return Ok(args);
        };
        if !seen.insert(cmd.clone()) {
            return Err(AliasError::Cycle(cmd));
        }
        let tokens =
            shlex::split(expansion).ok_or_else(|| AliasError::BadTokens(cmd.clone()))?;

        // Replace argv[1] with the expansion, keeping argv[0] and any trailing
        // arguments the user typed after the alias.
        let argv0 = args.remove(0);
        args.remove(0); // the alias name
        let mut next = Vec::with_capacity(1 + tokens.len() + args.len());
        next.push(argv0);
        next.extend(tokens);
        next.append(&mut args);
        args = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn no_alias_passes_through() {
        let a = aliases(&[]);
        let out = expand_aliases(argv(&["stacc", "ci", "-m", "x"]), &a).unwrap();
        assert_eq!(out, argv(&["stacc", "ci", "-m", "x"]));
    }

    #[test]
    fn simple_alias_expands() {
        let a = aliases(&[("st", "status")]);
        let out = expand_aliases(argv(&["stacc", "st", "--format", "json"]), &a).unwrap();
        assert_eq!(out, argv(&["stacc", "status", "--format", "json"]));
    }

    #[test]
    fn multi_token_alias_with_quotes_expands_via_shlex() {
        let a = aliases(&[("body", "submit --description \"my body\"")]);
        let out = expand_aliases(argv(&["stacc", "body"]), &a).unwrap();
        assert_eq!(
            out,
            argv(&["stacc", "submit", "--description", "my body"])
        );
    }

    #[test]
    fn builtin_shadows_alias() {
        // A user trying to override `status` is silently ignored.
        let a = aliases(&[("status", "log")]);
        let out = expand_aliases(argv(&["stacc", "status"]), &a).unwrap();
        assert_eq!(out, argv(&["stacc", "status"]));
    }

    #[test]
    fn recursive_alias_resolves() {
        let a = aliases(&[("a", "b"), ("b", "status")]);
        let out = expand_aliases(argv(&["stacc", "a"]), &a).unwrap();
        assert_eq!(out, argv(&["stacc", "status"]));
    }

    #[test]
    fn cyclic_alias_errors() {
        let a = aliases(&[("a", "b"), ("b", "a")]);
        let err = expand_aliases(argv(&["stacc", "a"]), &a).unwrap_err();
        assert!(matches!(err, AliasError::Cycle(_)));
    }

    #[test]
    fn no_args_after_program_is_ok() {
        let a = aliases(&[("st", "status")]);
        let out = expand_aliases(argv(&["stacc"]), &a).unwrap();
        assert_eq!(out, argv(&["stacc"]));
    }

    /// The alias table as `run` seeds it: built-in defaults plus any overrides.
    fn seeded(extra: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut a: BTreeMap<String, String> = DEFAULT_ALIASES
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        for (k, v) in extra {
            a.insert((*k).to_string(), (*v).to_string());
        }
        a
    }

    #[test]
    fn builtin_aliases_expand_to_real_commands() {
        let a = seeded(&[]);
        assert_eq!(
            expand_aliases(argv(&["stacc", "co", "main"]), &a).unwrap(),
            argv(&["stacc", "checkout", "main"])
        );
        assert_eq!(
            expand_aliases(argv(&["stacc", "u"]), &a).unwrap(),
            argv(&["stacc", "up"])
        );
    }

    #[test]
    fn user_alias_overrides_a_builtin_alias() {
        // A user `co` shadows the shipped `co` -> checkout.
        let a = seeded(&[("co", "create")]);
        assert_eq!(
            expand_aliases(argv(&["stacc", "co", "x"]), &a).unwrap(),
            argv(&["stacc", "create", "x"])
        );
    }

    #[test]
    fn builtin_aliases_never_collide_with_a_command_name() {
        // None of the shipped aliases are real command names, so they never
        // shadow a builtin (which would stop expansion before reaching git).
        for (alias, _) in DEFAULT_ALIASES {
            assert!(
                !BUILTINS.contains(alias),
                "alias `{alias}` collides with a builtin command"
            );
        }
    }
}
