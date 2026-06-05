//! Command-line interface definition.
//!
//! We use clap's *derive* API: the CLI is described as Rust structs and enums
//! annotated with attributes, and clap generates the parser, `--help`, and
//! validation from them.

use clap::{Parser, Subcommand, ValueEnum};

/// stacc: a stacked-diff CLI for AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "stacc", version, long_about = None)]
pub struct Cli {
    /// Flags shared by every subcommand.
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Flags accepted by every subcommand.
#[derive(Debug, clap::Args)]
pub struct GlobalArgs {
    /// Output format.
    #[arg(long, value_enum, default_value = "pretty", global = true)]
    pub format: OutputFormat,

    /// When to use colored output.
    #[arg(long, value_enum, default_value = "auto", global = true)]
    pub color: ColorChoice,

    /// Never prompt; fail with a structured error instead.
    #[arg(long, global = true)]
    pub no_interactive: bool,
}

/// How command output is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable, possibly colored.
    Pretty,
    /// Machine-readable JSON.
    Json,
}

/// When to colorize human-readable output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Color only when writing to a terminal.
    Auto,
    Always,
    Never,
}

/// The top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Detect trunk and remote, and initialize the state ref.
    Init(InitArgs),
    /// Track the current branch as part of a stack.
    Track(TrackArgs),
    /// Create a new branch stacked on the current one and track it.
    Create(CreateArgs),
    /// Fold staged changes into the current branch, then restack its upstack.
    Modify(ModifyArgs),
    /// Push branches and create or update PRs.
    Submit(SubmitArgs),
    /// Pull upstream changes, detect merges, and restack.
    Sync(SyncArgs),
    /// Rebase tracked branches back onto their bases (current + upstack by default).
    Restack(RestackArgs),
    /// Re-parent the current branch (and its upstack) onto a new base.
    Move(MoveArgs),
    /// Resume the operation interrupted by a conflict, after resolving it.
    Continue,
    /// Abort the operation interrupted by a conflict, undoing the in-progress rebase.
    Abort,
    /// Move up the stack toward the tip (optionally N levels).
    Up(StepsArgs),
    /// Move down the stack toward the trunk (optionally N levels).
    Down(StepsArgs),
    /// Jump to the top of the current stack.
    Top,
    /// Jump to the bottom of the current stack (the trunk's child).
    Bottom,
    /// Switch to a branch (pick interactively when run bare on a terminal).
    Checkout(CheckoutArgs),
    /// Print the stack.
    Log(LogArgs),
    /// Show the current branch's position and PR status.
    Status,
    /// Manage the GitHub access token.
    Auth(AuthArgs),

    /// Any other subcommand is proxied to `git` (e.g. `commit`, `rebase`, `push`).
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Arguments for `stacc auth`.
#[derive(Debug, clap::Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub action: AuthAction,
}

/// Sub-actions under `stacc auth`.
#[derive(Debug, Subcommand)]
pub enum AuthAction {
    /// Run OAuth device flow and store the token in the OS keychain.
    Login,
    /// Clear the stored token (env-var auth, if any, is untouched).
    Logout,
    /// Report which auth source is active and which user it identifies.
    Status,
}

/// Arguments for `stacc init`.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Override the detected trunk branch.
    #[arg(long)]
    pub trunk: Option<String>,
    /// Override the detected remote.
    #[arg(long)]
    pub remote: Option<String>,
}

/// Arguments for `stacc track`.
#[derive(Debug, clap::Args)]
pub struct TrackArgs {
    /// Branch this one is stacked on (defaults to the trunk).
    #[arg(long)]
    pub base: Option<String>,
}

/// Arguments for `stacc create`.
#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// Name of the new branch.
    pub name: String,
    /// Commit message for staged changes (defaults to the branch name).
    #[arg(long, short)]
    pub message: Option<String>,
}

/// Arguments for `stacc modify`.
#[derive(Debug, clap::Args)]
pub struct ModifyArgs {
    /// Append a new commit instead of amending the branch's tip.
    #[arg(long)]
    pub commit: bool,
    /// Commit message: the new commit's subject with `--commit`, or the reworded
    /// subject when amending.
    #[arg(long, short)]
    pub message: Option<String>,
}

/// Arguments for `stacc up` / `stacc down`: how many levels to move.
#[derive(Debug, clap::Args)]
pub struct StepsArgs {
    /// Number of levels to move (default 1).
    #[arg(default_value_t = 1)]
    pub steps: usize,
}

/// Arguments for `stacc log`.
#[derive(Debug, clap::Args)]
pub struct LogArgs {
    /// One compact line per branch instead of the indented graph.
    #[arg(long)]
    pub short: bool,
}

/// Arguments for `stacc checkout`.
#[derive(Debug, clap::Args)]
pub struct CheckoutArgs {
    /// Branch to switch to. Omit on a terminal to pick interactively.
    pub branch: Option<String>,
}

/// Arguments for `stacc submit`.
#[derive(Debug, clap::Args)]
pub struct SubmitArgs {
    /// PR body: a literal string, or `@path` to read from a file.
    /// Defaults to the branch's latest commit body.
    #[arg(long)]
    pub description: Option<String>,
}

/// Arguments for `stacc sync`.
#[derive(Debug, clap::Args)]
pub struct SyncArgs {
    /// Resume a sync that stopped on a conflict, after resolving it.
    #[arg(long = "continue")]
    pub continue_: bool,
    /// Skip the upstream fetch and restack on local refs only.
    #[arg(long)]
    pub offline: bool,
}

/// Arguments for `stacc restack`.
#[derive(Debug, clap::Args)]
pub struct RestackArgs {
    /// Restack the whole stack instead of just the current branch and its upstack.
    #[arg(long)]
    pub stack: bool,
}

/// Arguments for `stacc move`.
#[derive(Debug, clap::Args)]
pub struct MoveArgs {
    /// Branch (or the trunk) to re-parent the current branch onto.
    #[arg(long)]
    pub onto: String,
}

