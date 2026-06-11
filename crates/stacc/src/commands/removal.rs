//! Branch removal: `stacc delete` and `stacc pop` (KTD-5).
//!
//! Both commands share one removal core: drop the branch from state and
//! reparent its children onto its base in ONE transactional update, then
//! restack the children's subtrees through the recovery-aware engine. The
//! immediate children are FORCED through the rebase: they descend the base's
//! live tip through the removed branch's commits, so the engine's plain
//! already-on-base skip would leave those commits in their history; the forced
//! replay of exactly `base.hash..child` drops them (the same flatten reasoning
//! as `reorder`).
//!
//! `delete` additionally applies a safety predicate (graphite's
//! `isSafeToDelete`) and deletes the git ref; its PR is left open by default,
//! `--close` closes it best-effort. `pop` keeps the branch's changes in the
//! working tree via `git reset --mixed <base>`, ordering the children's
//! restack BEFORE the reset so they rebase while the branch's commits still
//! exist. `untrack`'s reparent is the same state delta minus the restack and
//! ref surgery; folding it onto this core is a follow-up, not this unit.

use std::collections::BTreeSet;

use serde_json::json;
use stacc_core::{ops, recovery};
use stacc_git::Git;
use stacc_github::{GitHub, PrState};
use stacc_state::{RepoConfig, State, StateStore};

use super::operations::{
    clear_conflict_artifacts, close_pr_best_effort, guard_worktree, restack_with_recovery_forced,
};
use crate::cli::{DeleteArgs, OutputFormat};
use crate::error::Error;

/// `stacc delete`: delete a tracked branch and its metadata, reparenting its
/// children onto its base and restacking them. Refuses an unsafe delete (see
/// [`ensure_safe_to_delete`]) without `--force`. The branch's PR is left open
/// by default; `--close` closes it best-effort.
pub fn delete(args: &DeleteArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    if git.rebase_in_progress() {
        return Err(Error::Usage(
            "a rebase is already in progress; run `stacc continue` to resume it or `stacc abort` to undo".into(),
        ));
    }

    let branch = args.branch.clone();
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot delete the trunk branch `{}`",
            repo.trunk
        )));
    }
    let branch_state = state.branches.get(&branch).cloned().ok_or_else(|| {
        Error::Usage(format!(
            "branch `{branch}` is not tracked; stacc only deletes branches it manages, use `git branch -D {branch}` for an untracked one"
        ))
    })?;
    let base = branch_state.base.name.clone();

    let tip = git.rev_parse(&branch)?;
    if !args.force {
        ensure_safe_to_delete(
            &git,
            &repo,
            &branch,
            &base,
            &tip,
            branch_state.pr.as_ref().map(|pr| pr.number),
        )?;
    }

    // Fail fast if the branch or any upstack child is checked out in another
    // worktree, BEFORE mutating anything: deleting the ref or rebasing a child
    // would desync that worktree.
    guard_worktree(&git, &ops::upstack_order(&state.branches, &branch))?;

    // Deleting the checked-out branch: move to the trunk first (mirroring how
    // fold moves to the parent), so HEAD never names a deleted ref.
    if git.current_branch().ok().as_deref() == Some(branch.as_str()) {
        git.checkout(&repo.trunk).map_err(|e| {
            Error::Usage(format!(
                "`{branch}` is checked out; switching to the trunk `{}` before deleting it failed ({e})",
                repo.trunk
            ))
        })?;
    }
    let here = git.current_branch().ok();

    let (reparented, restacked) =
        match remove_and_restack_children(&git, &store, &mut state, &repo, &branch, &base) {
            Ok(outcome) => outcome,
            Err(err @ Error::Conflict { .. }) => {
                eprintln!(
                    "note: `{branch}` is already removed from the stack; after resolving and `stacc continue`, delete its git ref with `git branch -D {branch}`"
                );
                return Err(err);
            }
            Err(err) => return Err(err),
        };

    // Best-effort: the children's restack leaves HEAD on the last branch it
    // rebased; put the user back where they started.
    if let Some(here) = &here {
        if git.current_branch().ok().as_deref() != Some(here.as_str()) {
            if let Err(err) = git.checkout(here) {
                eprintln!("warning: could not switch back to `{here}`: {err}");
            }
        }
    }

    delete_branch_ref(&git, &branch);

    let pr_closed = if args.close {
        close_pr_best_effort(&git, &repo, &branch, branch_state.pr.as_ref().map(|pr| pr.number))
    } else {
        None
    };

    report_delete(format, &branch, &base, &reparented, &restacked, pr_closed);
    Ok(())
}

