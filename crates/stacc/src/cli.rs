//! Command-line interface definition.
//!
//! We use clap's *derive* API: the CLI is described as Rust structs and enums
//! annotated with attributes, and clap generates the parser, `--help`, and
//! validation from them.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// A stacked-diff CLI.
#[derive(Debug, Parser)]
#[command(name = "stacc", version = env!("STACC_VERSION"), long_about = None)]
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
    /// Output JSON instead of pretty-printed output.
    #[arg(long, global = true)]
    pub json: bool,

    /// When to use colored output.
    #[arg(long, value_enum, default_value = "auto", global = true)]
    pub color: ColorChoice,

    /// Never prompt; fail with a structured error instead.
    #[arg(long, short = 'i', global = true)]
    pub no_interactive: bool,

    /// Run as if stacc was started in DIR, analogous to `git -C`.
    #[arg(long, short = 'C', value_name = "DIR", global = true)]
    pub cwd: Option<PathBuf>,
}

impl GlobalArgs {
    pub fn output_format(&self) -> OutputFormat {
        if self.json {
            OutputFormat::Json
        } else {
            OutputFormat::Pretty
        }
    }

    pub fn work_dir(&self) -> PathBuf {
        self.cwd.clone().unwrap_or_else(|| PathBuf::from("."))
    }
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
///
/// Variants stay alphabetized (clap's derive API lists `--help` commands in
/// declaration order); `External` stays last as the catch-all.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Abort the operation interrupted by a conflict, undoing the in-progress rebase.
    Abort,
    /// Distribute staged hunks into the downstack commits that introduced their lines.
    Absorb(AbsorbArgs),
    /// Manage agent context files (install skill files for coding agents).
    Agent(AgentArgs),
    /// Manage the GitHub access token.
    Auth(AuthArgs),
    /// Jump to the bottom of the current stack (the trunk's child).
    Bottom,
    /// Switch to a branch (pick interactively when run bare on a terminal).
    Checkout(CheckoutArgs),
    /// Print the branches stacked directly on the current branch, name-ordered.
    Children,
    /// Print a tab-completion script for a shell to stdout.
    ///
    /// The script completes the `stacc` name. `st` users can reuse it, e.g.
    /// `complete -F _stacc st` (bash) or `compdef _stacc st` (zsh).
    Completion(CompletionArgs),
    /// Get and set stacc configuration (repo-local `.stacc.toml`, or the
    /// user-global file with `--global`).
    Config(ConfigArgs),
    /// Resume the operation interrupted by a conflict, after resolving it.
    Continue,
    /// Create a new branch stacked on the current one and track it.
    Create(CreateArgs),
    /// Delete a branch and its metadata, reparenting and restacking its children onto its base.
    Delete(DeleteArgs),
    /// Move down the stack toward the trunk (optionally N levels).
    Down(StepsArgs),
    /// Fold the current branch into its parent, reparenting and restacking its children.
    Fold(FoldArgs),
    /// Show a branch's stack detail: base, head, children, diffstat, and PR.
    Info(InfoArgs),
    /// Detect trunk and remote, and initialize the state ref.
    Init(InitArgs),
    /// Print the stack.
    Log(LogArgs),
    /// Squash-merge the ready PRs from the trunk up to the current branch, then sync.
    Merge(MergeArgs),
    /// Reconcile a branch already merged into trunk: drop it, re-parent its children, keep its tip.
    Merged(MergedArgs),
    /// Fold staged changes into the current branch, then restack its upstack.
    Modify(ModifyArgs),
    /// Re-parent the current branch (and its upstack) onto a new base.
    Move(MoveArgs),
    /// Print the current branch's recorded parent (base); nothing on the trunk.
    Parent,
    /// Remove the current branch, keeping its changes in the working tree as unstaged modifications.
    Pop,
    /// Print the current branch's PR URL (and open it in a browser on a terminal).
    Pr,
    /// Rename the current branch, updating local state, children, and the remote.
    Rename(RenameArgs),
    /// Reorder the branches between the trunk and the current branch, restacking descendants.
    Reorder(ReorderArgs),
    /// Rebase tracked branches back onto their bases (current + upstack by default).
    Restack(RestackArgs),
    /// Split the current branch into stacked branches, by commit or by file.
    Split(SplitArgs),
    /// Squash the current branch's commits into one, then restack its upstack.
    Squash(SquashArgs),
    /// Show the current branch's position and PR status.
    Status,
    /// Push branches and create or update PRs.
    Submit(SubmitArgs),
    /// Pull upstream changes, detect merges, and restack.
    Sync(SyncArgs),
    /// Jump to the top of the current stack.
    Top,
    /// Track the current branch as part of a stack.
    Track(TrackArgs),
    /// Revert the most recent stacc mutation(s), restoring prior state and tips.
    Undo(UndoArgs),
    /// Stop tracking a branch, reparenting its children onto its base.
    Untrack(UntrackArgs),
    /// Move up the stack toward the tip (optionally N levels).
    Up(StepsArgs),

