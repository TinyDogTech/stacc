//! Stack navigation: `up` / `down` / `top` / `bottom` move HEAD around the
//! tracked stack via `git checkout`, and the read-only `parent` / `children`
//! report the current branch's neighbors without moving.

use std::io::IsTerminal;

use serde_json::json;
use stacc_core::ops;
use stacc_forge::SCHEMA_VERSION;
use stacc_git::Git;
use stacc_state::{State, StateStore};

use crate::cli::{CheckoutArgs, OutputFormat, StepsArgs};
use crate::error::Error;

/// Load state, the trunk, and the current branch, refusing on an uninitialized
/// repo or a detached HEAD.
fn context() -> Result<(Git, String, State, String), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let trunk = state
        .repo
        .as_ref()
        .map(|r| r.trunk.clone())
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;
    let current = git.current_branch().map_err(|_| {
        Error::Usage("cannot navigate from a detached HEAD; check out a branch first".into())
    })?;
    Ok((git, current, state, trunk))
}

/// Check out `to` (unless already there) and report the move.
fn go(git: &Git, format: OutputFormat, op: &str, from: &str, to: &str) -> Result<(), Error> {
    let moved = to != from;
    if moved {
        git.checkout(to)?;
    }
    match format {
        OutputFormat::Json => {
            println!("{}", json!({ "op": op, "branch": to, "moved": moved, "schema_version": SCHEMA_VERSION }));
        }
        OutputFormat::Pretty => {
            if moved {
                println!("Switched to {to}.");
            } else {
                println!("Already at {from}.");
            }
        }
    }
    Ok(())
}

/// `stacc up`: move toward the tip, `args.steps` levels (default 1). Errors with
/// the choices when a level forks into multiple children.
pub fn up(args: &StepsArgs, format: OutputFormat) -> Result<(), Error> {
    let (git, current, state, _trunk) = context()?;
    let mut target = current.clone();
    for _ in 0..args.steps {
        let kids = ops::children(&state.branches, &target);
        match kids.len() {
            0 => break,
            1 => target = kids.into_iter().next().expect("one child"),
            _ => return Err(Error::Ambiguous { choices: kids }),
        }
    }
    go(&git, format, "up", &current, &target)
}

/// `stacc down`: move toward the trunk, `args.steps` levels (default 1). Clamps
/// at the trunk.
pub fn down(args: &StepsArgs, format: OutputFormat) -> Result<(), Error> {
    let (git, current, state, trunk) = context()?;
    let mut target = current.clone();
    for _ in 0..args.steps {
        match ops::parent(&state.branches, &target) {
            Some(base) => target = base,
            None => break,
        }
        if target == trunk {
            break;
        }
    }
    go(&git, format, "down", &current, &target)
}

/// `stacc top`: jump to the tip of the current stack (errors at a fork).
pub fn top(format: OutputFormat) -> Result<(), Error> {
    let (git, current, state, _trunk) = context()?;
    let target =
        ops::top(&state.branches, &current).map_err(|choices| Error::Ambiguous { choices })?;
    go(&git, format, "top", &current, &target)
}

/// `stacc bottom`: jump to the bottom of the current stack (the trunk's child).
pub fn bottom(format: OutputFormat) -> Result<(), Error> {
    let (git, current, state, trunk) = context()?;
    let target = ops::bottom(&state.branches, &current, &trunk);
    go(&git, format, "bottom", &current, &target)
}

/// `stacc parent`: print the current branch's recorded base. Read-only. On the
/// trunk or an untracked branch the parent is null and the exit code is 0, so
/// scripts walking down a stack do not break at the root.
pub fn parent(format: OutputFormat) -> Result<(), Error> {
    let (_git, current, state, _trunk) = context()?;
    let parent = ops::parent(&state.branches, &current);
    match format {
        OutputFormat::Json => {
            println!("{}", json!({ "op": "parent", "parent": parent, "schema_version": SCHEMA_VERSION }));
        }
        OutputFormat::Pretty => {
            if let Some(parent) = parent {
                println!("{parent}");
            }
        }
    }
    Ok(())
}

/// `stacc children`: print the branches stacked directly on the current branch
/// (recorded base == current), in name order. Read-only. On the trunk this
/// lists the trunk-based branches; a leaf prints an empty list, exit 0.
pub fn children(format: OutputFormat) -> Result<(), Error> {
    let (_git, current, state, _trunk) = context()?;
    let kids = ops::children(&state.branches, &current);
    match format {
        OutputFormat::Json => {
            println!("{}", json!({ "op": "children", "children": kids, "schema_version": SCHEMA_VERSION }));
        }
        OutputFormat::Pretty => {
            for kid in &kids {
                println!("{kid}");
            }
        }
    }
    Ok(())
}

/// `stacc checkout`: switch to `args.branch`, or pick one interactively when run
/// bare on a terminal. `--trunk` checks out the trunk directly (no picker);
/// `--stack`/`--all` scope the picker's candidates (the current branch's stack /
/// every tracked branch, the latter being the default made explicit). Bare +
/// non-interactive errors structured (never prompts), flags or not.
pub fn checkout(
    args: &CheckoutArgs,
    format: OutputFormat,
    no_interactive: bool,
) -> Result<(), Error> {
    let git = Git::open(".");
    let current = git.current_branch().unwrap_or_default();
    if let Some(branch) = &args.branch {
        // A leading dash would be parsed by `git checkout` as an option.
        if branch.starts_with('-') {
            return Err(Error::Usage(format!("`{branch}` is not a valid branch name")));
        }
        return go(&git, format, "checkout", &current, branch);
    }
    // --trunk is deterministic: no picker, so no TTY needed.
    if args.trunk {
        let store = StateStore::new(git.clone());
        let state = store.load()?;
        let trunk = state
            .repo
            .as_ref()
            .map(|r| r.trunk.clone())
            .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;
        return go(&git, format, "checkout", &current, &trunk);
    }
    if !crate::interactive::allowed(std::io::stdin().is_terminal(), no_interactive, format) {
        return Err(Error::Usage(
            "`stacc checkout` needs a branch name when not interactive; pass one explicitly".into(),
        ));
    }
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let items: Vec<String> = if args.stack {
        // The current branch's stack: its ancestors (to the trunk) and its
        // descendants, bottom-up.
        let trunk = state
            .repo
            .as_ref()
            .map(|r| r.trunk.clone())
            .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;
        if current == trunk || !state.branches.contains_key(&current) {
            return Err(Error::Usage(
                "`--stack` scopes the picker to the current branch's stack; check out a tracked stack branch first".into(),
            ));
        }
        let mut items = ops::downstack_chain(&state, &current, &trunk)?;
        items.extend(
            ops::upstack_order(&state.branches, &current)
                .into_iter()
                .skip(1),
        );
        items
    } else {
        // The default candidate set (--all is its explicit spelling): the trunk
        // plus every tracked branch.
        let mut items: Vec<String> = Vec::new();
        if let Some(repo) = &state.repo {
            items.push(repo.trunk.clone());
        }
        items.extend(state.branches.keys().cloned());
        items
    };
    if items.is_empty() {
        return Err(Error::Usage("no branches to choose from".into()));
    }
    let choice = crate::interactive::prompt_select("Check out which branch?", &items)?;
    go(&git, format, "checkout", &current, &choice)
}