/// `stacc pop`: remove the current branch but keep its changes in the working
/// tree as unstaged modifications, reparenting and restacking its children
/// onto its base. Refuses a dirty working tree: the popped diff must land in a
/// clean one or it would collide with the existing uncommitted changes.
///
/// The order is load-bearing: (1) drop + reparent in one state write, (2)
/// restack the children while the branch's commits still exist, (3) mixed-reset
/// the branch (and HEAD) to the base's live tip so the diff surfaces as
/// unstaged modifications, (4) check out the base (same commit, so the dirty
/// tree carries over untouched), (5) delete the now fully-merged ref.
pub fn pop(format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    if git.rebase_in_progress() {
        return Err(Error::Usage(
            "a rebase is already in progress; run `stacc continue` to resume it or `stacc abort` to undo".into(),
        ));
    }

    let branch = git.current_branch().map_err(|_| {
        Error::Usage("cannot pop a detached HEAD; check out a branch first".into())
    })?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot pop the trunk branch `{}`",
            repo.trunk
        )));
    }
    let base = state
        .branches
        .get(&branch)
        .map(|b| b.base.name.clone())
        .ok_or_else(|| {
            Error::Usage(format!(
                "branch `{branch}` is not tracked; run `stacc track` first"
            ))
        })?;

    if git.has_uncommitted_changes()? {
        return Err(Error::Usage(
            "the working tree has uncommitted changes; commit or stash them, then pop".into(),
        ));
    }

    // Fail fast if any upstack child is checked out in another worktree,
    // BEFORE mutating anything (the popped branch itself is checked out HERE).
    guard_worktree(&git, &ops::upstack_order(&state.branches, &branch))?;

    let (reparented, restacked) =
        match remove_and_restack_children(&git, &store, &mut state, &repo, &branch, &base) {
            Ok(outcome) => outcome,
            Err(err @ Error::Conflict { .. }) => {
                eprintln!(
                    "note: `{branch}` is already removed from the stack but its commits are untouched; after resolving and `stacc continue`, finish the pop by hand: `git checkout {branch} && git reset --mixed {base} && git checkout {base} && git branch -D {branch}`"
                );
                return Err(err);
            }
            Err(err) => return Err(err),
        };

    // The children's restack leaves HEAD on the last branch it rebased; the
    // reset below moves the CURRENT branch's ref, so get back onto the popped
    // branch first. Fatal on failure: the diff has not landed yet, and the
    // recovery steps keep every commit reachable.
    if git.current_branch().ok().as_deref() != Some(branch.as_str()) {
        git.checkout(&branch).map_err(|e| {
            Error::Usage(format!(
                "`{branch}` is removed from the stack but switching back to it failed ({e}); its commits are intact, finish by hand: `git checkout {branch} && git reset --mixed {base} && git checkout {base} && git branch -D {branch}`"
            ))
        })?;
    }

    // The canonical pop mechanic (graphite/Sapling): move the branch ref and
    // HEAD down to the base's live tip while leaving the on-disk files alone.
    // The index now matches the base, so the branch's whole diff reads back as
    // unstaged modifications.
    let onto_tip = git.rev_parse(&base)?;
    git.reset_mixed(&onto_tip).map_err(|e| {
        Error::Usage(format!(
            "`{branch}` is removed from the stack but the mixed reset to `{base}` failed ({e}); its commits are intact, finish by hand: `git reset --mixed {base} && git checkout {base} && git branch -D {branch}`"
        ))
    })?;

    // Both refs now name the same commit and the index matches HEAD, so this
    // checkout touches no file: it only moves HEAD off the doomed ref, carrying
    // the unstaged diff along.
    git.checkout(&base).map_err(|e| {
        Error::Usage(format!(
            "popped `{branch}`'s changes into the working tree but switching to `{base}` failed ({e}); run `git checkout {base} && git branch -D {branch}` to finish"
        ))
    })?;

    delete_branch_ref(&git, &branch);

    report_pop(format, &branch, &base, &reparented, &restacked);
    Ok(())
}

/// The shared removal core: drop `branch` from state and reparent its
/// immediate children onto `base` in ONE transactional update (BEFORE any git
/// surgery, so the recorded stack never holds a half-removed branch), then
/// restack the children's subtrees. Each child's `base.hash` is deliberately
/// KEPT (the `untrack` reparent semantics): it marks where the child's own
/// commits start, exactly the range the forced restack replays onto the base's
/// live tip, dropping the removed branch's commits from the child's history.
/// Returns `(reparented children, restacked branches)`.
fn remove_and_restack_children(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    branch: &str,
    base: &str,
) -> Result<(Vec<String>, Vec<String>), Error> {
    let children = ops::children(&state.branches, branch);
    let upstack = ops::upstack_order(&state.branches, branch);

    // The state delta, applied to the in-memory state for the restack pass and
    // re-applied onto fresh state by every transactional save (idempotent, so
    // a CAS retry is safe).
    let base_owned = base.to_string();
    let apply_removal = |s: &mut State| {
        s.branches.remove(branch);
        for child in &children {
            if let Some(b) = s.branches.get_mut(child) {
                b.base.name.clone_from(&base_owned);
            }
        }
    };
    apply_removal(state);
    store.update(|s| {
        apply_removal(s);
        Ok(())
    })?;

    // The children's subtrees, bottom-up (the upstack minus the removed branch
    // itself, mirroring fold). Only the immediate children need forcing; their
    // descendants stop descending their base's tip as soon as the child
    // rebases, so the plain skip check handles them. A conflict resumes via the
    // plain `Restack` continuation; a forced sibling still queued there resumes
    // unforced and may keep the removed commits until the next restack moves
    // its base, which is content-safe, just delayed.
    let order: Vec<String> = upstack.iter().skip(1).cloned().collect();
    let force: BTreeSet<String> = children.iter().cloned().collect();
    let restacked = restack_with_recovery_forced(
        git,
        store,
        state,
        repo,
        &order,
        |remaining| recovery::Operation::Restack { remaining },
        &apply_removal,
        &force,
        false,
    )?;
    clear_conflict_artifacts(git);
    Ok((children, restacked))
}