    /// Any other subcommand is proxied to `git` (e.g. `commit`, `rebase`, `push`).
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Arguments for `stacc agent`.
#[derive(Debug, clap::Args)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub action: AgentAction,
}

/// Sub-actions under `stacc agent`.
#[derive(Debug, Subcommand)]
pub enum AgentAction {
    /// Install agent context files for one or more harnesses.
    Install(AgentInstallArgs),
}

/// Arguments for `stacc agent install`.
#[derive(Debug, clap::Args)]
pub struct AgentInstallArgs {
    /// Harness(es) to install context for. Repeat to combine. Omit for interactive checklist.
    #[arg(long, value_enum)]
    pub harness: Vec<AgentHarness>,
}

/// Agent harness targets for `stacc agent install`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum AgentHarness {
    /// Universal skill (~/.agents/skills/stacc/SKILL.md, covers all agentskills.io clients).
    Universal,
    /// Claude Code skill (~/.claude/skills/stacc/SKILL.md).
    #[value(name = "claude-skill")]
    ClaudeSkill,
    /// All of the above.
    All,
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

/// Arguments for `stacc config`.
#[derive(Debug, clap::Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// Sub-actions under `stacc config`. Keys: `trunk`, `remote`, `aliases.<name>`.
// An interactive TTY menu is a possible future convenience (KTD-6); the
// non-interactive get/set surface is the contract.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print a key's resolved value (repo overrides global overrides detected).
    Get {
        /// Key to read: `trunk`, `remote`, or `aliases.<name>`.
        key: String,
    },
    /// Set a key in the repo-local `.stacc.toml` (or the user config with `--global`).
    Set {
        /// Key to write: `trunk`, `remote`, or `aliases.<name>`.
        key: String,
        /// Value to store.
        value: String,
        /// Write the user-global config file instead of the repo-local one.
        #[arg(long)]
        global: bool,
    },
    /// Remove a key from the repo-local `.stacc.toml` (or the user config with `--global`).
    Unset {
        /// Key to remove: `trunk`, `remote`, or `aliases.<name>`.
        key: String,
        /// Edit the user-global config file instead of the repo-local one.
        #[arg(long)]
        global: bool,
    },
    /// List every known key with its resolved value and source.
    List,
}

/// Arguments for `stacc completion`.
#[derive(Debug, clap::Args)]
pub struct CompletionArgs {
    /// Shell to generate the script for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
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

/// Arguments for `stacc untrack`.
#[derive(Debug, clap::Args)]
pub struct UntrackArgs {
    /// Branch to untrack (defaults to the current branch).
    pub branch: Option<String>,
}

/// Arguments for `stacc create`.
#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// Name of the new branch.
    pub name: String,
    /// Commit message for staged changes (defaults to the branch name).
    #[arg(long, short)]
    pub message: Option<String>,
    /// Stage all changes, tracked and untracked (`git add -A`), before the
    /// branch-create commit. A path the same commit adds to `.gitignore` stops
    /// being tracked rather than getting swept in.
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Base the new branch on `<branch>` instead of the current branch.
    /// Mutually exclusive with `--insert`.
    #[arg(long, value_name = "BRANCH")]
    pub onto: Option<String>,
    /// Insert the new branch between the current branch and its children: the
    /// current branch's existing children are reparented onto the new branch
    /// and restacked. Mutually exclusive with `--onto`.
    #[arg(long)]
    pub insert: bool,
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
    /// Stage all changes, tracked and untracked (`git add -A`), before
    /// amending or committing. A path the same change adds to `.gitignore`
    /// stops being tracked rather than getting swept in.
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Apply the staged changes into the named downstack branch's tip instead
    /// of the current branch, replaying the commits above it and restacking.
    #[arg(long, value_name = "BRANCH")]
    pub into: Option<String>,
    /// Reword the tip's commit message only, with no content change. Never
    /// opens an editor: requires `--message` (an interactive editor is a
    /// possible future convenience) and refuses staged changes.
    #[arg(long)]
    pub edit: bool,
    /// Use only the staged changes under these paths (comma-separated; a path
    /// matches literally or as a directory prefix); the rest stay staged.
    /// Path-granular for now; hunk-granular selection is a planned follow-up.
    #[arg(long, value_name = "PATHS", value_delimiter = ',')]
    pub patch: Vec<String>,
}

