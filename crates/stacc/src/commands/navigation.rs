//! Stack navigation: `up` / `down` / `top` / `bottom` move HEAD around the
//! tracked stack via `git checkout`.

use serde_json::json;
use stacc_core::ops;
use stacc_git::Git;
use stacc_state::{State, StateStore};

use crate::cli::{OutputFormat, StepsArgs};
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
            println!("{}", json!({ "op": op, "branch": to, "moved": moved }));
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
    let target = ops::top(&state.branches, &current);
    let kids = ops::children(&state.branches, &target);
    if kids.len() > 1 {
        return Err(Error::Ambiguous { choices: kids });
    }
    go(&git, format, "top", &current, &target)
}

/// `stacc bottom`: jump to the bottom of the current stack (the trunk's child).
pub fn bottom(format: OutputFormat) -> Result<(), Error> {
    let (git, current, state, trunk) = context()?;
    let target = ops::bottom(&state.branches, &current, &trunk);
    go(&git, format, "bottom", &current, &target)
}