/// The delete safety predicate (graphite's `isSafeToDelete`): deleting without
/// `--force` is allowed when the branch is merged into its base, OR its diff
/// vs the base is empty, OR its recorded PR is closed or merged on GitHub.
/// The PR check runs last (it is a network fetch) and is best-effort: an
/// unreachable or unrecorded PR reads as unknown, which does NOT count as
/// safe. Anything else is a structured refusal naming `--force`.
fn ensure_safe_to_delete(
    git: &Git,
    repo: &RepoConfig,
    branch: &str,
    base: &str,
    tip: &str,
    pr_number: Option<u64>,
) -> Result<(), Error> {
    let base_tip = git.rev_parse(base)?;
    if git.is_ancestor(tip, &base_tip)? {
        return Ok(()); // merged into its base
    }
    // Empty diff vs the base: the tip's tree matches the merge-base's tree
    // (covers both "no own commits" and "commits with no net change").
    let merge_base = git.merge_base(tip, &base_tip)?;
    if git.rev_parse(&format!("{tip}^{{tree}}"))?
        == git.rev_parse(&format!("{merge_base}^{{tree}}"))?
    {
        return Ok(());
    }
    if let Some(number) = pr_number {
        if matches!(
            live_pr_state(git, repo, number),
            Some(PrState::Closed | PrState::Merged)
        ) {
            return Ok(());
        }
    }
    Err(Error::Usage(format!(
        "`{branch}` is not merged into `{base}`, its diff is not empty, and its PR is not closed or merged; pass --force to delete it anyway (its children will be reparented onto `{base}`)"
    )))
}

/// The live state of PR `number`, or `None` when it cannot be fetched (no
/// token, no network, a non-GitHub remote); unknown deliberately reads as
/// not-safe in the predicate above.
fn live_pr_state(git: &Git, repo: &RepoConfig, number: u64) -> Option<PrState> {
    let url = git.remote_url(&repo.remote).ok()?;
    let (owner, name) = stacc_github::parse_remote(&url)?;
    let pr = GitHub::from_env()
        .ok()?
        .get_pull_request(&owner, &name, number)
        .ok()?;
    Some(pr.state)
}

/// Delete `branch`'s git ref, leased on its current tip so a concurrent move
/// is never clobbered. Best-effort: the state surgery is already saved, so a
/// failure here is a warning naming the manual cleanup, not a fatal error.
fn delete_branch_ref(git: &Git, branch: &str) {
    let head_ref = format!("refs/heads/{branch}");
    match git.ref_commit(&head_ref) {
        Ok(Some(tip)) => {
            if let Err(err) = git.delete_ref(&head_ref, Some(&tip)) {
                eprintln!(
                    "warning: removed `{branch}` from the stack but could not delete its ref ({err}); delete it with `git branch -D {branch}`"
                );
            }
        }
        Ok(None) => {} // already gone
        Err(err) => eprintln!(
            "warning: removed `{branch}` from the stack but could not read its ref to delete it ({err}); delete it with `git branch -D {branch}`"
        ),
    }
}

fn report_delete(
    format: OutputFormat,
    branch: &str,
    base: &str,
    reparented: &[String],
    restacked: &[String],
    pr_closed: Option<bool>,
) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "delete",
                "branch": branch,
                "reparented": reparented,
                "restacked": restacked,
                "pr_closed": pr_closed,
            })
        ),
        OutputFormat::Pretty => {
            println!("Deleted {branch}");
            if !reparented.is_empty() {
                println!("  reparented onto {base}: {}", reparented.join(", "));
            }
            for name in restacked {
                println!("Restacked {name}");
            }
            match pr_closed {
                Some(true) => println!("Closed {branch}'s PR"),
                Some(false) => println!("Could not close {branch}'s PR (see warning)"),
                None => {}
            }
        }
    }
}

fn report_pop(
    format: OutputFormat,
    branch: &str,
    onto: &str,
    reparented: &[String],
    restacked: &[String],
) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "pop",
                "branch": branch,
                "onto": onto,
                "reparented": reparented,
                "restacked": restacked,
            })
        ),
        OutputFormat::Pretty => {
            println!("Popped {branch} onto {onto}; its changes are unstaged in the working tree");
            if !reparented.is_empty() {
                println!("  reparented onto {onto}: {}", reparented.join(", "));
            }
            for name in restacked {
                println!("Restacked {name}");
            }
        }
    }
}