/// Arguments for `stacc up` / `stacc down`: how many levels to move.
#[derive(Debug, clap::Args)]
pub struct StepsArgs {
    /// Number of levels to move (default 1).
    #[arg(default_value_t = 1)]
    pub steps: usize,
}

/// Which form `stacc log` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogForm {
    /// One row per branch: the graph without per-branch metadata.
    #[value(alias = "s")]
    Short,
    /// Git's own commit history for the stack (`git log --graph`).
    #[value(alias = "l")]
    Long,
}

/// Arguments for `stacc log`.
// Independent CLI flags are naturally booleans; the count is a surface, not a
// data-modeling smell.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct LogArgs {
    /// Form to render: omit for the full graph, `short` for one row per branch,
    /// `long` for git's commit history.
    #[arg(value_enum)]
    pub form: Option<LogForm>,

    /// Limit to the current branch's stack (its ancestors and descendants).
    #[arg(long)]
    pub stack: bool,

    /// Limit to N levels around the current branch (implies `--stack`).
    #[arg(long, value_name = "N")]
    pub steps: Option<usize>,

    /// Order the trunk on top instead of at the bottom.
    #[arg(long)]
    pub reverse: bool,

    /// Also list local branches stacc is not tracking.
    #[arg(long)]
    pub show_untracked: bool,

    /// Skip the live PR-status fetch (offline, faster).
    #[arg(long, alias = "offline")]
    pub no_status: bool,

    /// Deprecated alias for the `short` form.
    #[arg(long, hide = true)]
    pub short: bool,
}

impl LogArgs {
    /// The effective form, honoring the deprecated `--short` flag.
    pub fn form(&self) -> Option<LogForm> {
        self.form.or(if self.short {
            Some(LogForm::Short)
        } else {
            None
        })
    }

    /// Whether scoping to the current branch's stack was requested.
    pub fn scoped(&self) -> bool {
        self.stack || self.steps.is_some()
    }
}

/// Arguments for `stacc info`.
#[derive(Debug, clap::Args)]
pub struct InfoArgs {
    /// Branch to inspect (defaults to the current branch).
    pub branch: Option<String>,

    /// Include the branch's diff against its base.
    #[arg(long)]
    pub diff: bool,

    /// Include the branch's per-commit patches (like `git log -p`).
    #[arg(long)]
    pub patch: bool,

    /// Fetch the PR title, state, and body from GitHub (best-effort).
    #[arg(long)]
    pub body: bool,
}

/// Arguments for `stacc checkout`.
#[derive(Debug, clap::Args)]
pub struct CheckoutArgs {
    /// Branch to switch to. Omit on a terminal to pick interactively.
    pub branch: Option<String>,

    /// Check out the trunk branch directly (no picker).
    #[arg(long, group = "scope", conflicts_with = "branch")]
    pub trunk: bool,

    /// Limit the interactive picker to the current branch's stack.
    #[arg(long, group = "scope", conflicts_with = "branch")]
    pub stack: bool,

    /// Offer every tracked branch in the interactive picker (the default,
    /// made explicit).
    #[arg(long, group = "scope", conflicts_with = "branch")]
    pub all: bool,
}

/// Arguments for `stacc submit`.
#[derive(Debug, clap::Args)]
pub struct SubmitArgs {
    /// PR title for the current branch. Persisted in stack state so re-submits
    /// reuse it without repeating the flag. Defaults to the branch's commit
    /// subject when not set and no stored title exists.
    #[arg(long)]
    pub title: Option<String>,

    /// PR body: a literal string, or `@path` to read from a file.
    /// Persisted in stack state so re-submits reuse it without repeating the
    /// flag. Defaults to the branch's latest commit body.
    #[arg(long)]
    pub description: Option<String>,

    /// Submit every branch in the current stack (ancestors and descendants),
    /// not just the current branch and its downstack.
    #[arg(long)]
    pub stack: bool,

    /// Only update branches that already have a PR; skip (and report) the
    /// branches that would need a new one.
    #[arg(long)]
    pub update_only: bool,

    /// Create new PRs as drafts (existing PRs are updated, never re-drafted).
    #[arg(long)]
    pub draft: bool,
}

/// Arguments for `stacc sync`.
// Independent CLI flags are naturally booleans; the count is a surface, not a
// data-modeling smell (same as RestackArgs and LogArgs).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct SyncArgs {
    /// Resume a sync that stopped on a conflict, after resolving it.
    #[arg(long = "continue")]
    pub continue_: bool,
    /// Skip the upstream fetch and merged-PR detection; restack local refs only.
    #[arg(long)]
    pub offline: bool,
    /// Keep tracked branches whose git ref is gone instead of pruning them.
    #[arg(long)]
    pub no_prune: bool,
    /// Keep the local branches of merged PRs instead of deleting them.
    #[arg(long)]
    pub keep_branches: bool,
}

/// Arguments for `stacc restack`. The scope flags are mutually exclusive; with
/// none, the scope is the current branch plus its upstack.
// Independent CLI flags are naturally booleans; the count is a surface, not a
// data-modeling smell (same as LogArgs).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct RestackArgs {
    /// Restack the whole stack instead of just the current branch and its upstack.
    #[arg(long, group = "scope")]
    pub stack: bool,

    /// Restack only the current branch, leaving its upstack alone.
    #[arg(long, group = "scope")]
    pub only: bool,

    /// Restack the current branch and its ancestors (toward the trunk),
    /// leaving its upstack alone.
    #[arg(long, group = "scope")]
    pub downstack: bool,

    /// Restack the current branch and its upstack (the default, made explicit).
    #[arg(long, group = "scope")]
    pub upstack: bool,
}

#[derive(Debug, clap::Args)]
pub struct UndoArgs {
    /// How many versions back to revert (default 1).
    #[arg(long, default_value_t = 1)]
    pub steps: usize,
}

/// Arguments for `stacc move`.
#[derive(Debug, clap::Args)]
pub struct MoveArgs {
    /// Branch (or the trunk) to re-parent the current branch onto.
    #[arg(long)]
    pub onto: String,

    /// Move only the current branch, not its upstack: its children are
    /// reparented onto its old base so they stay put.
    #[arg(long)]
    pub only: bool,
}

/// Arguments for `stacc absorb`.
#[derive(Debug, clap::Args)]
pub struct AbsorbArgs {
    /// Compute and print the hunk-to-commit mapping without mutating anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for `stacc squash`.
#[derive(Debug, clap::Args)]
pub struct SquashArgs {
    /// Message for the squashed commit (defaults to the squashed commits'
    /// subjects and bodies, concatenated oldest-first).
    #[arg(long, short)]
    pub message: Option<String>,
}

/// Arguments for `stacc fold`.
#[derive(Debug, clap::Args)]
pub struct FoldArgs {
    /// Close the folded branch's PR on GitHub (best-effort; a failure to close
    /// is reported, not fatal).
    #[arg(long)]
    pub close: bool,
}

/// Arguments for `stacc delete`.
#[derive(Debug, clap::Args)]
pub struct DeleteArgs {
    /// Branch to delete (must be tracked; never the trunk).
    pub branch: String,
    /// Delete even when the branch is not merged into its base, its diff is
    /// not empty, and its PR is not closed or merged.
    #[arg(long)]
    pub force: bool,
    /// Close the branch's PR on GitHub (left open by default; best-effort, a
    /// failure to close is reported, not fatal).
    #[arg(long)]
    pub close: bool,
}

/// Arguments for `stacc merged`.
#[derive(Debug, clap::Args)]
pub struct MergedArgs {
    /// Branch to reconcile (must be tracked; never the trunk).
    pub branch: String,
    /// Drop the branch even when stacc cannot prove it merged (its diff only
    /// looks already on trunk, or no signal at all). Acts only on this one
    /// named branch, never a flagged set.
    #[arg(long = "assume-merged")]
    pub assume_merged: bool,
}

/// Arguments for `stacc split`.
#[derive(Debug, clap::Args)]
pub struct SplitArgs {
    /// By-commit mode: names for the new branches, one per commit except the
    /// tip, oldest commit first (the tip keeps the current branch's name). A
    /// branch with N own commits takes exactly N-1 names.
    pub names: Vec<String>,

    /// By-file mode: a `<pathspec>=<branch-name>` group (repeatable). Every
    /// changed path must match a group: a path matches by literal equality or
    /// directory prefix (`src` matches `src/a.rs`), first group wins. Mutually
    /// exclusive with positional names.
    #[arg(long = "by-file", value_name = "PATHSPEC=NAME")]
    pub by_file: Vec<String>,
}

/// Arguments for `stacc reorder`.
#[derive(Debug, clap::Args)]
pub struct ReorderArgs {
    /// The downstack branches in their new bottom-up order, comma-separated
    /// (the first name sits on the trunk). Must name exactly the branches
    /// between the trunk and the current branch, each once.
    #[arg(long, value_name = "B1,B2,...")]
    pub order: Option<String>,
}

/// Arguments for `stacc merge`.
// Independent CLI flags are naturally booleans; the count is a surface, not a
// data-modeling smell (same as RestackArgs, LogArgs, and SyncArgs).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct MergeArgs {
    /// Skip the post-merge fetch: merged branches still leave local state, but the
    /// stack is not rebased onto the merged commits until a later `stacc sync`.
    #[arg(long)]
    pub offline: bool,
    /// Refuse to merge unless the trunk has branch protection enabled.
    #[arg(long)]
    pub require_protected: bool,
    /// Keep the local branches of merged PRs instead of deleting them.
    #[arg(long)]
    pub keep_branches: bool,
    /// Wait on a child that stops only because its CI is still running: poll its
    /// checks and continue the merge when they pass, instead of stopping for a
    /// manual re-run. A hard block (conflict, review) still stops immediately.
    #[arg(long)]
    pub watch: bool,
    /// Seconds between CI polls while `--watch` waits.
    #[arg(long, value_name = "SECS", default_value_t = 15)]
    pub watch_interval: u64,
    /// Give up `--watch` after this many seconds of still-pending CI, leaving the
    /// retryable stop to report.
    #[arg(long, value_name = "SECS", default_value_t = 600)]
    pub watch_timeout: u64,
}

/// Arguments for `stacc rename`.
#[derive(Debug, clap::Args)]
pub struct RenameArgs {
    /// New name for the current branch.
    pub name: String,
    /// Rename even when it will close the branch's own open PR on the remote.
    #[arg(long)]
    pub force: bool,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::{Cli, GlobalArgs, OutputFormat};

    #[test]
    fn output_format_json_true() {
        let args = GlobalArgs {
            json: true,
            color: super::ColorChoice::Auto,
            no_interactive: false,
            cwd: None,
        };
        assert_eq!(args.output_format(), OutputFormat::Json);
    }

    #[test]
    fn output_format_json_false() {
        let args = GlobalArgs {
            json: false,
            color: super::ColorChoice::Auto,
            no_interactive: false,
            cwd: None,
        };
        assert_eq!(args.output_format(), OutputFormat::Pretty);
    }

    #[test]
    fn work_dir_defaults_to_dot() {
        let args = GlobalArgs {
            json: false,
            color: super::ColorChoice::Auto,
            no_interactive: false,
            cwd: None,
        };
        assert_eq!(args.work_dir(), std::path::PathBuf::from("."));
    }

    #[test]
    fn work_dir_returns_cwd_when_set() {
        let args = GlobalArgs {
            json: false,
            color: super::ColorChoice::Auto,
            no_interactive: false,
            cwd: Some(std::path::PathBuf::from("/tmp/repo")),
        };
        assert_eq!(args.work_dir(), std::path::PathBuf::from("/tmp/repo"));
    }

    #[test]
    fn help_lists_commands_alphabetically() {
        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            names, sorted,
            "`--help` shows declaration order; keep Command variants alphabetized"
        );
    }
}

