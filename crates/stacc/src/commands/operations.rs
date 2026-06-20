//! Stack operations: sync, restack, modify, move, and the conflict-recovery
//! (continue/abort) lifecycle they share.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::Path;

use serde_json::{json, Value};
use stacc_core::{ops, recovery};
use stacc_forge::{MergeRejectionReason, SCHEMA_VERSION};
use stacc_git::{Git, Hunk, HunkKind, MergeEquivalence, RebaseError};
use stacc_github::{CheckRollup, GitHub, GitHubError, PrState, PullRequestUpdate};
use stacc_state::{
    Base, BranchState, Disposal, PullRequest, RepoConfig, State, StateError, StateStore,
};

use super::absorb;
use super::split::spec_matches;
use crate::cli::{
    FoldArgs, MergeArgs, MergedArgs, ModifyArgs, MoveArgs, OutputFormat, RestackArgs, SquashArgs,
    SyncArgs, UndoArgs,
};
use crate::error::Error;

/// `stacc modify`: fold staged changes into the current branch (amend its tip by
/// default, append with `--commit`, reword-only with `--edit`), then restack its
/// upstack onto the new tip. `--all` stages everything first; `--patch <paths>`
/// narrows the staged set to the matching paths, leaving the rest staged;
/// `--into <branch>` lands the staged changes in a downstack branch's tip
/// instead (see [`modify_into`]). On conflict, records an `Operation::Modify`
/// whose `pre_amend` anchor lets `abort` undo the amend. Local-only: no push.
// A cohesive validate -> stage/narrow -> amend -> restack -> report sequence;
// splitting it would only trade this lint for too_many_arguments on a helper.
#[allow(clippy::too_many_lines)]
pub fn modify(args: &ModifyArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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
        Error::Usage("cannot modify a detached HEAD; check out a branch first".into())
    })?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot modify the trunk branch `{}`",
            repo.trunk
        )));
    }
    let base_name = state
        .branches
        .get(&branch)
        .map(|b| b.base.name.clone())
        .ok_or_else(|| {
            Error::Usage(format!(
                "branch `{branch}` is not tracked; run `stacc track` first"
            ))
        })?;

    validate_modify_flags(args)?;

    if args.all {
        git.stage_all_respecting_ignores()?;
    }

    if args.into.is_some() {
        return modify_into(&git, &store, &mut state, &repo, &branch, args, format);
    }

    // Fail fast if any branch we would restack is checked out in another
    // worktree, rather than amending and then skipping that child mid-pass.
    guard_worktree(&git, &ops::upstack_order(&state.branches, &branch))?;

    // --patch: narrow the staged set to the matching paths. The non-matching
    // paths are unstaged around the amend and re-staged after the restack.
    let excluded = if args.patch.is_empty() {
        Vec::new()
    } else {
        let (included, excluded) = partition_patch(&git.diff_hunks()?, &args.patch);
        if included.is_empty() {
            return Err(Error::Usage(
                "no staged changes match --patch; stage the paths first or fix the pathspecs"
                    .into(),
            ));
        }
        if !excluded.is_empty() {
            git.unstage_paths(&excluded)?;
        }
        excluded
    };

    let pre_amend = git.rev_parse("HEAD")?;

    if args.commit {
        if !git.has_staged_changes()? {
            return Err(Error::Usage(
                "nothing staged to commit; stage changes, or drop --commit to amend".into(),
            ));
        }
        let message = args.message.clone().unwrap_or_else(|| branch.clone());
        git.commit(&message)?;
    } else {
        // --edit guarantees a pure reword: with staged content the amend would
        // change the tree too, so refuse rather than silently fold it in.
        if args.edit && git.has_staged_changes()? {
            return Err(Error::Usage(
                "staged changes present; --edit rewords only. Unstage them, or drop --edit to fold them into the tip".into(),
            ));
        }
        // Amending a branch with no commit of its own would rewrite the base's
        // commit; require an explicit --commit there instead.
        if pre_amend == git.rev_parse(&base_name)? {
            return Err(Error::Usage(format!(
                "`{branch}` has no commit of its own above `{base_name}`; use --commit to add one"
            )));
        }
        // A bare amend with nothing staged and no reword only churns the commit
        // timestamp, forcing a needless upstack restack; refuse it.
        if !git.has_staged_changes()? && args.message.is_none() {
            return Err(Error::Usage(
                "nothing staged to amend; stage changes, pass -m to reword, or --commit to append"
                    .into(),
            ));
        }
        git.commit_amend(args.message.as_deref())?;
    }

    // Restack the upstack onto the amended tip. The engine checks out each child
    // it rebases, so restore the user to the modified branch afterward.
    let order = ops::upstack_order(&state.branches, &branch);
    let restacked = restack_with_recovery(
        &git,
        &store,
        &mut state,
        &repo,
        &order,
        |remaining| recovery::Operation::Modify {
            branch: branch.clone(),
            remaining,
            pre_amend: pre_amend.clone(),
        },
        // No command-specific state change: the amend is git-only, so the engine's
        // base.hash deltas are the whole logical change.
        &|_s| {},
    )?;
    clear_conflict_artifacts(&git);
    // Best-effort: the work is already done and saved, so a failure to switch
    // back to the modified branch must not report the whole modify as failed.
    if let Err(err) = git.checkout(&branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    // Put back what --patch excluded: a restack's autostash round-trip leaves
    // those changes unstaged in the working tree, so re-stage them last.
    if !excluded.is_empty() {
        if let Err(err) = git.stage_paths(&excluded) {
            eprintln!("warning: could not re-stage the paths --patch excluded: {err}");
        }
    }

    let tip = git.rev_parse(&branch)?;
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "modify",
                "branch": branch,
                "amended": !args.commit,
                "sha": tip,
                "restacked": restacked,
            })
        ),
        OutputFormat::Pretty => {
            if args.commit {
                println!("Committed to {branch}");
            } else {
                println!("Amended {branch}");
            }
            for name in &restacked {
                println!("Restacked {name}");
            }
        }
    }
    Ok(())
}

/// Reject contradictory `modify` flag combinations with structured usage
/// errors (kept out of clap so `--format json` renders them like every other
/// error).
fn validate_modify_flags(args: &ModifyArgs) -> Result<(), Error> {
    if args.edit {
        if args.commit {
            return Err(Error::Usage(
                "--edit rewords the tip in place; it cannot be combined with --commit".into(),
            ));
        }
        if args.all || !args.patch.is_empty() {
            return Err(Error::Usage(
                "--edit rewords only; it cannot be combined with --all or --patch".into(),
            ));
        }
        if args.into.is_some() {
            return Err(Error::Usage(
                "--edit rewords the current tip; it cannot be combined with --into".into(),
            ));
        }
        if args.message.is_none() {
            return Err(Error::Usage(
                "--edit requires --message; stacc never opens an editor (interactive rewording is a possible future convenience)".into(),
            ));
        }
    }
    if args.into.is_some() {
        if args.commit {
            return Err(Error::Usage(
                "--into amends the target's tip in place; it cannot be combined with --commit"
                    .into(),
            ));
        }
        if args.message.is_some() {
            return Err(Error::Usage(
                "--into keeps the target commit's message; --message is not supported with it"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// Partition the staged hunks by the `--patch` pathspecs (literal or directory
/// prefix, the `split --by-file` rule): hunks touching a matching path are
/// included, and the distinct non-matching paths are returned so the caller
/// can unstage them around the commit and re-stage them afterwards.
fn partition_patch(hunks: &[Hunk], specs: &[String]) -> (Vec<Hunk>, Vec<String>) {
    let mut included = Vec::new();
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    for hunk in hunks {
        let matched = specs.iter().any(|spec| {
            spec_matches(spec, &hunk.path)
                || hunk
                    .old_path
                    .as_deref()
                    .is_some_and(|old| spec_matches(spec, old))
        });
        if matched {
            included.push(hunk.clone());
        } else {
            excluded.insert(hunk.path.clone());
            if let Some(old) = &hunk.old_path {
                excluded.insert(old.clone());
            }
        }
    }
    (included, excluded.into_iter().collect())
}

/// `stacc modify --into <target>`: land the staged changes in `target`'s tip
/// instead of the current branch. The mechanics are absorb's: splice the staged
/// hunks into the target's tip commit and replay every commit above it up to
/// the current tip in memory (the shared [`absorb::rewrite_chain`]), move each
/// chain branch's ref to its rewritten tip in lockstep, mixed-reset the current
/// branch so the landed hunks read as committed, and restack the target's
/// upstack. Only in-place line edits are supported (the splice machinery needs
/// pre-image lines); file adds, deletes, renames, binaries, and pure insertions
/// are structured errors, never silently misplaced.
// A cohesive validate -> map -> rewrite -> move-refs -> restack sequence,
// mirroring absorb's shape.
#[allow(clippy::too_many_lines)]
fn modify_into(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    branch: &str,
    args: &ModifyArgs,
    format: OutputFormat,
) -> Result<(), Error> {
    let target = args.into.as_deref().expect("--into is set");

    // The target must be strictly below the current branch in its downstack.
    if target == branch {
        return Err(Error::Usage(format!(
            "`{target}` is the current branch; run `stacc modify` without --into"
        )));
    }
    let chain = ops::downstack_chain(state, branch, &repo.trunk)?;
    let Some(pos) = chain.iter().position(|b| b == target) else {
        return Err(Error::Usage(format!(
            "`{target}` is not in `{branch}`'s downstack; --into lands changes in a branch below the current one"
        )));
    };

    if !git.has_staged_changes()? {
        return Err(Error::Usage(
            "nothing staged to apply; stage the changes you want landed in the target first"
                .into(),
        ));
    }

    let tip = git.rev_parse(branch)?;
    let target_tip = git.rev_parse(target)?;
    // The rewrite replays `target_tip..tip`, so the chain must be restacked.
    if !git.is_ancestor(&target_tip, &tip)? {
        return Err(Error::Usage(format!(
            "`{branch}` does not descend `{target}`'s tip; run `stacc restack` first, then retry"
        )));
    }

    // The staged hunks, narrowed by --patch when given; non-matching paths stay
    // staged (re-staged after the mixed reset below).
    let hunks = git.diff_hunks()?;
    let (included, excluded) = if args.patch.is_empty() {
        (hunks, Vec::new())
    } else {
        let (included, excluded) = partition_patch(&hunks, &args.patch);
        if included.is_empty() {
            return Err(Error::Usage(
                "no staged changes match --patch; stage the paths first or fix the pathspecs"
                    .into(),
            ));
        }
        (included, excluded)
    };

    // Every hunk must be a splice the target's blobs can take, checked up front
    // so the command errors before mutating anything.
    let mut mapped = Vec::new();
    for hunk in included {
        if !matches!(hunk.kind, HunkKind::Modified) || hunk.old_range.count == 0 {
            return Err(Error::Usage(format!(
                "cannot apply `{}` into `{target}`: --into supports in-place edits to existing lines only (not file adds, deletes, renames, binaries, or pure insertions); land it with a plain `stacc modify`",
                hunk.path
            )));
        }
        let path = hunk.old_path.clone().unwrap_or_else(|| hunk.path.clone());
        if git.read_blob(&target_tip, &path)?.is_none() {
            return Err(Error::Usage(format!(
                "`{path}` does not exist in `{target}`'s tip; cannot land its changes there"
            )));
        }
        mapped.push(absorb::Mapped {
            hunk,
            commit: target_tip.clone(),
        });
    }

    // Fail fast if anything the rewrite or restack touches is checked out in
    // another worktree, BEFORE mutating (mirrors absorb).
    let upstack = ops::upstack_order(&state.branches, target);
    guard_worktree(git, &upstack)?;

    // The chain to rewrite: the target's tip plus everything above it up to the
    // current tip, oldest first. Every hunk targets the first commit, so the
    // shared propagation applies it there and in every descendant.
    let mut commits = vec![target_tip.clone()];
    commits.extend(git.rev_list(&target_tip, &tip)?);
    let parent_of_first = git.rev_parse(&format!("{target_tip}^")).ok();
    let new_by_old = absorb::rewrite_chain(git, parent_of_first.as_deref(), &commits, &mapped)?;

    // Move each chain branch from the target up to (and including) the current
    // branch onto its rewritten tip, leased on the old one. A drifted member
    // whose tip is not in the rewritten chain is left to the restack below.
    for member in &chain[pos..] {
        let old = git.rev_parse(member)?;
        if let Some(new) = new_by_old.get(&old) {
            git.update_ref(&format!("refs/heads/{member}"), new, Some(&old))
                .map_err(|e| {
                    Error::Usage(format!(
                        "could not move `{member}` to its rewritten tip ({e}); the tip moved under modify, run `stacc restack` to reconcile"
                    ))
                })?;
        }
    }

    // The current branch is checked out here: sync HEAD and the index to the
    // rewritten tip while leaving the working tree alone, so the landed hunks
    // read as committed and only what --patch excluded remains a modification.
    let new_tip = new_by_old.get(&tip).cloned().unwrap_or(tip);
    git.reset_mixed(&new_tip)?;

    // Each chain member above the target now sits on its parent's rewritten
    // tip; record those base hashes (idempotent, re-applied on CAS retries).
    let mut hash_updates: Vec<(String, String)> = Vec::new();
    for pair in chain[pos..].windows(2) {
        hash_updates.push((pair[1].clone(), git.rev_parse(&pair[0])?));
    }
    let apply_hashes = move |s: &mut State| {
        for (child, hash) in &hash_updates {
            if let Some(b) = s.branches.get_mut(child) {
                b.base.hash.clone_from(hash);
            }
        }
    };
    apply_hashes(state);

    // Restack the target's upstack. The chain members moved in lockstep, so
    // they skip as already-based; only branches hanging off the chain rebase.
    let restacked = restack_with_recovery(
        git,
        store,
        state,
        repo,
        &upstack,
        |remaining| recovery::Operation::Restack { remaining },
        &apply_hashes,
    )?;
    clear_conflict_artifacts(git);
    if let Err(err) = git.checkout(branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    // Put back what --patch excluded: the mixed reset (and any restack
    // autostash) left those changes unstaged in the working tree.
    if !excluded.is_empty() {
        if let Err(err) = git.stage_paths(&excluded) {
            eprintln!("warning: could not re-stage the paths --patch excluded: {err}");
        }
    }

    let sha = git.rev_parse(branch)?;
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "modify",
                "branch": branch,
                "into": target,
                "applied": mapped.len(),
                "sha": sha,
                "restacked": restacked,
            })
        ),
        OutputFormat::Pretty => {
            println!("Applied {} staged hunk(s) into {target}", mapped.len());
            for name in &restacked {
                println!("Restacked {name}");
            }
        }
    }
    Ok(())
}

/// `stacc squash`: collapse the current branch's own commits into a single
/// commit, then restack its upstack onto the squashed tip.
///
/// Refuses unless the branch is already restacked onto its base (git-spice's
/// `VerifyRestacked`): on a drifted branch, `base..tip` includes commits that
/// are not part of the branch's own diff, and the squash would fold them in.
/// The squash itself is pure object surgery: the tip's tree IS the squashed
/// content, so it is committed directly onto the base tip (`commit-tree`), with
/// no rebase, no working-tree side effects, and no possible conflict. Only the
/// upstack restack can conflict, and that is a plain resumable `Restack`.
pub fn squash(args: &SquashArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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
        Error::Usage("cannot squash a detached HEAD; check out a branch first".into())
    })?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot squash the trunk branch `{}`",
            repo.trunk
        )));
    }
    let base = state
        .branches
        .get(&branch)
        .map(|b| b.base.clone())
        .ok_or_else(|| {
            Error::Usage(format!(
                "branch `{branch}` is not tracked; run `stacc track` first"
            ))
        })?;

    let tip = git.rev_parse(&branch)?;

    // Precondition (git-spice `VerifyRestacked`): the branch must sit on its
    // base's live tip, the same "already on top of its base" condition the
    // restack engine uses to skip a branch. When it holds, `base_tip..tip` is
    // exactly the branch's own commits and `base_tip` is exactly where they
    // fork, so committing the tip's tree onto `base_tip` is sound.
    let base_tip = git.rev_parse(&base.name)?;
    if !git.is_ancestor(&base_tip, &tip)? {
        return Err(Error::Usage(format!(
            "`{branch}` is not restacked onto `{}`; run `stacc restack` first, then squash",
            base.name
        )));
    }

    // Fail fast if the branch or any upstack child is checked out in another
    // worktree, BEFORE mutating anything (mirrors `modify`/`absorb`).
    let upstack = ops::upstack_order(&state.branches, &branch);
    guard_worktree(&git, &upstack)?;

    // The branch's own commits, oldest-first. Zero or one: nothing to collapse,
    // a clear no-op report rather than an error.
    let own_commits = git.rev_list(&base_tip, &tip)?;
    if own_commits.len() < 2 {
        report_squash(format, &branch, 0, &tip, &[]);
        return Ok(());
    }

    // The combined message: `--message` verbatim, or the oldest-first
    // concatenation of the squashed commits' subjects and bodies.
    let message = match &args.message {
        Some(m) => m.clone(),
        None => squash_message(&git, &own_commits)?,
    };

    // The tip's tree is already the squashed content; commit it onto the base
    // tip and move the branch ref there with the old tip as a lease.
    // (`commit_tree` stamps stacc's identity; the original subjects and bodies
    // survive in the concatenated message.)
    let tree = git.rev_parse(&format!("{tip}^{{tree}}"))?;
    let new_tip = git.commit_tree(&tree, Some(&base_tip), &message)?;
    git.update_ref(&format!("refs/heads/{branch}"), &new_tip, Some(&tip))
        .map_err(|e| {
            Error::Usage(format!(
                "could not move `{branch}` to the squashed tip ({e}); the branch tip moved under squash, re-run it"
            ))
        })?;
    // No reset: HEAD follows the moved ref, and the new tip's tree is identical
    // to the old one, so the index and working tree (including anything the
    // user has staged) already match it exactly.

    // Restack the upstack onto the squashed tip. The base pointer did not move,
    // so the only command-specific delta is refreshing the recorded base hash,
    // a no-op unless it had drifted while the branch still descended the live
    // base tip (e.g. a hand-run rebase).
    let restacked = restack_with_recovery(
        &git,
        &store,
        &mut state,
        &repo,
        &upstack,
        |remaining| recovery::Operation::Restack { remaining },
        &|s| {
            if let Some(b) = s.branches.get_mut(&branch) {
                b.base.hash.clone_from(&base_tip);
            }
        },
    )?;
    clear_conflict_artifacts(&git);
    // Best-effort: the engine leaves HEAD on the last child it rebased, so
    // restore the user to the squashed branch.
    if let Err(err) = git.checkout(&branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    report_squash(format, &branch, own_commits.len(), &new_tip, &restacked);
    Ok(())
}

/// The default squash message: the squashed commits' subjects and bodies,
/// oldest-first, blank-line separated.
fn squash_message(git: &Git, commits: &[String]) -> Result<String, Error> {
    let mut parts = Vec::with_capacity(commits.len());
    for commit in commits {
        let subject = git.commit_subject(commit)?;
        let body = git.commit_body(commit)?;
        if body.is_empty() {
            parts.push(subject);
        } else {
            parts.push(format!("{subject}\n\n{body}"));
        }
    }
    Ok(parts.join("\n\n"))
}

fn report_squash(
    format: OutputFormat,
    branch: &str,
    squashed: usize,
    sha: &str,
    restacked: &[String],
) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "squash",
                "branch": branch,
                "squashed": squashed,
                "sha": sha,
                "restacked": restacked,
            })
        ),
        OutputFormat::Pretty => {
            if squashed == 0 {
                println!("Nothing to squash.");
            } else {
                println!("Squashed {squashed} commits on {branch}");
                for name in restacked {
                    println!("Restacked {name}");
                }
            }
        }
    }
}

/// `stacc fold`: fold the current branch into its parent by fast-forwarding the
/// parent's ref to the branch's tip, reparenting the branch's children onto the
/// parent, dropping the branch from state, and deleting its git ref. `--close`
/// closes the folded branch's PR (best-effort).
///
/// Refuses unless the branch is already restacked onto its parent (git-spice's
/// `VerifyRestacked`, same check as `squash`): on a drifted branch the parent's
/// ref move would not be a fast-forward and would fold in commits that are not
/// part of the branch's own diff. With the precondition held, the fold itself
/// cannot conflict; only the children's restack can, and that resumes via
/// `continue`/`abort` through `Operation::Fold`.
// A cohesive validate -> ff -> reparent/restack -> finish sequence, like `merge`.
#[allow(clippy::too_many_lines)]
pub fn fold(args: &FoldArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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
        Error::Usage("cannot fold a detached HEAD; check out a branch first".into())
    })?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot fold the trunk branch `{}`",
            repo.trunk
        )));
    }
    let branch_state = state.branches.get(&branch).cloned().ok_or_else(|| {
        Error::Usage(format!(
            "branch `{branch}` is not tracked; run `stacc track` first"
        ))
    })?;
    let parent = branch_state.base.name.clone();

    // Precondition (git-spice `VerifyRestacked`, mirroring `squash`): the branch
    // must sit on its parent's live tip, so moving the parent's ref to the
    // branch's tip is a true fast-forward that adds exactly the branch's own
    // commits to the parent.
    let tip = git.rev_parse(&branch)?;
    let parent_pre_tip = git.rev_parse(&parent)?;
    if !git.is_ancestor(&parent_pre_tip, &tip)? {
        return Err(Error::Usage(format!(
            "`{branch}` is not restacked onto `{parent}`; run `stacc restack` first, then fold"
        )));
    }

    // Fail fast if any branch this fold rewrites is checked out in another
    // worktree, BEFORE mutating anything. Unlike the other surgery commands the
    // PARENT is guarded too: its ref moves, which would desync a worktree that
    // has it checked out.
    let upstack = ops::upstack_order(&state.branches, &branch);
    let mut guarded = vec![parent.clone()];
    guarded.extend(upstack.iter().cloned());
    guard_worktree(&git, &guarded)?;

    // Folding into the trunk rewrites trunk history locally; legal, but usually
    // a mistake (git-spice warns here too). Non-interactive: warn, don't prompt.
    let into_trunk = parent == repo.trunk;
    if into_trunk {
        eprintln!(
            "warning: folding `{branch}` into the trunk `{parent}`; this moves the trunk's local tip. Run `stacc undo` if it was a mistake."
        );
    }

    // The immediate children and their pre-fold base hashes (their pre-fold base
    // name is `branch` by definition), recorded for `abort`.
    let children = ops::children(&state.branches, &branch);
    let children_pre: Vec<(String, String)> = children
        .iter()
        .filter_map(|c| state.branches.get(c).map(|b| (c.clone(), b.base.hash.clone())))
        .collect();

    // Fast-forward the parent to the branch's tip, with the parent's live tip as
    // a lease (the CAS equivalent of git-spice's `git fetch . child:parent`).
    git.update_ref(&format!("refs/heads/{parent}"), &tip, Some(&parent_pre_tip))
        .map_err(|e| {
            Error::Usage(format!(
                "could not fast-forward `{parent}` to `{branch}`'s tip ({e}); the parent tip moved under fold, re-run it"
            ))
        })?;

    // The state delta: drop the folded branch and reparent its children onto the
    // parent at its new tip. Applied to the in-memory state for the restack pass
    // and re-applied onto fresh state by the transactional save.
    let apply_fold = |s: &mut State| {
        s.branches.remove(&branch);
        for child in &children {
            if let Some(b) = s.branches.get_mut(child) {
                b.base.name.clone_from(&parent);
                b.base.hash.clone_from(&tip);
            }
        }
    };
    apply_fold(&mut state);

    // Restack the folded branch's former upstack (its children's subtrees).
    // They sat on the folded tip, which IS the parent's new tip, so this is
    // normally a skip-everything no-op; a stale child rebases and can conflict,
    // which resumes via the `Operation::Fold` continuation.
    let order: Vec<String> = upstack.iter().skip(1).cloned().collect();
    let restacked = restack_with_recovery(
        &git,
        &store,
        &mut state,
        &repo,
        &order,
        |remaining| recovery::Operation::Fold {
            branch: branch.clone(),
            parent: parent.clone(),
            remaining,
            parent_pre_tip: parent_pre_tip.clone(),
            branch_base_hash: branch_state.base.hash.clone(),
            children_pre: children_pre.clone(),
            pr_number: branch_state.pr.as_ref().map(|pr| pr.number),
            pr_url: branch_state.pr.as_ref().and_then(|pr| pr.url.clone()),
            close: args.close,
        },
        &apply_fold,
    )?;
    clear_conflict_artifacts(&git);

    // The fold is saved: switch to the parent (the folded branch is checked out
    // here), then delete the folded ref, in that order so HEAD never names a
    // deleted ref.
    finish_fold_refs(&git, &branch, &parent);

    let pr_closed = if args.close {
        close_pr_best_effort(&git, &repo, &branch, branch_state.pr.as_ref().map(|pr| pr.number))
    } else {
        None
    };

    report_fold(format, &branch, &parent, &restacked, pr_closed, into_trunk);
    Ok(())
}

/// Finish a fold's ref surgery: switch to the parent, then delete the folded
/// branch's ref (it is fully merged: the parent's tip IS its tip, so this has
/// `git branch -d` safety). Both steps are best-effort, the fold's state work
/// is already saved; a failed checkout skips the deletion so HEAD never points
/// at a deleted ref.
fn finish_fold_refs(git: &Git, branch: &str, parent: &str) {
    if let Err(err) = git.checkout(parent) {
        eprintln!(
            "warning: could not switch to `{parent}` ({err}); `{branch}` is folded but its ref was left in place, delete it with `git branch -d {branch}`"
        );
        return;
    }
    let head_ref = format!("refs/heads/{branch}");
    match git.ref_commit(&head_ref) {
        // Lease on the current tip so a concurrent move is never clobbered.
        Ok(Some(tip)) => {
            if let Err(err) = git.delete_ref(&head_ref, Some(&tip)) {
                eprintln!(
                    "warning: folded `{branch}` but could not delete its ref ({err}); delete it with `git branch -d {branch}`"
                );
            }
        }
        Ok(None) => {} // already gone
        Err(err) => eprintln!(
            "warning: folded `{branch}` but could not read its ref to delete it ({err}); delete it with `git branch -d {branch}`"
        ),
    }
}

/// Close `branch`'s PR on GitHub, best-effort: `Some(true)` when it closed,
/// `Some(false)` (plus a warning) when the attempt failed, `None` when there is
/// no recorded PR to close. Shared by `fold` and `delete`, whose `--close` is
/// an opt-in side effect that must never fail the already-finished local work.
pub(crate) fn close_pr_best_effort(
    git: &Git,
    repo: &RepoConfig,
    branch: &str,
    number: Option<u64>,
) -> Option<bool> {
    let Some(number) = number else {
        eprintln!("note: `{branch}` has no recorded PR; nothing to close");
        return None;
    };
    let attempt = (|| -> Result<(), Error> {
        let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
            .ok_or_else(|| Error::Usage(format!("remote `{}` is not a GitHub URL", repo.remote)))?;
        GitHub::from_env()?.close_pull_request(&owner, &repo_name, number)?;
        Ok(())
    })();
    match attempt {
        Ok(()) => Some(true),
        Err(err) => {
            eprintln!(
                "warning: could not close `{branch}`'s PR #{number} ({err}); close it on GitHub manually"
            );
            Some(false)
        }
    }
}

fn report_fold(
    format: OutputFormat,
    branch: &str,
    parent: &str,
    restacked: &[String],
    pr_closed: Option<bool>,
    into_trunk: bool,
) {
    match format {
        OutputFormat::Json => {
            let mut v = json!({
                "op": "fold",
                "branch": branch,
                "into": parent,
                "restacked": restacked,
                "pr_closed": pr_closed,
            });
            if into_trunk {
                v["folded_into_trunk"] = json!(true);
            }
            println!("{v}");
        }
        OutputFormat::Pretty => {
            if into_trunk {
                println!("Folded {branch} into {parent} (the trunk)");
            } else {
                println!("Folded {branch} into {parent}");
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

/// `stacc move`: re-parent the current branch (and its upstack) onto `--onto`.
/// With `--only`, the branch moves alone: its children are reparented onto its
/// old base (the same reparent delta as `delete`/`pop`, refs untouched) so they
/// stay put instead of following the move. Rejects a move onto the branch's own
/// upstack (a cycle). On conflict records an `Operation::Move` whose `pre_base`
/// lets `abort` roll the recorded base back. Local-only: no push.
pub fn move_cmd(args: &MoveArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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

    let branch = git
        .current_branch()
        .map_err(|_| Error::Usage("cannot move a detached HEAD; check out a branch first".into()))?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot move the trunk branch `{}`",
            repo.trunk
        )));
    }
    let pre_base = state
        .branches
        .get(&branch)
        .map(|b| b.base.name.clone())
        .ok_or_else(|| {
            Error::Usage(format!(
                "branch `{branch}` is not tracked; run `stacc track` first"
            ))
        })?;

    let onto = &args.onto;
    // The new base must be the trunk or another tracked branch.
    if onto != &repo.trunk && !state.branches.contains_key(onto) {
        return Err(Error::Usage(format!(
            "`{onto}` is not the trunk or a tracked branch; cannot move onto it"
        )));
    }
    if onto == &pre_base {
        return Err(Error::Usage(format!(
            "`{branch}` is already based on `{onto}`"
        )));
    }
    // Moving onto the branch itself or anything in its own upstack is a cycle.
    let subtree = ops::upstack_order(&state.branches, &branch);
    if subtree.iter().any(|b| b == onto) {
        return Err(Error::Usage(format!(
            "cannot move `{branch}` onto `{onto}`: that is the branch itself or part of its upstack (a cycle)"
        )));
    }
    // If the branch already descends `onto`, a move would only flatten the
    // intermediate branches out, a different operation, and `restack` would skip
    // the branch as already-based and silently no-op, leaving state claiming a
    // new base the history never adopted. Reject it. `rev_parse` also confirms
    // `onto` exists in git, not just in recorded state.
    let onto_tip = git.rev_parse(onto)?;
    if git.is_ancestor(&onto_tip, &branch)? {
        return Err(Error::Usage(format!(
            "`{branch}` already descends `{onto}`; move re-parents onto a different lineage, it does not flatten the stack"
        )));
    }

    // --only: the branch moves alone. Its children are reparented onto its old
    // base so they stay put; their refs are untouched and their `base.hash` is
    // kept (the `delete`/`pop` reparent semantics: it marks where each child's
    // own commits start, so a later restack replays exactly those onto the old
    // base, not the moved branch's commits).
    let reparented = if args.only {
        ops::children(&state.branches, &branch)
    } else {
        Vec::new()
    };
    let order: Vec<String> = if args.only {
        vec![branch.clone()]
    } else {
        subtree.clone()
    };

    // Fail fast if any branch we would restack is checked out in another
    // worktree.
    guard_worktree(&git, &order)?;

    // The state delta: re-point the moved branch's base name (keep base.hash, it
    // marks where the branch's own commits start, which restack replays onto the
    // new base's tip) and, under --only, the children's bases onto the old one.
    // Applied to fresh state on every transactional save, so a concurrent change
    // to another branch is preserved by the re-apply.
    let apply_move = |s: &mut State| {
        if let Some(b) = s.branches.get_mut(&branch) {
            b.base.name.clone_from(onto);
        }
        for child in &reparented {
            if let Some(b) = s.branches.get_mut(child) {
                b.base.name.clone_from(&pre_base);
            }
        }
    };
    apply_move(&mut state);

    let restacked = restack_with_recovery(
        &git,
        &store,
        &mut state,
        &repo,
        &order,
        |remaining| recovery::Operation::Move {
            branch: branch.clone(),
            remaining,
            pre_base: pre_base.clone(),
        },
        &apply_move,
    )?;
    clear_conflict_artifacts(&git);
    // Best-effort: the move is already saved, so a failure to switch back to the
    // moved branch must not report the whole move as failed.
    if let Err(err) = git.checkout(&branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    let tip = git.rev_parse(&branch)?;
    report_move(format, &branch, onto, &tip, &restacked, &reparented, &pre_base);
    Ok(())
}

fn report_move(
    format: OutputFormat,
    branch: &str,
    base: &str,
    sha: &str,
    restacked: &[String],
    reparented: &[String],
    old_base: &str,
) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "move",
                "branch": branch,
                "base": base,
                "sha": sha,
                "restacked": restacked,
                "reparented": reparented,
            })
        ),
        OutputFormat::Pretty => {
            println!("Moved {branch} onto {base}");
            if !reparented.is_empty() {
                println!("  reparented onto {old_base}: {}", reparented.join(", "));
            }
            for name in restacked {
                println!("Restacked {name}");
            }
        }
    }
}

/// `stacc sync`: reconcile merged PRs and restack the stack.
///
/// Detects branches whose PR has merged (re-parenting their children and
/// dropping them), pulls the trunk from upstream, then restacks the remaining
/// branches bottom-up onto their bases.
pub fn sync(args: &SyncArgs, format: OutputFormat, no_interactive: bool, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    if args.continue_ {
        return continue_op(&git, &store, &mut state, &repo, format);
    }

    // Adoption + merged-PR detection need the GitHub API. Skip them under
    // `--offline`; otherwise build the client ONCE and run them. A missing
    // token or non-GitHub remote is a hard error: no silent degradation, which
    // is exactly what rebased a squash-merged branch into a phantom conflict
    // (STA-90). The `--continue` resume above returns before here, so it never
    // needs a token.
    let has_tracked = !state.branches.is_empty();
    let local_mode = stacc_config::local_mode(&work_dir.join(".stacc.toml"));
    // `--offline` skips the fetch and detection both. Otherwise classify the
    // merge-detection path: reachable GitHub uses the authoritative API; a
    // forge-less repo (local-mode key, a non-GitHub remote, a missing token, or
    // an unreachable API) fetches trunk but only PROPOSES likely-merged branches
    // via the local heuristic and never drops on it (KTD-4/6). `detection_skipped`
    // marks a run that emitted the skipped note (the JSON contract), as for
    // `--offline`.
    let (adopted, merged, likely_merged, detection_skipped) = if args.offline || !has_tracked {
        if args.offline && has_tracked {
            eprintln!(
                "note: --offline skipped merged-PR detection; run `stacc sync` online to reconcile merged PRs."
            );
        }
        (Vec::new(), BTreeSet::new(), Vec::new(), args.offline && has_tracked)
    } else {
        match reconcile_detection(&git, &store, &mut state, &repo, local_mode)? {
            Detection::Api { adopted, merged } => (adopted, merged, Vec::new(), false),
            Detection::ForgeLess { likely, note } => {
                if note {
                    eprintln!(
                        "note: skipped merged-PR detection (no reachable forge); proposing likely-merged branches from local history, reconcile one with `stacc merged <branch>`."
                    );
                }
                (Vec::new(), BTreeSet::new(), likely, note)
            }
        }
    };
    // Snapshot the merged branches' tips NOW, the leases for the ref cleanup
    // after the reconcile: a branch that moves mid-sync is kept, not destroyed.
    let leases: Vec<(String, String)> = merged
        .iter()
        .filter_map(|name| ref_lease(&git, name))
        .collect();
    // Prune tracked branches whose git ref is gone and which carry no PR. (A
    // branch with a PR is governed by merge-detection above: a merged PR drops
    // it, an open/closed one keeps it, so an in-flight branch is never silently
    // pruned.) `--no-prune` opts out.
    let pruned = if args.no_prune {
        BTreeSet::new()
    } else {
        missing_ref_branches(&git, &state)
    };
    let drop: BTreeSet<String> = merged.union(&pruned).cloned().collect();
    // tree_guard = true: sync's restack pass skips branches whose tree already
    // matches their base (the squash-merge backstop). Merge passes false.
    let outcome = reconcile_with(&git, &store, &mut state, &repo, drop, Vec::new(), args.offline, true)?;
    // The merged branches landed and their children are restacked off them:
    // delete their local refs, unless the user asked to keep merged branches.
    // (The pruned set's refs are already gone, nothing to clean there.)
    let (cleaned, cleanup_skipped) = if args.keep_branches {
        (Vec::new(), Vec::new())
    } else {
        cleanup_merged_refs(&git, &repo.trunk, &leases)
    };

    // Surface local branches that stacc doesn't track yet (and the user hasn't
    // previously declined). In interactive sessions, offer to track them now.
    // In non-interactive / JSON mode, report them in the output so agents can act.
    let local_branches = git.local_branches().unwrap_or_default();
    let tracked_names: BTreeSet<&str> = state.branches.keys().map(String::as_str).collect();
    let candidates: Vec<String> = local_branches
        .into_iter()
        .filter(|b| {
            b != &repo.trunk
                && !tracked_names.contains(b.as_str())
                && !repo.declined_tracking.contains(b)
        })
        .collect();

    let mut untracked_for_report: Vec<String> = Vec::new();
    if !candidates.is_empty() {
        if crate::interactive::allowed(std::io::stdin().is_terminal(), no_interactive, format) {
            let inferred: Vec<(String, String)> = candidates
                .iter()
                .map(|b| {
                    let base = infer_base(&git, &state.branches, &repo.trunk, b);
                    (b.clone(), base)
                })
                .collect();
            let items: Vec<String> = inferred
                .iter()
                .map(|(b, base)| format!("{b} (parent: {base})"))
                .collect();
            let selected =
                crate::interactive::prompt_multi_select("Track local branches?", &items)?;
            let mut to_decline: Vec<String> = Vec::new();
            for (i, (branch, base)) in inferred.iter().enumerate() {
                if selected.contains(&i) {
                    super::track_branch_impl(&git, &store, branch, base)?;
                } else {
                    to_decline.push(branch.clone());
                }
            }
            if !to_decline.is_empty() {
                store.update(|s| {
                    if let Some(r) = s.repo.as_mut() {
                        for b in &to_decline {
                            r.declined_tracking.insert(b.clone());
                        }
                    }
                    Ok(())
                })?;
            }
        } else {
            untracked_for_report = candidates;
        }
    }

    report_sync(
        format,
        "sync",
        &merged,
        &pruned,
        &adopted,
        &outcome.reparented,
        &outcome.restacked,
        &cleaned,
        &cleanup_skipped,
        detection_skipped,
        &likely_merged,
        &untracked_for_report,
    );
    Ok(())
}

/// Tracked branches whose local git ref is gone and which carry no PR, so `sync`
/// can prune the dead state. Branches with a PR are deliberately excluded: they
/// flow through merge-detection instead, which keeps an open/closed PR's branch.
fn missing_ref_branches(git: &Git, state: &State) -> BTreeSet<String> {
    state
        .branches
        .iter()
        .filter(|(_, b)| b.pr.is_none())
        .filter(|(name, _)| git.ref_missing(name))
        .map(|(name, _)| name.clone())
        .collect()
}

/// What a reconcile pass did: the merged branches it dropped, the children it
/// re-parented (name -> new base), and the branches it restacked.
struct SyncOutcome {
    merged: BTreeSet<String>,
    reparented: Vec<(String, String)>,
    restacked: Vec<String>,
}

/// Build the GitHub client for sync's API work, or a hard error. A non-GitHub
/// remote and a missing token are both fatal: stacc v1 is GitHub-only and ships
/// no degraded path (one would rebase squash-merged branches into phantom
/// conflicts). The error names the remote, never its URL, which can carry
/// `user:token@` credentials.
fn github_client(git: &Git, repo: &RepoConfig) -> Result<(GitHub, String, String), Error> {
    let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
        .ok_or_else(|| {
            Error::Usage(format!(
                "remote `{}` is not a GitHub URL; stacc v1 is GitHub-only (use `--offline` to restack local refs without merged-PR detection)",
                repo.remote
            ))
        })?;
    let github = GitHub::from_env()?;
    Ok((github, owner, repo_name))
}

/// The forge boundary for `submit` and `merge`. stacc v1 opens pull requests on
/// GitHub only, so when the user has opted into local mode or the remote is not a
/// github.com URL the operation is unavailable: return a forge-generic message
/// (it names the remote, never echoes its URL, and never detects or names a
/// specific forge) instead of attempting the API. On a reachable github.com
/// remote it resolves the owner/repo and builds the client.
///
/// The forge-less mode is selected by the local-mode config key and the remote
/// shape, never a per-command flag (R11): there is deliberately no `--local` or
/// `--no-forge` override to keep the flag surface from multiplying.
pub(super) fn require_github_forge(
    git: &Git,
    repo: &RepoConfig,
    op: &str,
    work_dir: &Path,
) -> Result<(GitHub, String, String), Error> {
    if stacc_config::local_mode(&work_dir.join(".stacc.toml")) {
        return Err(Error::Usage(format!(
            "local mode is on, so `stacc {op}` is unavailable; stacc v1 opens pull requests on GitHub only. Push your branch and open a change through your forge directly."
        )));
    }
    let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
        .ok_or_else(|| {
            Error::Usage(format!(
                "remote `{}` is not a GitHub URL, so `stacc {op}` is unavailable; stacc v1 opens pull requests on GitHub only. Push your branch and open a change through your forge directly.",
                repo.remote
            ))
        })?;
    let github = GitHub::from_env()?;
    Ok((github, owner, repo_name))
}

/// Build the client and run sync's adoption + merged-PR detection, returning
/// the adopted PRs and the set of merged branch names. Any failure here is
/// fatal so sync never silently degrades into a phantom-conflict rebase.
fn detect_and_adopt(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
) -> Result<(Vec<AdoptedPr>, BTreeSet<String>), Error> {
    let (github, owner, repo_name) = github_client(git, repo)?;
    let (adopted, adopted_merged) = adopt_prs(&github, &owner, &repo_name, store, state)?;
    let adopted_names: BTreeSet<String> = adopted.iter().map(|a| a.branch.clone()).collect();
    let mut merged = detect_merged(&github, &owner, &repo_name, state, &adopted_names)?;
    merged.extend(adopted_merged.into_keys());
    Ok((adopted, merged))
}

/// How `sync` resolves merges this run.
enum Detection {
    /// Reachable GitHub: the authoritative API result (adopted PRs, merged set).
    Api {
        adopted: Vec<AdoptedPr>,
        merged: BTreeSet<String>,
    },
    /// Forge-less (local-mode key, a non-GitHub remote, a missing token, or an
    /// unreachable API): the local heuristic's propose-only list. `note` is false
    /// only when the user explicitly opted in via the local-mode key.
    ForgeLess {
        likely: Vec<ops::LikelyMerged>,
        note: bool,
    },
}

/// Classify how to reconcile merges and run it. Forge-less engages when the
/// per-repo local-mode key is set, the remote is not a `github.com` URL, the
/// GitHub token is missing, or the first API call cannot reach GitHub; otherwise
/// the authoritative API runs. The local heuristic only PROPOSES (KTD-4/6):
/// nothing is dropped on a forge-less run.
fn reconcile_detection(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    local_mode: bool,
) -> Result<Detection, Error> {
    if local_mode {
        let likely = ops::detect_merged_local(git, state, &repo.trunk)?;
        return Ok(Detection::ForgeLess { likely, note: false });
    }
    // A non-`github.com` remote (GitLab, Bitbucket, GitHub Enterprise) cannot be
    // told apart by URL, so it auto-engages forge-less WITH a note, so it can
    // never silently skip detection (the STA-94 contract).
    if stacc_github::parse_remote(&git.remote_url(&repo.remote)?).is_none() {
        let likely = ops::detect_merged_local(git, state, &repo.trunk)?;
        return Ok(Detection::ForgeLess { likely, note: true });
    }
    // `github.com`: the authoritative API. A missing token or an unreachable API
    // falls back to forge-less with a note rather than hard-erroring.
    match detect_and_adopt(git, store, state, repo) {
        Ok((adopted, merged)) => Ok(Detection::Api { adopted, merged }),
        Err(err) if is_forge_unreachable(&err) => {
            let likely = ops::detect_merged_local(git, state, &repo.trunk)?;
            Ok(Detection::ForgeLess { likely, note: true })
        }
        Err(err) => Err(err),
    }
}

/// Whether an error means the GitHub API is simply unreachable (no token, or a
/// transport failure), as opposed to a real API error the user must see.
fn is_forge_unreachable(err: &Error) -> bool {
    matches!(
        err,
        Error::Github(GitHubError::MissingToken | GitHubError::Transport(_))
    )
}

/// Ask GitHub which recorded PRs have merged, returning their branch names.
/// `skip` names branches whose PR state this sync already fetched (the ones
/// just adopted, known open), so they are not queried a second time.
fn detect_merged(
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    state: &State,
    skip: &BTreeSet<String>,
) -> Result<BTreeSet<String>, Error> {
    let with_prs: Vec<(String, u64)> = state
        .branches
        .iter()
        .filter(|(name, _)| !skip.contains(*name))
        .filter_map(|(name, b)| b.pr.as_ref().map(|pr| (name.clone(), pr.number)))
        .collect();
    let mut merged: BTreeSet<String> = BTreeSet::new();
    for (name, number) in &with_prs {
        if github.get_pull_request(owner, repo_name, *number)?.state == PrState::Merged {
            merged.insert(name.clone());
        }
    }
    Ok(merged)
}

/// A PR adopted by head-branch lookup: a tracked branch with no recorded PR
/// that GitHub says has an open one (created via `gh` or the web UI).
struct AdoptedPr {
    branch: String,
    number: u64,
    url: String,
}

/// The shared adoption core of `sync` and `merge`: look up each candidate (a
/// tracked branch with no recorded PR) by head branch, in any state. Open PRs
/// are recorded in state (persisted, and mirrored into `state` in memory) and
/// returned; merged PRs are returned (branch -> PR number) for the caller's
/// merge handling. Closed-unmerged PRs are deliberately not recorded: `submit`
/// opens a fresh PR for a closed head, and a recorded closed PR would
/// resurrect it instead. (`submit` has its own open-only lookup: it adopts in
/// order to update the PR, where merged and closed heads must fall through to
/// create.)
///
/// A failed lookup is fatal: a broken token or unreachable network is an error,
/// not "no PR", so callers never silently skip merged-PR reconciliation (the
/// STA-90 phantom-conflict bug). Adoptions found before the failing lookup are
/// persisted first. `Ok(None)` (no PR anywhere) is the common WIP case and is
/// skipped, not an error.
fn adopt_prs_among(
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    store: &StateStore,
    state: &mut State,
    candidates: &[String],
) -> Result<(Vec<AdoptedPr>, BTreeMap<String, u64>), Error> {
    let mut adopted: Vec<AdoptedPr> = Vec::new();
    let mut merged: BTreeMap<String, u64> = BTreeMap::new();
    for name in candidates {
        match github.pull_request_for_branch_any_state(owner, repo_name, name) {
            Ok(Some(pr)) => match pr.state {
                PrState::Merged => {
                    merged.insert(name.clone(), pr.number);
                }
                PrState::Open => {
                    if let Some(branch) = state.branches.get_mut(name) {
                        branch.pr = Some(PullRequest {
                            number: pr.number,
                            url: Some(pr.url.clone()),
                        });
                    }
                    adopted.push(AdoptedPr {
                        branch: name.clone(),
                        number: pr.number,
                        url: pr.url,
                    });
                }
                PrState::Closed => {}
            },
            Ok(None) => {}
            Err(e) => {
                // Persist what we adopted before surfacing the failure, so a
                // mid-loop error does not lose PRs GitHub already confirmed.
                let _ = persist_adopted(store, &adopted);
                return Err(e.into());
            }
        }
    }
    persist_adopted(store, &adopted)?;
    Ok((adopted, merged))
}

/// Persist adopted open PRs into state before the caller's fallible work
/// (sync's fetch and restack, merge's walk), so a later failure does not lose
/// what GitHub already told us.
fn persist_adopted(store: &StateStore, adopted: &[AdoptedPr]) -> Result<(), Error> {
    if adopted.is_empty() {
        return Ok(());
    }
    store.update(|s| {
        for a in adopted {
            if let Some(branch) = s.branches.get_mut(&a.branch) {
                branch.pr = Some(PullRequest {
                    number: a.number,
                    url: Some(a.url.clone()),
                });
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// `sync`'s adoption pass: every tracked branch with no recorded PR is a
/// candidate, looked up by head branch with the client built once by the
/// caller. A lookup failure is fatal (see [`adopt_prs_among`]).
fn adopt_prs(
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    store: &StateStore,
    state: &mut State,
) -> Result<(Vec<AdoptedPr>, BTreeMap<String, u64>), Error> {
    let candidates: Vec<String> = state
        .branches
        .iter()
        .filter(|(_, b)| b.pr.is_none())
        .map(|(name, _)| name.clone())
        .collect();
    if candidates.is_empty() {
        return Ok((Vec::new(), BTreeMap::new()));
    }
    adopt_prs_among(github, owner, repo_name, store, state, &candidates)
}

/// Reconcile a caller-supplied drop set and restack the stack, the shared core
/// of `sync` (merged-plus-pruned branches) and `merge` (the branches it just
/// merged): re-parent the dropped branches' children onto the nearest surviving
/// base, drop them, fast-forward the trunk (unless `offline`), then restack the
/// remainder bottom-up. Persists state and best-effort pushes it.
// One shared sync/merge/merged reconcile core threads the drop set, disposal
// records, and two mode flags; a parameter struct would only move this lint
// without helping the three callers.
#[allow(clippy::too_many_arguments)]
fn reconcile_with(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    dropped: BTreeSet<String>,
    disposals: Vec<(String, Disposal)>,
    offline: bool,
    tree_guard: bool,
) -> Result<SyncOutcome, Error> {
    // Re-parent children of dropped branches onto the nearest surviving base.
    let mut reparented: Vec<(String, String)> = Vec::new();
    for (name, branch) in &state.branches {
        if dropped.contains(name) {
            continue;
        }
        let new_base = ops::resolve_base(&state.branches, &dropped, branch.base.name.clone());
        if new_base != branch.base.name {
            reparented.push((name.clone(), new_base));
        }
    }
    for (name, new_base) in &reparented {
        if let Some(branch) = state.branches.get_mut(name) {
            branch.base.name.clone_from(new_base);
        }
    }
    for name in &dropped {
        state.branches.remove(name);
    }
    for (key, record) in &disposals {
        state.disposals.insert(key.clone(), record.clone());
    }

    // The command's own state change, re-applied onto fresh state whenever the
    // transactional save retries: drop the merged/pruned branches, re-parent
    // their children, and append any disposal records. All are idempotent
    // (removes, base-repoints, and keyed inserts), so re-applying after a CAS
    // miss is safe, and the disposal record lands in the same commit as the drop.
    let dropped_delta = dropped.clone();
    let reparented_delta = reparented.clone();
    let has_disposals = !disposals.is_empty();
    let apply_drops = move |s: &mut State| {
        for (name, new_base) in &reparented_delta {
            if let Some(b) = s.branches.get_mut(name) {
                b.base.name.clone_from(new_base);
            }
        }
        for name in &dropped_delta {
            s.branches.remove(name);
        }
        for (key, record) in &disposals {
            s.disposals.insert(key.clone(), record.clone());
        }
    };

    // Persist the dropped/re-parented branches transactionally before the
    // fallible fetch and restack, so PRs already merged on GitHub are not
    // stranded in local state if the fetch or restack then fails (a re-run
    // reconciles from here).
    if !dropped.is_empty() || has_disposals {
        persist_restack(store, &apply_drops, &[])?;
    }

    // Pull the trunk from upstream. Strict by default, a flaky network or a
    // bad remote should surface immediately. `--offline` opts out and restacks
    // on whatever refs are already local.
    if !offline {
        if let Err(err) = fast_forward_trunk(git, &repo.remote, &repo.trunk) {
            eprintln!("hint: pass --offline to skip the fetch and restack on local refs only");
            return Err(err);
        }
    }

    // Pull-and-restack the remaining branches bottom-up onto their bases.
    let order = ops::topo_order(&state.branches, &repo.trunk);
    let restacked = restack_with_recovery_forced(
        git,
        store,
        state,
        repo,
        &order,
        |remaining| recovery::Operation::Sync { remaining },
        &apply_drops,
        &BTreeSet::new(),
        tree_guard,
    )?;

    finish_sync(git, store, repo);
    Ok(SyncOutcome {
        merged: dropped,
        reparented,
        restacked,
    })
}

/// `stacc merged <branch>`: reconcile a branch whose changes already landed in
/// trunk without a forge. Drops it, re-parents its children, records the
/// disposal, and keeps the dropped tip reachable. Disposes only on a
/// deterministic merge proof (`is_ancestor` / `same_tree`); a propose-only
/// patch-id match refuses unless `--assume-merged` overrides it (KTD-4).
/// State-first: the drop and the record land in one compare-and-swap, then the
/// keep-alive ref transaction (KTD-5). Local-only: no forge call.
pub fn merged(args: &MergedArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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
            "cannot reconcile the trunk branch `{}`",
            repo.trunk
        )));
    }
    let branch_state = state.branches.get(&branch).cloned().ok_or_else(|| {
        Error::Usage(format!(
            "branch `{branch}` is not tracked; stacc only reconciles branches it manages"
        ))
    })?;

    let tip = git
        .rev_parse(&branch)
        .map_err(|_| Error::Usage(format!("branch `{branch}` has no git ref to reconcile")))?;

    // Verify the merge. A deterministic proof disposes; a propose-only patch-id
    // match (or no match at all) needs `--assume-merged` (KTD-4).
    let evidence = match (git.merge_equivalence(&repo.trunk, &branch)?, args.assume_merged) {
        (MergeEquivalence::Ancestor, _) => "ancestor",
        (MergeEquivalence::SameTree, _) => "same_tree",
        (MergeEquivalence::NetDiff, true) => "net_diff",
        (MergeEquivalence::NotFound, true) => "assume_merged",
        (MergeEquivalence::NetDiff, false) => {
            return Err(Error::Usage(format!(
                "`{branch}`'s changes look already on `{}` but stacc cannot prove it merged; pass `--assume-merged` to drop it",
                repo.trunk
            )));
        }
        (MergeEquivalence::NotFound, false) => {
            return Err(Error::Usage(format!(
                "`{branch}` does not look merged into `{}`; pass `--assume-merged` if you have confirmed it merged out of band",
                repo.trunk
            )));
        }
    };

    // Guards (mirroring delete): never delete or rebase a ref checked out in
    // another worktree, and move off the branch before it is dropped.
    guard_worktree(&git, &ops::upstack_order(&state.branches, &branch))?;
    if git.current_branch().ok().as_deref() == Some(branch.as_str()) {
        git.checkout(&repo.trunk).map_err(|e| {
            Error::Usage(format!(
                "`{branch}` is checked out; switching to the trunk `{}` before dropping it failed ({e})",
                repo.trunk
            ))
        })?;
    }
    let here = git.current_branch().ok();

    // Capture the stack shape for the record before reconcile mutates state.
    let dropped: BTreeSet<String> = std::iter::once(branch.clone()).collect();
    let base = ops::resolve_base(&state.branches, &dropped, branch_state.base.name.clone());
    let children = ops::children(&state.branches, &branch);
    // Stamp the drop time so retention prunes keep-alive refs by when they were
    // dropped, not by the dropped tip's commit date (a long-lived branch can have
    // an old tip). Best-effort: a clock failure records 0, which sorts oldest.
    let dropped_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    let record = Disposal {
        branch: branch.clone(),
        tip: tip.clone(),
        base,
        children,
        evidence: evidence.to_string(),
        dropped_at,
    };
    let disposals = vec![(format!("{branch}@{tip}"), record)];

    // Drop + reparent + record in one CAS (state-first), restacking children
    // against the local trunk (offline: a dispose makes no forge call).
    match reconcile_with(&git, &store, &mut state, &repo, dropped, disposals, true, false) {
        Ok(_) => {}
        Err(err @ Error::Conflict { .. }) => {
            // State landed (drop + disposal record), but the children restack
            // stopped before the branch ref could be removed. Preserve the dropped
            // tip now so the recorded keep-alive ref exists and the `git branch -D`
            // below is safe; the branch ref lingers until the user removes it.
            store.preserve_tip(&branch, &tip)?;
            eprintln!(
                "note: `{branch}` is dropped from the stack and its tip is preserved at `refs/stacc/dropped/{branch}-{tip}`; after resolving and `stacc continue`, remove its git ref with `git branch -D {branch}`"
            );
            return Err(err);
        }
        Err(err) => return Err(err),
    }

    // The children's restack may have left HEAD on the last rebased branch; put
    // the user back where they started.
    if let Some(here) = &here {
        if git.current_branch().ok().as_deref() != Some(here.as_str()) {
            if let Err(err) = git.checkout(here) {
                eprintln!("warning: could not switch back to `{here}`: {err}");
            }
        }
    }

    // State landed; now the git refs. The keep-alive ref preserves the dropped
    // tip and the branch ref is deleted under its OID in one transaction, so a
    // failure here leaves the branch re-trackable (KTD-5).
    store.keep_alive_and_delete(&branch, &tip)?;
    store.prune_dropped()?;

    report_merged(format, &branch, evidence, &tip);
    Ok(())
}

fn report_merged(format: OutputFormat, branch: &str, evidence: &str, tip: &str) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "merged",
                "branch": branch,
                "evidence": evidence,
                "dropped_tip": tip,
                "schema_version": SCHEMA_VERSION,
            })
        ),
        OutputFormat::Pretty => println!(
            "Dropped `{branch}` (merged: {evidence}); its tip is kept at `refs/stacc/dropped/{branch}-{tip}`"
        ),
    }
}

/// A PR `merge` resolved: the branch, its number, the squash commit SHA when we
/// merged it (`None` otherwise), and whether GitHub already had it merged.
struct MergedPr {
    branch: String,
    number: u64,
    sha: Option<String>,
    out_of_band: bool,
}

/// The result of walking the downstack chain: what merged, the children it
/// re-parented and restacked onto the trunk between merges, the merged local
/// refs it deleted (and the ones it kept, with reasons), where it stopped
/// short (structured), and any deferred hard error.
struct MergeWalk {
    merged: Vec<MergedPr>,
    reparented: Vec<(String, String)>,
    restacked: Vec<String>,
    cleaned: Vec<String>,
    cleanup_skipped: Vec<CleanupSkip>,
    stopped: Option<Value>,
    error: Option<Error>,
}

/// `stacc merge`: squash-merge the ready PRs from the trunk up to the current
/// branch, bottom-up, then reconcile via the `sync` logic. Stops at the first PR
/// that is not cleanly mergeable. No-op (with a message) when nothing is ready.
// A cohesive validate -> protection-check -> retarget -> walk -> report ->
// restore sequence; splitting it would only trade this lint for
// too_many_arguments on a helper (same as modify).
#[allow(clippy::too_many_lines)]
pub fn merge(args: &MergeArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    let current = git.current_branch().map_err(|_| {
        Error::Usage("cannot merge from a detached HEAD; check out a branch first".into())
    })?;
    if current == repo.trunk {
        return Err(Error::Usage("cannot merge the trunk branch".into()));
    }
    // The merge loop restacks between merges; refuse to start on top of an
    // in-progress rebase (e.g. a prior merge stopped on a conflict) so we never
    // clobber a continuation or rebase into a half-rebased tree.
    if git.rebase_in_progress() {
        return Err(Error::Usage(
            "a rebase is already in progress; run `stacc continue` to resume it or `stacc abort` to undo".into(),
        ));
    }

    let (github, owner, repo_name) = require_github_forge(&git, &repo, "merge", work_dir)?;

    // Build the downstack chain ONCE: merged branches leave state, which would
    // make a re-derived `downstack_chain` error mid-loop.
    let chain = ops::downstack_chain(&state, &current, &repo.trunk)?;

    // Without trunk branch protection, GitHub's "clean" means only "no merge
    // conflicts": required checks and reviews are not enforced. Warn loudly.
    // Distinguish "confirmed unprotected" (404) from "could not check" (a token
    // scope or transient API error): `--require-protected` refuses both, but with
    // different, honest messages.
    let protected = match github.branch_protected(&owner, &repo_name, &repo.trunk) {
        Ok(true) => true,
        Ok(false) => {
            if args.require_protected {
                return Err(Error::Usage(format!(
                    "`{}` has no branch protection; enable it, or drop --require-protected to merge anyway",
                    repo.trunk
                )));
            }
            eprintln!(
                "warning: `{}` has no branch protection, so a `clean` PR means only `no merge conflicts`; required checks and reviews are NOT enforced here.",
                repo.trunk
            );
            false
        }
        Err(err) => {
            if args.require_protected {
                return Err(Error::Usage(format!(
                    "could not verify `{}` branch protection ({err}); resolve it, or drop --require-protected to merge anyway",
                    repo.trunk
                )));
            }
            eprintln!(
                "warning: could not check `{}` branch protection ({err}); proceeding as if unprotected, so required checks and reviews are NOT enforced.",
                repo.trunk
            );
            false
        }
    };

    // Adopt PRs created outside stacc (gh, the web UI) for chain branches with
    // no recorded PR, BEFORE the retarget pass below: an adopted child PR must
    // also be pointed at the trunk, or its parent's branch deletion on merge
    // closes it. Open PRs are recorded (so the walk merges them); merged ones
    // are carried into the walk as already-merged. (API only, runs in
    // `--offline` too.)
    let candidates: Vec<String> = chain
        .iter()
        .filter(|name| state.branches.get(*name).is_some_and(|b| b.pr.is_none()))
        .cloned()
        .collect();
    let (adopted, adopted_merged) =
        adopt_prs_among(&github, &owner, &repo_name, &store, &mut state, &candidates)?;
    for a in &adopted {
        eprintln!("note: adopted PR #{} for `{}`: {}", a.number, a.branch, a.url);
    }

    // Also adopt PRs for off-chain children of any chain branch (branches not
    // being merged in this run but whose parent branch will be deleted). Without
    // adoption their externally-opened PRs lack a recorded PR number and the
    // retarget pass silently skips them. Off-chain children are not in the merge
    // walk, so their adopted_merged result carries no action here.
    let chain_set: BTreeSet<&str> = chain.iter().map(String::as_str).collect();
    let off_chain_children: Vec<String> = chain
        .iter()
        .flat_map(|branch| {
            ops::children(&state.branches, branch)
                .into_iter()
                .filter(|b| !chain_set.contains(b.as_str()))
        })
        .collect();
    let off_chain_candidates: Vec<String> = off_chain_children
        .into_iter()
        .filter(|name| state.branches.get(name).is_some_and(|b| b.pr.is_none()))
        .collect();
    if !off_chain_candidates.is_empty() {
        let (off_adopted, _) = adopt_prs_among(
            &github,
            &owner,
            &repo_name,
            &store,
            &mut state,
            &off_chain_candidates,
        )?;
        for a in &off_adopted {
            eprintln!("note: adopted PR #{} for `{}`: {}", a.number, a.branch, a.url);
        }
    }

    // Capture the direct children of the starting branch before state is mutated
    // by the merge walk. If `current` is itself merged (its local ref deleted),
    // the checkout at the end of `merge` falls back to the first surviving child
    // rather than the trunk, so the user can immediately run `stacc submit`
    // without a manual `stacc checkout` first (STA-125).
    let current_children: Vec<String> = ops::children(&state.branches, &current);

    // Retarget every non-bottom open PR to the trunk UP FRONT, before any parent
    // merges. A child PR whose base is the parent's branch is closed (un-
    // reopenably) when GitHub deletes that branch on merge; pointing it at the
    // trunk first keeps it open regardless of branch-deletion timing. (API only,
    // so it runs in `--offline` too.)
    retarget_children_to_trunk(&github, &owner, &repo_name, &repo.trunk, &state, &chain)?;

    // Walk bottom-up: merge each PR, then restack the rest onto the freshly
    // merged trunk and force-push the next branch, so its PR is `trunk + its own
    // commits` and merges cleanly (no squash-cascade conflict).
    // `--watch`: when a child stops only because its restacked CI is still
    // running, poll its checks and continue instead of stopping for a manual
    // re-run. Bounded by a wall-clock timeout, with a poll-count backstop
    // (polls = timeout / interval, at least one) for stubbed clocks.
    let watch = args.watch.then(|| Watch {
        interval: std::time::Duration::from_secs(args.watch_interval.max(1)),
        timeout: std::time::Duration::from_secs(args.watch_timeout.max(1)),
        polls: u32::try_from(args.watch_timeout.max(1) / args.watch_interval.max(1))
            .unwrap_or(u32::MAX)
            .max(1),
    });

    let MergeWalk {
        merged: merged_prs,
        reparented,
        restacked,
        cleaned,
        cleanup_skipped,
        stopped,
        error: loop_err,
    } = merge_stack(
        &git,
        &store,
        &mut state,
        &repo,
        &github,
        &owner,
        &repo_name,
        &chain,
        &adopted_merged,
        args.offline,
        args.keep_branches,
        watch,
    );

    if args.offline && !merged_prs.is_empty() {
        eprintln!(
            "note: --offline skipped the fetch; run `stacc sync` to rebase the local stack onto the merged commits."
        );
    }

    let outcome = (!merged_prs.is_empty()).then(|| SyncOutcome {
        merged: merged_prs.iter().map(|m| m.branch.clone()).collect(),
        reparented,
        restacked,
    });

    report_merge(
        format,
        &merged_prs,
        stopped.as_ref(),
        protected,
        outcome.as_ref(),
        &cleaned,
        &cleanup_skipped,
    );

    // Restore the user's branch. Each restack leaves HEAD on whichever branch it
    // rebased last, so without this `merge` silently strands you on a different
    // branch. Skip when a conflict left a rebase in progress (HEAD must stay on
    // the conflicting branch for `stacc continue`). The starting branch may have
    // been merged and dropped from state but its local ref still exists; fall
    // back to the first surviving direct child (so `stacc submit` works
    // immediately without a manual checkout), or the trunk if no children survive.
    if !git.rebase_in_progress() {
        let target: &str = if git.ref_missing(&current) {
            current_children
                .iter()
                .find(|b| !git.ref_missing(b))
                .map(String::as_str)
                .unwrap_or(&repo.trunk)
        } else {
            &current
        };
        let _ = git.checkout(target);
    }

    // A conflict during a mid-merge restack stops the walk with some PRs already
    // merged; point the user at the resume path that finishes the remaining PRs.
    if !merged_prs.is_empty() && matches!(loop_err, Some(Error::Conflict { .. })) {
        eprintln!(
            "note: merged {} PR(s) before the restack conflicted; after resolving and `stacc continue`, run `stacc merge` again to finish the rest.",
            merged_prs.len()
        );
    }

    if let Some(err) = loop_err {
        return Err(err);
    }
    Ok(())
}

/// Point every non-bottom open PR in the chain at the trunk, so deleting a
/// parent's branch on merge cannot close its child. Idempotent (re-setting an
/// already-trunk base is a no-op). Only the bottom branch keeps its trunk base.
fn retarget_children_to_trunk(
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    trunk: &str,
    state: &State,
    chain: &[String],
) -> Result<(), Error> {
    // Retarget (1) every in-chain non-bottom PR and (2) every open PR whose
    // state-parent is any chain branch but is itself outside the chain. Case (2)
    // covers partial merges (user on a mid-stack branch) including forked stacks:
    // when A -> B -> {C, D} with the user on D, chain = [A, B, D] and C (B's
    // other child) would be orphaned when B is deleted. Collecting off-chain
    // children of ALL chain branches (not only chain.last()) covers both the
    // linear and forked cases.
    let chain_set: BTreeSet<&str> = chain.iter().map(String::as_str).collect();
    let off_chain_children: Vec<String> = chain
        .iter()
        .flat_map(|branch| {
            ops::children(&state.branches, branch)
                .into_iter()
                .filter(|b| !chain_set.contains(b.as_str()))
        })
        .collect();
    let to_retarget: Vec<&String> = chain
        .iter()
        .skip(1)
        .chain(off_chain_children.iter())
        .collect();
    for branch in to_retarget {
        let Some(pr) = state.branches.get(branch).and_then(|b| b.pr.as_ref()) else {
            continue;
        };
        if github.get_pull_request(owner, repo_name, pr.number)?.state != PrState::Open {
            continue;
        }
        let update = PullRequestUpdate {
            base: Some(trunk.to_string()),
            ..Default::default()
        };
        if let Err(err) = github.update_pull_request(owner, repo_name, pr.number, &update) {
            // A child that merged or closed between the read above and this write
            // rejects a base change; it no longer needs retargeting, so skip it. A
            // still-open PR (or a failed re-check) is a real error.
            let confirmed_gone = github
                .get_pull_request(owner, repo_name, pr.number)
                .is_ok_and(|pr| pr.state != PrState::Open);
            if !confirmed_gone {
                return Err(err.into());
            }
        }
    }
    Ok(())
}

/// Walk the downstack `chain` bottom-up. For each PR: squash-merge it, then
/// reconcile (drop it, re-parent its children onto the trunk, and restack the
/// remainder onto the freshly merged trunk). Before merging a non-bottom PR,
/// force-push its branch, which the previous iteration's reconcile already
/// restacked onto the new trunk, so the PR is `trunk + its own commits` and
/// merges without a squash-cascade conflict. Stops at the first PR that is not
/// cleanly mergeable; defers a hard error so the caller still reports the merged
/// prefix. Mutates `state` (the reconcile drops/restacks as it goes). After each
/// merged PR's reconcile, deletes its local branch ref (leased on the tip it
/// had when it merged; `keep_branches` opts out).
///
/// `adopted_merged` names chain branches whose head-branch lookup (the adoption
/// pass in `merge`) found an already-merged PR; merged PRs are never recorded
/// in state, so they reach this walk with no recorded PR and would otherwise
/// stop it. They count like the `PrState::Merged` arm: reconciled away, not
/// re-queried.
// One cohesive bottom-up walk: every arm's break/stop is coupled to the loop's
// deferred-error reporting, so splitting it would only trade this lint for
// too_many_arguments on a helper (same trade-off as `merge` above).
/// How long `merge --watch` waits on a child stopped for pending CI.
#[derive(Clone, Copy)]
struct Watch {
    /// Pause between readiness polls.
    interval: std::time::Duration,
    /// Wall-clock budget: stop waiting once this much time has elapsed.
    timeout: std::time::Duration,
    /// Poll-count backstop, in case a stubbed/zero-duration clock never advances
    /// (`timeout / interval`).
    polls: u32,
}

/// What one readiness sample means for the watch loop.
enum WatchStep {
    /// Mergeable now: proceed to merge.
    Merge,
    /// Still resolvable by waiting (CI running, or checks passed but the forge
    /// has not recomputed mergeability yet): keep polling.
    Waiting,
    /// A block that waiting cannot clear (failed checks, conflict, no CI to wait
    /// on, review required): stop with this reason.
    Hard(MergeRejectionReason),
    /// A permanent read error (auth, not found): abort the wait.
    Permanent(Error),
}

/// How a `--watch` wait ended.
enum WatchOutcome {
    /// The change became mergeable; re-read and merge.
    Ready,
    /// A block that waiting cannot clear; stop with this reason.
    Hard(MergeRejectionReason),
    /// The deadline (or poll backstop) passed while still waiting on CI.
    TimedOut,
    /// A permanent read error aborted the wait.
    Errored(Error),
}

/// Classify one readiness sample. `Waiting` covers both pending CI and the
/// "checks passed but mergeability not yet recomputed" lag window, so a child
/// whose checks just went green is not abandoned mid-flight. A failed check, a
/// conflict, a behind branch, no CI to wait on, or a review block ends the wait.
fn watch_step(live: &stacc_github::PullRequest, checks: Option<CheckRollup>) -> WatchStep {
    if live.ready() {
        return WatchStep::Merge;
    }
    // The states where more polling could plausibly reach `clean`. A conflict or
    // behind branch never will, so they are never `Waiting`.
    let gateable = matches!(
        live.mergeable_state.as_deref(),
        Some("blocked" | "unstable" | "unknown") | None
    );
    match checks {
        // CI running, or passed but mergeability is lagging: keep waiting.
        Some(CheckRollup::Pending | CheckRollup::Pass) if gateable => WatchStep::Waiting,
        // Failed checks, no CI configured, or a non-CI block: waiting won't help.
        _ => WatchStep::Hard(stacc_github::merge_rejection_for(
            live.mergeable_state.as_deref(),
            checks,
        )),
    }
}

/// Run the `--watch` wait loop. Pure over its `poll`, `sleep`, and `expired`
/// closures so the loop logic (merge / keep-waiting / hard-stop / time-out) is
/// unit-testable without real time or network. Bounded by both a wall-clock
/// deadline (`expired`) and a poll-count backstop (`polls`).
fn run_watch(
    mut poll: impl FnMut() -> WatchStep,
    mut sleep: impl FnMut(std::time::Duration),
    mut expired: impl FnMut() -> bool,
    interval: std::time::Duration,
    mut polls: u32,
) -> WatchOutcome {
    loop {
        if polls == 0 || expired() {
            return WatchOutcome::TimedOut;
        }
        sleep(interval);
        polls -= 1;
        match poll() {
            WatchStep::Merge => return WatchOutcome::Ready,
            WatchStep::Waiting => {}
            WatchStep::Hard(reason) => return WatchOutcome::Hard(reason),
            WatchStep::Permanent(err) => return WatchOutcome::Errored(err),
        }
    }
}

/// The CI rollup for a change, best-effort (None on any read failure).
fn probe_checks(
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    number: u64,
) -> Option<CheckRollup> {
    github
        .pull_request_checks_within(owner, repo_name, &[number], std::time::Duration::from_secs(10))
        .ok()
        .and_then(|mut m| m.remove(&number))
        .and_then(|c| c.checks)
}

/// Whether a readiness read failed permanently (bad token, missing PR), so the
/// watch should abort rather than burn its budget retrying. Everything else
/// (rate limits, transport blips) is transient and keeps waiting.
fn is_permanent(err: &Error) -> bool {
    matches!(
        err,
        Error::Github(
            GitHubError::Status {
                status: 401 | 403 | 404,
                ..
            } | GitHubError::MissingToken
        )
    )
}

/// Build the structured `not_ready` stop envelope. `rejection`/`retryable`
/// reflect the state that *ended* the wait (not a stale pre-watch value), and
/// `watch_outcome` is set only on a `--watch` stop (`timed_out` | `hard_failed`).
/// The human `reason` string is frozen so machine consumers key on
/// `rejection`/`retryable`, never the prose.
fn build_stop(
    branch: &str,
    number: u64,
    live: &stacc_github::PullRequest,
    reason: MergeRejectionReason,
    retryable: bool,
    watch_outcome: Option<&str>,
) -> Value {
    json!({
        "kind": "not_ready",
        "branch": branch,
        "number": number,
        "readiness": super::readiness_str(live.mergeable_state.as_deref()),
        "rejection": serde_json::to_value(reason).unwrap_or(Value::Null),
        "retryable": retryable,
        "watch_outcome": watch_outcome,
        "reason": "not cleanly mergeable",
    })
}

/// The readiness gate for one PR in the merge walk.
enum Gate {
    /// The PR is mergeable.
    Ready,
    /// The PR is not mergeable and `--watch` could not clear it; the value is the
    /// structured stop to report.
    Stop(Value),
    /// A read failed.
    Failed(Error),
}

/// Resolve one PR's readiness: return the live PR once mergeable, a structured
/// stop when it is not (and `--watch` can't clear it), or an error. With `watch`
/// set, a stop that is only pending CI is polled until the checks pass (then
/// readiness is re-read and the PR merges) or the budget runs out.
fn await_pr_ready(
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    branch: &str,
    number: u64,
    watch: Option<Watch>,
) -> Gate {
    loop {
        let live = match poll_pr_ready(github, owner, repo_name, number) {
            Ok(live) => live,
            Err(err) => return Gate::Failed(err),
        };
        let checks = probe_checks(github, owner, repo_name, number);
        match watch_step(&live, checks) {
            WatchStep::Merge => return Gate::Ready,
            // Unreachable from a successful read (no error to carry), but the
            // match stays exhaustive over WatchStep.
            WatchStep::Permanent(err) => return Gate::Failed(err),
            WatchStep::Hard(reason) => {
                return Gate::Stop(build_stop(branch, number, &live, reason, false, None));
            }
            WatchStep::Waiting => {
                let Some(w) = watch else {
                    // No --watch: report the retryable "waiting on CI" stop so an
                    // agent knows to poll and retry.
                    return Gate::Stop(build_stop(
                        branch,
                        number,
                        &live,
                        MergeRejectionReason::ChecksPending,
                        true,
                        None,
                    ));
                };
                eprintln!("waiting on CI for #{number} (`{branch}`)...");
                let deadline = std::time::Instant::now() + w.timeout;
                let outcome = run_watch(
                    || match poll_pr_ready(github, owner, repo_name, number) {
                        Ok(live) => watch_step(&live, probe_checks(github, owner, repo_name, number)),
                        Err(err) if is_permanent(&err) => WatchStep::Permanent(err),
                        // A transient blip keeps waiting; the deadline bounds it.
                        Err(_) => WatchStep::Waiting,
                    },
                    std::thread::sleep,
                    move || std::time::Instant::now() >= deadline,
                    w.interval,
                    w.polls,
                );
                match outcome {
                    // Checks passed: re-read at the top of the loop and merge.
                    WatchOutcome::Ready => {}
                    WatchOutcome::Hard(reason) => {
                        return Gate::Stop(build_stop(
                            branch,
                            number,
                            &live,
                            reason,
                            false,
                            Some("hard_failed"),
                        ));
                    }
                    WatchOutcome::TimedOut => {
                        return Gate::Stop(build_stop(
                            branch,
                            number,
                            &live,
                            MergeRejectionReason::ChecksPending,
                            true,
                            Some("timed_out"),
                        ));
                    }
                    WatchOutcome::Errored(err) => return Gate::Failed(err),
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn merge_stack(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    chain: &[String],
    adopted_merged: &BTreeMap<String, u64>,
    offline: bool,
    keep_branches: bool,
    watch: Option<Watch>,
) -> MergeWalk {
    let mut merged_prs: Vec<MergedPr> = Vec::new();
    let mut reparented_all: Vec<(String, String)> = Vec::new();
    let mut restacked_all: Vec<String> = Vec::new();
    let mut cleaned: Vec<String> = Vec::new();
    let mut cleanup_skipped: Vec<CleanupSkip> = Vec::new();
    let mut stopped: Option<Value> = None;
    // A hard error is deferred, not returned immediately, so the caller can still
    // report whatever already merged before surfacing it.
    let mut loop_err: Option<Error> = None;

    for (i, branch) in chain.iter().enumerate() {
        if let Some(pr) = state.branches.get(branch).and_then(|b| b.pr.clone()) {
            let current = match github.get_pull_request(owner, repo_name, pr.number) {
                Ok(current) => current,
                Err(err) => {
                    loop_err = Some(err.into());
                    break;
                }
            };
            match current.state {
                PrState::Merged => {
                    // Already merged out of band: count it and reconcile it away below.
                    merged_prs.push(MergedPr { branch: branch.clone(), number: pr.number, sha: None, out_of_band: true });
                }
                PrState::Closed => {
                    stopped = Some(json!({ "kind": "closed", "branch": branch, "number": pr.number, "reason": "PR is closed, not merged" }));
                    break;
                }
                PrState::Open => {
                    // Online: the previous iteration's reconcile restacked this branch
                    // onto the merged trunk; force-push it so the PR head is `trunk +
                    // its own commits` before GitHub recomputes readiness. Offline
                    // skips this (no fetch, no push) and leans on GitHub's server-side
                    // 3-way merge, which still squashes only the branch's net diff: a
                    // non-overlapping stack merges clean, an overlapping one reads
                    // not-ready below and the walk stops there.
                    if i > 0 && !offline {
                        if let Err(err) = git.push_force_with_lease(&repo.remote, branch) {
                            loop_err = Some(Error::Usage(format!(
                                "merged the PR(s) below, but could not force-push `{branch}` onto the merged trunk ({err}); run `stacc sync` then `stacc merge` to finish"
                            )));
                            break;
                        }
                    }
                    // Resolve readiness: with `--watch`, a child stopped only on
                    // pending CI is polled until its checks pass and it merges,
                    // rather than stopping for a manual re-run (STA-118). The probe
                    // + retryable classification lives in `await_pr_ready`.
                    match await_pr_ready(github, owner, repo_name, branch, pr.number, watch) {
                        Gate::Ready => {}
                        Gate::Stop(stop) => {
                            stopped = Some(stop);
                            break;
                        }
                        Gate::Failed(err) => {
                            loop_err = Some(err);
                            break;
                        }
                    }
                    match github.merge_pull_request(owner, repo_name, pr.number) {
                        Ok(outcome) if outcome.merged => {
                            merged_prs.push(MergedPr { branch: branch.clone(), number: pr.number, sha: outcome.sha, out_of_band: false });
                        }
                        // 200 but not merged: a clean stop, not a silent drop.
                        Ok(_) => {
                            stopped = Some(json!({ "kind": "did_not_merge", "branch": branch, "number": pr.number, "reason": "GitHub accepted the request but did not merge the PR" }));
                            break;
                        }
                        // No longer mergeable (the head moved since the readiness read).
                        Err(stacc_github::GitHubError::NotMergeable) => {
                            stopped = Some(json!({ "kind": "not_mergeable", "branch": branch, "number": pr.number, "reason": "no longer mergeable (head moved or checks failed)" }));
                            break;
                        }
                        Err(err) => {
                            loop_err = Some(err.into());
                            break;
                        }
                    }
                }
            }
        } else if let Some(&number) = adopted_merged.get(branch) {
            // The adoption pass found this branch's PR already merged out of
            // band (merged PRs are never recorded in state, so it reads as
            // unrecorded here): count it like the `PrState::Merged` arm above
            // and let the reconcile below drop it.
            merged_prs.push(MergedPr { branch: branch.clone(), number, sha: None, out_of_band: true });
        } else {
            // No recorded PR, and the adoption pass found none on GitHub either.
            stopped = Some(json!({ "kind": "no_pr", "branch": branch, "reason": "no PR recorded or found on GitHub; submit it first" }));
            break;
        }

        // Reaching here means `branch` merged (out of band or just now): every
        // non-merging arm above breaks. Snapshot its tip NOW, the lease for the
        // ref cleanup below: a branch the user moves after this point is kept,
        // not destroyed.
        let lease = ref_lease(git, branch);

        // Drop it and restack the remaining stack onto the merged trunk, so the
        // next branch becomes `trunk + its own commits` (force-pushed at the top
        // of the next iteration). Online fetches the trunk; offline restacks
        // against the local trunk.
        let drop: BTreeSet<String> = std::iter::once(branch.clone()).collect();
        // tree_guard = false: merge reconciles via the API, so it needs no
        // squash-merge restack backstop (that is sync's pass).
        match reconcile_with(git, store, state, repo, drop, Vec::new(), offline, false) {
            Ok(outcome) => {
                reparented_all.extend(outcome.reparented);
                restacked_all.extend(outcome.restacked);
            }
            Err(err) => {
                loop_err = Some(err);
                break;
            }
        }

        // The branch is merged and its children are restacked off it: delete
        // its local ref, unless the user asked to keep merged branches.
        if !keep_branches {
            if let Some(lease) = lease {
                let (c, s) = cleanup_merged_refs(git, &repo.trunk, &[lease]);
                cleaned.extend(c);
                cleanup_skipped.extend(s);
            }
        }
    }

    // A branch can be restacked across several iterations (each merge restacks
    // the whole remainder); report each branch once, first-seen order.
    let mut seen_restacked = BTreeSet::new();
    restacked_all.retain(|b| seen_restacked.insert(b.clone()));
    let mut seen_reparented = BTreeSet::new();
    reparented_all.retain(|(b, _)| seen_reparented.insert(b.clone()));

    MergeWalk {
        merged: merged_prs,
        reparented: reparented_all,
        restacked: restacked_all,
        cleaned,
        cleanup_skipped,
        stopped,
        error: loop_err,
    }
}

/// Read a PR's readiness, re-polling briefly while GitHub still reports
/// `mergeable_state` as not-yet-computed (`null`/`unknown`), which it often does
/// right after a base change.
fn poll_pr_ready(
    github: &GitHub,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<stacc_github::PullRequest, Error> {
    let mut pr = github.get_pull_request(owner, repo, number)?;
    let mut tries = 0;
    while matches!(pr.mergeable_state.as_deref(), None | Some("unknown")) && tries < 4 {
        if tries == 0 {
            eprintln!("waiting for GitHub to compute mergeability for #{number}...");
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
        pr = github.get_pull_request(owner, repo, number)?;
        tries += 1;
    }
    Ok(pr)
}

fn report_merge(
    format: OutputFormat,
    merged: &[MergedPr],
    stopped: Option<&Value>,
    protected: bool,
    outcome: Option<&SyncOutcome>,
    cleaned: &[String],
    cleanup_skipped: &[CleanupSkip],
) {
    match format {
        OutputFormat::Json => {
            let merged_json: Vec<Value> = merged
                .iter()
                .map(|m| {
                    json!({ "branch": m.branch, "number": m.number, "sha": m.sha, "out_of_band": m.out_of_band })
                })
                .collect();
            let synced = outcome.map(|o| {
                json!({
                    "dropped": o.merged.iter().cloned().collect::<Vec<_>>(),
                    "reparented": o
                        .reparented
                        .iter()
                        .map(|(name, base)| json!({ "branch": name, "base": base }))
                        .collect::<Vec<_>>(),
                    "restacked": o.restacked,
                })
            });
            super::print_compact(json!({
                "op": "merge",
                "merged": merged_json,
                "stopped_at": stopped,
                "trunk_protected": protected,
                "synced": synced,
                "cleaned": cleaned,
                "cleanup_skipped": cleanup_skipped_json(cleanup_skipped),
                "schema_version": SCHEMA_VERSION,
            }));
        }
        OutputFormat::Pretty => {
            if merged.is_empty() {
                println!("Nothing ready to merge.");
            } else {
                for m in merged {
                    println!("Merged {} (#{})", m.branch, m.number);
                }
            }
            if let Some(stop) = stopped {
                let branch = stop.get("branch").and_then(Value::as_str).unwrap_or("?");
                // Surface the retryable "waiting on CI" case so a human on the
                // default output learns to poll-and-retry (or use --watch) rather
                // than treating every stop as a hard block.
                if stop.get("retryable").and_then(Value::as_bool) == Some(true) {
                    println!("Stopped at {branch}: waiting on CI (re-run merge, or use --watch).");
                } else {
                    println!("Stopped at {branch}: not cleanly mergeable.");
                }
            }
            if let Some(o) = outcome {
                for name in &o.restacked {
                    println!("Restacked {name}");
                }
            }
            report_cleanup_pretty(cleaned, cleanup_skipped);
        }
    }
}

/// The JSON shape of the kept-branch reports: `[{branch, reason}, ...]`.
fn cleanup_skipped_json(skipped: &[CleanupSkip]) -> Vec<Value> {
    skipped
        .iter()
        .map(|s| json!({ "branch": s.branch, "reason": s.reason }))
        .collect()
}

/// Pretty lines for the ref cleanup: one per deleted branch, one per kept
/// branch with its reason.
fn report_cleanup_pretty(cleaned: &[String], skipped: &[CleanupSkip]) {
    for name in cleaned {
        println!("Deleted branch {name}");
    }
    for skip in skipped {
        println!("Kept branch {} ({})", skip.branch, skip.reason);
    }
}

/// `stacc restack`: rebase tracked branches back onto their bases, repairing a
/// drifted stack. Defaults to the current branch and its upstack (`--upstack`
/// makes that explicit); `--only` narrows to the current branch, `--downstack`
/// to the current branch and its ancestors, and `--stack` widens to the whole
/// stack. Unlike `sync`, this is purely local: no fetch, no merge detection.
pub fn restack(args: &RestackArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    // Refuse to start on top of an interrupted operation: a fresh restack would
    // clobber its continuation and rebase into a tree that is already mid-rebase.
    if git.rebase_in_progress() {
        return Err(Error::Usage(
            "a rebase is already in progress; run `stacc continue` to resume it or `stacc abort` to undo".into(),
        ));
    }

    let order = if args.stack {
        ops::topo_order(&state.branches, &repo.trunk)
    } else {
        let current = git.current_branch()?;
        if current == repo.trunk {
            return Err(Error::Usage(format!(
                "on the trunk branch `{}`; check out a stack branch, or pass --stack to restack everything",
                repo.trunk
            )));
        }
        if args.only {
            vec![current]
        } else if args.downstack {
            ops::downstack_chain(&state, &current, &repo.trunk)?
        } else {
            // The default scope; --upstack is its explicit spelling.
            ops::upstack_order(&state.branches, &current)
        }
    };

    let restacked = restack_with_recovery(
        &git,
        &store,
        &mut state,
        &repo,
        &order,
        |remaining| recovery::Operation::Restack { remaining },
        &|_s| {},
    )?;

    // A clean restack leaves no recovery artifacts behind (a prior aborted run
    // may have). Local-only: unlike `sync`, we do not push the state ref.
    clear_conflict_artifacts(&git);

    report_restacked(format, "restack", &restacked);
    Ok(())
}

/// `stacc continue`: resume the operation interrupted by a conflict.
pub fn continue_cmd(format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;
    continue_op(&git, &store, &mut state, &repo, format)
}

/// `stacc abort`: abort the operation interrupted by a conflict, undoing the
/// in-progress rebase and clearing recovery artifacts so the working tree
/// returns to before the operation. Escapes even a corrupt continuation.
pub fn abort_cmd(format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let git_dir = git.git_dir()?;
    // Read the record once: present unless NotInProgress (a Corrupt/Read error
    // still means a file is there, just unreadable).
    let cont = recovery::read_continuation(&git_dir);
    let has_continuation = !matches!(&cont, Err(recovery::RecoveryError::NotInProgress));
    let in_progress = git.rebase_in_progress();
    if !in_progress && !has_continuation {
        return Err(Error::NotInProgress(
            "nothing to abort; no operation in progress".into(),
        ));
    }
    // Only abort a rebase stacc owns. With no continuation, an in-progress
    // rebase belongs to the user (a hand-run `git rebase`); leave it alone.
    if in_progress && !has_continuation {
        return Err(Error::Usage(
            "a non-stacc rebase is in progress; run `git rebase --abort` to undo it".into(),
        ));
    }
    // Abort the rebase, then ALWAYS clear artifacts so a failed `rebase --abort`
    // can't strand the recovery record.
    let abort_err = if in_progress {
        git.rebase_abort().err()
    } else {
        None
    };
    // Undo a modify's amend by resetting its branch to the pre-amend tip, but
    // ONLY when no upstack child has restacked onto the amended tip yet (the
    // conflict landed on the first child, so `remaining` still covers the whole
    // upstack). Resetting after a later-child conflict would orphan the children
    // already rebased onto the amended tip, so there we keep the amend and tell
    // the user. Skip entirely if the rebase abort itself failed.
    if abort_err.is_none() {
        if let Ok(recovery::Operation::Modify {
            branch,
            remaining,
            pre_amend,
        }) = &cont
        {
            let child_count = StateStore::new(git.clone()).load().map_or(0, |s| {
                ops::upstack_order(&s.branches, branch).len().saturating_sub(1)
            });
            if child_count > 0 && remaining.len() == child_count {
                if let Err(err) = git.force_branch(branch, pre_amend) {
                    eprintln!("warning: could not restore `{branch}` to its pre-amend tip: {err}");
                }
            } else {
                eprintln!(
                    "warning: `{branch}` stays amended; upstack branches were already restacked onto it. Run `stacc restack` to finish, or reset `{branch}` to {pre_amend} to undo the amend."
                );
            }
        }
        // Undo a move by rolling the moved branch's recorded base back to
        // `pre_base`, but ONLY when no upstack child has restacked onto the new
        // base yet (the conflict landed on the moved branch itself). Otherwise
        // the children already re-rooted, so keep the move and tell the user.
        if let Ok(recovery::Operation::Move {
            branch,
            remaining,
            pre_base,
        }) = &cont
        {
            let store = StateStore::new(git.clone());
            // Roll the moved branch's base back transactionally, but only when no
            // upstack child has restacked onto the new base yet (the conflict
            // landed on the moved branch itself). The subtree check reads fresh
            // state inside the update so a concurrent change cannot race it.
            let rolled_back = store.update(|state| {
                let subtree = ops::upstack_order(&state.branches, branch).len();
                if remaining.len() == subtree {
                    if let Some(b) = state.branches.get_mut(branch) {
                        b.base.name.clone_from(pre_base);
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            });
            match rolled_back {
                Ok(true) => {}
                Ok(false) => eprintln!(
                    "warning: `{branch}` stays moved; upstack branches were already restacked onto the new base. Run `stacc restack` to finish, or `stacc move --onto {pre_base}` to move it back."
                ),
                Err(err) => eprintln!(
                    "warning: could not restore `{branch}`'s base to `{pre_base}`: {err}; run `stacc move --onto {pre_base}` to roll it back"
                ),
            }
        }
        if let Ok(op @ recovery::Operation::Fold { .. }) = &cont {
            rollback_fold(&git, op);
        }
        if let Ok(op @ recovery::Operation::Reorder { .. }) = &cont {
            rollback_reorder(&git, op);
        }
    }
    clear_conflict_artifacts(&git);
    if let Some(err) = abort_err {
        return Err(err.into());
    }
    match format {
        OutputFormat::Json => println!("{}", json!({ "op": "abort", "aborted": true })),
        OutputFormat::Pretty => println!("Aborted."),
    }
    Ok(())
}

/// Undo an aborted fold: roll the parent's ref back to its pre-fold tip,
/// re-track the folded branch on its old base (with its PR record), and
/// re-point its children back onto it. Unlike move's, this rollback is
/// unconditional: the folded ref is only deleted after a CLEAN restack, so it
/// still names the folded tip, and any child the restack already rebased sits
/// on that tip, exactly its restored base. Nothing is orphaned by rolling back
/// mid-pass. All steps warn rather than fail: `abort`'s job of clearing the
/// rebase already succeeded.
fn rollback_fold(git: &Git, op: &recovery::Operation) {
    let recovery::Operation::Fold {
        branch,
        parent,
        parent_pre_tip,
        branch_base_hash,
        children_pre,
        pr_number,
        pr_url,
        ..
    } = op
    else {
        return;
    };
    // The folded branch's ref still names the folded tip, which is what the
    // parent was fast-forwarded to: use it as the lease so a parent the user
    // has since moved is never clobbered.
    let folded_tip = git
        .ref_commit(&format!("refs/heads/{branch}"))
        .ok()
        .flatten();
    if let Err(err) = git.update_ref(
        &format!("refs/heads/{parent}"),
        parent_pre_tip,
        folded_tip.as_deref(),
    ) {
        eprintln!(
            "warning: could not restore `{parent}` to its pre-fold tip {parent_pre_tip}: {err}; reset it manually"
        );
    }
    let store = StateStore::new(git.clone());
    let restored = store.update(|state| {
        state.branches.insert(
            branch.clone(),
            BranchState {
                base: Base {
                    name: parent.clone(),
                    hash: branch_base_hash.clone(),
                },
                pr: pr_number.map(|number| PullRequest {
                    number,
                    url: pr_url.clone(),
                }),
                pr_title: None,
                pr_description: None,
            },
        );
        for (child, pre_hash) in children_pre {
            if let Some(b) = state.branches.get_mut(child) {
                b.base.name.clone_from(branch);
                b.base.hash.clone_from(pre_hash);
            }
        }
        Ok(())
    });
    if let Err(err) = restored {
        eprintln!(
            "warning: could not restore the pre-fold state ({err}); run `stacc track --base {parent}` on `{branch}` to recover"
        );
    }
}

/// Undo an aborted reorder completely: every chain branch's git ref back to
/// its pre-reorder tip, then every recorded base (name and hash) restored in
/// one transactional write. Unlike move's single-branch guard, this rollback
/// is unconditional: the `pre_state` snapshot describes the entire chain, so
/// it restores correctly no matter which rebase the conflict landed on, even
/// when earlier chain members already rebased (their refs are force-moved
/// back; the pre-mutation worktree guard ensured none lives in another
/// worktree). All steps warn rather than fail: `abort`'s job of clearing the
/// rebase already succeeded.
fn rollback_reorder(git: &Git, op: &recovery::Operation) {
    let recovery::Operation::Reorder { pre_state, .. } = op else {
        return;
    };
    let here = git.current_branch().ok();
    for pre in pre_state {
        let head_ref = format!("refs/heads/{}", pre.branch);
        let live = match git.ref_commit(&head_ref) {
            Ok(live) => live,
            Err(err) => {
                eprintln!(
                    "warning: could not read `{}` to restore it ({err}); reset it to {} manually",
                    pre.branch, pre.tip
                );
                continue;
            }
        };
        if live.as_deref() == Some(pre.tip.as_str()) {
            continue; // never moved (or the aborted rebase already restored it)
        }
        // The checked-out branch needs the index and working tree re-synced
        // too; the aborted rebase left a clean tree, so a hard reset is safe.
        // Everything else is a plain ref move, leased on the tip we just read.
        let restored = if here.as_deref() == Some(pre.branch.as_str()) {
            git.reset_hard(&pre.tip)
        } else {
            git.update_ref(&head_ref, &pre.tip, live.as_deref())
        };
        if let Err(err) = restored {
            eprintln!(
                "warning: could not restore `{}` to its pre-reorder tip {} ({err}); reset it manually",
                pre.branch, pre.tip
            );
        }
    }
    let store = StateStore::new(git.clone());
    let restored = store.update(|state| {
        for pre in pre_state {
            if let Some(b) = state.branches.get_mut(&pre.branch) {
                b.base.name.clone_from(&pre.base_name);
                b.base.hash.clone_from(&pre.base_hash);
            }
        }
        Ok(())
    });
    if let Err(err) = restored {
        eprintln!(
            "warning: could not restore the pre-reorder bases ({err}); run `stacc undo` to recover"
        );
    }
}

/// `stacc undo`: revert the most recent stacc mutation(s) by restoring a prior
/// version of the stack state and the affected branch tips. `--steps N` walks N
/// versions back (default 1). The restore is appended as a new version, so undo
/// is itself undoable. Non-interactive and JSON-complete.
pub fn undo(args: &UndoArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let store = StateStore::new(git.clone());

    // Resolve the target version up front (by hash), so a concurrent write cannot
    // retarget us mid-undo.
    let target = match store.version_back(args.steps) {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            return Err(Error::Usage(format!(
                "nothing to undo {} version(s) back; the recorded history does not reach that far",
                args.steps
            )))
        }
        Err(StateError::BeyondRetention { bound }) => {
            return Err(Error::Usage(format!(
                "cannot undo {} versions back; stacc retains only the last {bound} versions",
                args.steps
            )))
        }
        Err(err) => return Err(err.into()),
    };

    let target_state = store.load_version(&target)?;
    let target_tips = store.tips_at(&target)?;
    let here = git.current_branch().ok();

    let mut restored: Vec<String> = Vec::new();
    let mut worktree_skipped: Vec<(String, String)> = Vec::new();
    let mut dirty_skipped: Vec<String> = Vec::new();
    let mut moved_skipped: Vec<String> = Vec::new();

    // Restore each branch's tip to the target version before writing the restored
    // state, so the new version's tip snapshot reflects what was actually restored.
    for (branch, target_tip) in &target_tips {
        let head_ref = format!("refs/heads/{branch}");
        let live = git.ref_commit(&head_ref)?;
        if live.as_deref() == Some(target_tip.as_str()) {
            continue; // already at the target tip
        }
        if let Some(wt) = git.branch_checked_out_elsewhere(branch)? {
            worktree_skipped.push((branch.clone(), wt.to_string_lossy().into_owned()));
            continue;
        }
        if here.as_deref() == Some(branch.as_str()) {
            // Checked out here: sync the working tree too, unless it is dirty (a
            // hard reset would discard those changes).
            if git.has_uncommitted_changes()? {
                dirty_skipped.push(branch.clone());
                continue;
            }
            git.reset_hard(target_tip)?;
            restored.push(branch.clone());
        } else {
            // Not checked out anywhere: a leased ref move; skip if it moved under
            // us since we read its tip.
            match git.update_ref(&head_ref, target_tip, live.as_deref()) {
                Ok(()) => restored.push(branch.clone()),
                Err(_) => moved_skipped.push(branch.clone()),
            }
        }
    }

    // Append the restored state as a new forward version (undo is itself undoable).
    store.save(&target_state)?;

    report_undo(
        format,
        args.steps,
        &restored,
        &worktree_skipped,
        &dirty_skipped,
        &moved_skipped,
    );
    Ok(())
}

fn report_undo(
    format: OutputFormat,
    steps: usize,
    restored: &[String],
    worktree_skipped: &[(String, String)],
    dirty_skipped: &[String],
    moved_skipped: &[String],
) {
    match format {
        OutputFormat::Json => {
            let worktree: Vec<Value> = worktree_skipped
                .iter()
                .map(|(b, wt)| json!({ "branch": b, "worktree": wt }))
                .collect();
            println!(
                "{}",
                json!({
                    "op": "undo",
                    "steps": steps,
                    "restored": restored,
                    "worktree_skipped": worktree,
                    "dirty_skipped": dirty_skipped,
                    "moved_skipped": moved_skipped,
                })
            );
        }
        OutputFormat::Pretty => {
            if restored.is_empty()
                && worktree_skipped.is_empty()
                && dirty_skipped.is_empty()
                && moved_skipped.is_empty()
            {
                println!("Undid {steps} version(s); no branch tips needed restoring.");
                return;
            }
            for b in restored {
                println!("Restored {b}");
            }
            for (b, wt) in worktree_skipped {
                println!("Skipped {b} (checked out in {wt})");
            }
            for b in dirty_skipped {
                println!("Skipped {b} (uncommitted changes; commit or stash, then re-run)");
            }
            for b in moved_skipped {
                println!("Skipped {b} (tip moved since)");
            }
        }
    }
}

/// Resume the operation recorded in the continuation: finish the conflicting
/// rebase, then drain the remaining queue. The recorded [`recovery::Operation`]
/// drives the output shape and whether the state ref is pushed, so this resumes
/// whatever was in flight (sync, restack, ...) regardless of how it was invoked.
// A cohesive resume sequence: validate the in-progress rebase, finish it, record
// the resumed delta, drain the queue, then report per operation kind.
#[allow(clippy::too_many_lines)]
fn continue_op(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    format: OutputFormat,
) -> Result<(), Error> {
    let op = match recovery::read_continuation(&git.git_dir()?) {
        Ok(op) => op,
        // A rebase with no stacc record is the user's own; point them at it.
        Err(recovery::RecoveryError::NotInProgress) if git.rebase_in_progress() => {
            return Err(Error::Usage(
                "a rebase is in progress but stacc has no record of it; run `stacc abort` to clear it".into(),
            ));
        }
        Err(err) => return Err(err.into()),
    };
    // A recorded operation always coexists with an in-progress rebase. If the
    // rebase is gone, the record is stale (resolved or aborted out of band):
    // clear it rather than handing `git rebase --continue` a raw error.
    if !git.rebase_in_progress() {
        clear_conflict_artifacts(git);
        return Err(Error::Usage(
            "no rebase in progress; cleared a stale stacc continuation".into(),
        ));
    }
    let remaining = op.remaining().to_vec();

    // The continuation stores only branch names; confirm the rebase git is
    // actually mid-replay is the one we recorded, so a stale record or an
    // out-of-band rebase can't make us advance the wrong branch's base hash.
    if let Some(expected) = remaining.first() {
        match git.rebase_head_branch() {
            Some(head) if &head == expected => {}
            Some(head) => {
                return Err(Error::Usage(format!(
                    "the in-progress rebase is on `{head}`, not the recorded `{expected}`; resolve it with `git rebase --continue`/`--abort` first"
                )));
            }
            // stacc only ever runs merge-style rebases (head-name present), so an
            // unreadable head means this rebase is not ours: refuse to advance.
            None => {
                return Err(Error::Usage(format!(
                    "a rebase is in progress but stacc cannot confirm it is the recorded `{expected}`; run `git rebase --abort` if it is not stacc's"
                )));
            }
        }
    }

    match git.rebase_continue() {
        Ok(()) => {}
        Err(RebaseError::Interrupt(_)) => {
            // Still conflicting on the same branch; the artifacts stand.
            let branch = remaining.first().cloned().unwrap_or_default();
            return Err(Error::Conflict { branch });
        }
        Err(RebaseError::Git(err)) => return Err(err.into()),
    }

    // The conflicting branch's rebase just completed: record its new base hash.
    let mut restacked: Vec<String> = Vec::new();
    let mut resumed_delta: Option<(String, String)> = None;
    if let Some(first) = remaining.first() {
        if let Some(base_name) = state.branches.get(first).map(|b| b.base.name.clone()) {
            let base_tip = git.rev_parse(&base_name)?;
            if let Some(b) = state.branches.get_mut(first) {
                b.base.hash.clone_from(&base_tip);
            }
            resumed_delta = Some((first.clone(), base_tip));
        }
        restacked.push(first.clone());
    }

    // The resumed branch's new base.hash is this command's own state change; the
    // helper re-applies it alongside the rest of the queue's engine deltas.
    let apply_resumed = move |s: &mut State| {
        if let Some((branch, hash)) = &resumed_delta {
            if let Some(b) = s.branches.get_mut(branch) {
                b.base.hash.clone_from(hash);
            }
        }
    };

    // A resumed reorder must keep force-rebasing the chain members still in the
    // queue: a reordered branch can already descend its new base's tip (the
    // flatten case) and the plain skip check would wrongly leave it unmoved.
    let force: BTreeSet<String> = match &op {
        recovery::Operation::Reorder { order, .. } => order.iter().cloned().collect(),
        _ => BTreeSet::new(),
    };
    // A resumed sync keeps sync's tree-identical guard on for the rest of the
    // queue, so a squash-merged branch later in the chain is skipped, not
    // rebased into the phantom conflict the guard exists to prevent.
    let tree_guard = matches!(op, recovery::Operation::Sync { .. });

    let rest: Vec<String> = remaining.into_iter().skip(1).collect();
    restacked.extend(restack_with_recovery_forced(
        git,
        store,
        state,
        repo,
        &rest,
        |r| op.with_remaining(r),
        &apply_resumed,
        &force,
        tree_guard,
    )?);

    clear_conflict_artifacts(git);
    if op.pushes_state() {
        if let Err(err) = store.push(&repo.remote) {
            eprintln!("warning: could not push state to `{}`: {err}", repo.remote);
        }
    }

    match &op {
        recovery::Operation::Sync { .. } => {
            // A resumed sync only finishes the interrupted restack; it runs no
            // detection, so the offline detection-skipped marker stays false.
            report_sync(
                format,
                op.tag(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &[],
                &[],
                &restacked,
                &[],
                &[],
                false,
                &[],
                &[],
            );
        }
        // A resumed modify carries its branch, so JSON gets the same
        // {op,branch,sha,restacked} shape as the direct command, minus `amended`
        // (the continuation does not record the amend-vs-append choice). Pretty
        // uses the shared restacked output.
        recovery::Operation::Modify { branch, .. } if matches!(format, OutputFormat::Json) => {
            let tip = git.rev_parse(branch).unwrap_or_default();
            println!(
                "{}",
                json!({ "op": "modify", "branch": branch, "sha": tip, "restacked": restacked })
            );
        }
        // A resumed fold still owes its ref surgery (switch to the parent,
        // delete the folded ref) and any requested PR close; the direct path
        // runs these only after a clean first pass. Reports the direct
        // command's {op,branch,into,restacked,pr_closed} shape.
        recovery::Operation::Fold {
            branch,
            parent,
            close,
            pr_number,
            ..
        } => {
            finish_fold_refs(git, branch, parent);
            let pr_closed = if *close {
                close_pr_best_effort(git, repo, branch, *pr_number)
            } else {
                None
            };
            report_fold(format, branch, parent, &restacked, pr_closed, parent == &repo.trunk);
        }
        // A resumed reorder reports the direct command's {op,order,restacked}
        // shape (pretty uses the shared restacked output).
        recovery::Operation::Reorder { order, .. } if matches!(format, OutputFormat::Json) => {
            println!(
                "{}",
                json!({ "op": "reorder", "order": order, "restacked": restacked })
            );
        }
        // A resumed move reports the same {op,branch,base,restacked} shape as the
        // direct command (pretty uses the shared restacked output).
        recovery::Operation::Move { branch, .. } if matches!(format, OutputFormat::Json) => {
            let base = state
                .branches
                .get(branch)
                .map_or("", |b| b.base.name.as_str());
            let sha = git.rev_parse(branch).unwrap_or_default();
            println!(
                "{}",
                json!({ "op": "move", "branch": branch, "base": base, "sha": sha, "restacked": restacked })
            );
        }
        _ => report_restacked(format, op.tag(), &restacked),
    }
    Ok(())
}

/// Run the engine's [`ops::restack`], persisting recovery artifacts on a
/// conflict: the typed [`recovery::Operation`] continuation (built by `make_op`
/// from the unfinished queue) plus the GitHub-enriched conflict-context file.
/// The context writer stays in the CLI crate so `stacc-core` stays off
/// `stacc-github`.
pub(crate) fn restack_with_recovery(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    order: &[String],
    make_op: impl Fn(Vec<String>) -> recovery::Operation,
    command_deltas: &dyn Fn(&mut State),
) -> Result<Vec<String>, Error> {
    restack_with_recovery_forced(
        git,
        store,
        state,
        repo,
        order,
        make_op,
        command_deltas,
        &BTreeSet::new(),
        false,
    )
}

/// [`restack_with_recovery`] with a `force` set of branches that must rebase
/// even when they already descend their base's tip (the engine's
/// [`ops::restack_forced`]). `reorder` passes its chain here; everything else
/// goes through the plain wrapper.
// One extra argument over the shared recovery path; a params struct for a
// single forced caller would only add indirection.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restack_with_recovery_forced(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    order: &[String],
    make_op: impl Fn(Vec<String>) -> recovery::Operation,
    command_deltas: &dyn Fn(&mut State),
    force: &BTreeSet<String>,
    tree_guard: bool,
) -> Result<Vec<String>, Error> {
    let mut applied: Vec<(String, String)> = Vec::new();
    match ops::restack_forced(git, state, order, &mut applied, force, tree_guard) {
        Ok(outcome) => {
            persist_restack(store, command_deltas, &applied)?;
            if !outcome.skipped.is_empty() {
                eprintln!(
                    "warning: skipped {} branch(es) with no git ref: {}. Remove them with `stacc untrack <branch>`.",
                    outcome.skipped.len(),
                    outcome.skipped.join(", ")
                );
            }
            if !outcome.worktree_skipped.is_empty() {
                let list = outcome
                    .worktree_skipped
                    .iter()
                    .map(|(b, wt)| format!("{b} ({wt})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "warning: skipped {} branch(es) checked out in another worktree: {list}. Restack them from there, or finish and remove the worktree.",
                    outcome.worktree_skipped.len()
                );
            }
            if !outcome.tree_identical_skipped.is_empty() {
                eprintln!(
                    "note: skipped {} branch(es) whose tree already matches their base (they look squash-merged): {}. Run `stacc sync` online to confirm and clean up, or `stacc merged <branch>` to reconcile one locally.",
                    outcome.tree_identical_skipped.len(),
                    outcome.tree_identical_skipped.join(", ")
                );
            }
            Ok(outcome.restacked)
        }
        Err(ops::OpsError::Conflict { branch, remaining }) => {
            // The engine no longer saves; the CLI owns persistence. Write the
            // agent-readable context and the resume marker BEFORE the
            // transactional save, so a contention failure on the save can abort
            // to a clean tree rather than strand the user mid-rebase with no
            // `stacc continue`.
            write_conflict_context(git, state, repo, &branch);
            let dir = git.git_dir()?;
            if let Err(err) = recovery::write_continuation(&dir, &make_op(remaining)) {
                return Err(abort_and_report(
                    git,
                    &branch,
                    &format!("the recovery state could not be saved ({err})"),
                ));
            }
            // Persist the partial progress transactionally. On failure (e.g.
            // contention from a concurrent writer), abort the rebase and clear
            // artifacts so we never leave an in-progress rebase the user cannot
            // reconcile, mirroring the failed-marker-write guard above.
            if let Err(err) = persist_restack(store, command_deltas, &applied) {
                return Err(abort_and_report(
                    git,
                    &branch,
                    &format!("persisting recovery state failed ({err})"),
                ));
            }
            Err(Error::Conflict { branch })
        }
        Err(err) => Err(err.into()),
    }
}

/// Abort the in-progress rebase and clear recovery artifacts, then build the
/// usage error explaining that a conflict could not be made resumable (`reason`)
/// and how the fallback went. Shared by the conflict path's two
/// cannot-persist-recovery guards so the abort-to-clean-tree policy lives once.
fn abort_and_report(git: &Git, branch: &str, reason: &str) -> Error {
    let aborted = git.rebase_abort();
    clear_conflict_artifacts(git);
    Error::Usage(match aborted {
        Ok(()) => {
            format!("conflict on `{branch}`, but {reason}; rebase aborted to a clean tree")
        }
        Err(abort_err) => format!(
            "conflict on `{branch}`, but {reason} and the rebase abort also failed ({abort_err}); run `git rebase --abort` manually"
        ),
    })
}

/// Refuse a focused operation when any branch it would rewrite is checked out in
/// another worktree, naming the first such branch. Bulk passes (`restack`,
/// `sync`) skip these per-branch in the engine and report them; focused ops
/// (`modify`, `move`) fail fast here so the user finishes or relocates that
/// branch first instead of getting a partial result.
pub(crate) fn guard_worktree(git: &Git, branches: &[String]) -> Result<(), Error> {
    for branch in branches {
        if let Some(wt) = git.branch_checked_out_elsewhere(branch)? {
            return Err(Error::WorktreeConflict {
                branch: branch.clone(),
                worktree: wt.to_string_lossy().into_owned(),
            });
        }
    }
    Ok(())
}

/// A merged branch whose local ref was kept during cleanup, and why.
struct CleanupSkip {
    branch: String,
    reason: String,
}

/// What deleting one merged branch's local ref did.
enum RefCleanup {
    /// The ref was deleted.
    Cleaned,
    /// The ref was already gone; nothing to clean, nothing to report.
    AlreadyGone,
    /// The ref was kept, with the reason to report.
    Skipped(String),
}

/// Delete the local ref of a branch whose PR merged, leased on `tip` (the tip
/// the branch had when the merge happened or was detected): the delete is a
/// compare-and-swap on that tip, so a ref that moved since is kept and
/// reported rather than destroyed. A branch checked out in another worktree is
/// kept (deleting it would desync that worktree); a branch checked out HERE is
/// released by switching to the trunk first, so HEAD never names a deleted ref.
fn cleanup_merged_ref(git: &Git, trunk: &str, branch: &str, tip: &str) -> RefCleanup {
    match git.branch_checked_out_elsewhere(branch) {
        Ok(Some(wt)) => {
            return RefCleanup::Skipped(format!("checked out in {}", wt.display()));
        }
        Ok(None) => {}
        Err(err) => return RefCleanup::Skipped(format!("could not list worktrees ({err})")),
    }
    let head_ref = format!("refs/heads/{branch}");
    match git.ref_commit(&head_ref) {
        Ok(None) => return RefCleanup::AlreadyGone,
        Ok(Some(now)) if now != tip => {
            return RefCleanup::Skipped("moved since its PR merged".into());
        }
        Ok(Some(_)) => {}
        Err(err) => return RefCleanup::Skipped(format!("could not read its ref ({err})")),
    }
    if git.current_branch().ok().as_deref() == Some(branch) {
        if let Err(err) = git.checkout(trunk) {
            return RefCleanup::Skipped(format!(
                "checked out here and switching to `{trunk}` failed ({err})"
            ));
        }
    }
    match git.delete_ref(&head_ref, Some(tip)) {
        Ok(()) => RefCleanup::Cleaned,
        // The CAS catches a move between the read above and the delete.
        Err(_) => RefCleanup::Skipped("moved since its PR merged".into()),
    }
}

/// Run [`cleanup_merged_ref`] over `(branch, tip)` leases, splitting the
/// results into the deleted branches and the kept-with-reason ones.
fn cleanup_merged_refs(
    git: &Git,
    trunk: &str,
    leases: &[(String, String)],
) -> (Vec<String>, Vec<CleanupSkip>) {
    let mut cleaned = Vec::new();
    let mut skipped = Vec::new();
    for (branch, tip) in leases {
        match cleanup_merged_ref(git, trunk, branch, tip) {
            RefCleanup::Cleaned => cleaned.push(branch.clone()),
            RefCleanup::AlreadyGone => {}
            RefCleanup::Skipped(reason) => skipped.push(CleanupSkip {
                branch: branch.clone(),
                reason,
            }),
        }
    }
    (cleaned, skipped)
}

/// The lease for [`cleanup_merged_ref`]: `branch`'s live local tip, or `None`
/// when the ref is already gone (nothing to clean).
fn ref_lease(git: &Git, branch: &str) -> Option<(String, String)> {
    git.ref_commit(&format!("refs/heads/{branch}"))
        .ok()
        .flatten()
        .map(|tip| (branch.to_string(), tip))
}

/// Persist a restack-driven command transactionally: apply the command's own
/// state change (`command_deltas`) plus the engine's `(branch, base_hash)`
/// updates to fresh state under compare-and-swap, so a concurrent writer on a
/// different branch is re-applied onto rather than clobbered.
fn persist_restack(
    store: &StateStore,
    command_deltas: &dyn Fn(&mut State),
    applied: &[(String, String)],
) -> Result<(), Error> {
    store.update(|s| {
        command_deltas(s);
        for (branch, hash) in applied {
            if let Some(b) = s.branches.get_mut(branch) {
                b.base.hash.clone_from(hash);
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// Push the state ref (best-effort) and clear any conflict artifacts.
fn finish_sync(git: &Git, store: &StateStore, repo: &RepoConfig) {
    if let Err(err) = store.push(&repo.remote) {
        eprintln!("warning: could not push state to `{}`: {err}", repo.remote);
    }
    clear_conflict_artifacts(git);
}

/// Output for a restack-shaped result (`restack`, or a resumed restack/modify/
/// move continuation): the operation tag and the branches that were rebased.
fn report_restacked(format: OutputFormat, op: &str, restacked: &[String]) {
    match format {
        OutputFormat::Json => println!("{}", json!({ "op": op, "restacked": restacked })),
        OutputFormat::Pretty => {
            if restacked.is_empty() {
                println!("Already up to date.");
            } else {
                for name in restacked {
                    println!("Restacked {name}");
                }
            }
        }
    }
}

/// The local merge-equivalence signal as a stable JSON/pretty label.
fn evidence_str(e: MergeEquivalence) -> &'static str {
    match e {
        MergeEquivalence::Ancestor => "ancestor",
        MergeEquivalence::SameTree => "same_tree",
        MergeEquivalence::NetDiff => "net_diff",
        MergeEquivalence::NotFound => "not_found",
    }
}

#[allow(clippy::too_many_arguments)]
fn report_sync(
    format: OutputFormat,
    op: &str,
    merged: &BTreeSet<String>,
    pruned: &BTreeSet<String>,
    adopted: &[AdoptedPr],
    reparented: &[(String, String)],
    restacked: &[String],
    cleaned: &[String],
    cleanup_skipped: &[CleanupSkip],
    detection_skipped: bool,
    likely_merged: &[ops::LikelyMerged],
    untracked: &[String],
) {
    match format {
        OutputFormat::Json => {
            let merged_list: Vec<&String> = merged.iter().collect();
            let pruned_list: Vec<&String> = pruned.iter().collect();
            let adopted_list: Vec<Value> = adopted
                .iter()
                .map(|a| json!({ "branch": a.branch, "number": a.number, "url": a.url }))
                .collect();
            let reparented_list: Vec<Value> = reparented
                .iter()
                .map(|(branch, base)| json!({ "branch": branch, "base": base }))
                .collect();
            let likely_list: Vec<Value> = likely_merged
                .iter()
                .map(|l| json!({ "branch": l.branch, "evidence": evidence_str(l.evidence) }))
                .collect();
            println!(
                "{}",
                json!({
                    "op": op,
                    "merged": merged_list,
                    "pruned": pruned_list,
                    "adopted": adopted_list,
                    "reparented": reparented_list,
                    "restacked": restacked,
                    "cleaned": cleaned,
                    "cleanup_skipped": cleanup_skipped_json(cleanup_skipped),
                    "detection_skipped": detection_skipped,
                    "likely_merged": likely_list,
                    "untracked": untracked,
                    "schema_version": SCHEMA_VERSION,
                })
            );
        }
        OutputFormat::Pretty => {
            if merged.is_empty()
                && pruned.is_empty()
                && adopted.is_empty()
                && reparented.is_empty()
                && restacked.is_empty()
                && likely_merged.is_empty()
            {
                println!("Already up to date.");
            } else {
                for name in merged {
                    println!("Merged, untracked: {name}");
                }
                for name in pruned {
                    println!("Pruned (no git ref): {name}");
                }
                for a in adopted {
                    println!("Adopted PR #{} for {}: {}", a.number, a.branch, a.url);
                }
                for (name, base) in reparented {
                    println!("Re-parented {name} -> {base}");
                }
                for name in restacked {
                    println!("Restacked {name}");
                }
                for l in likely_merged {
                    println!(
                        "Likely merged: {} ({}); run `stacc merged {}` to reconcile",
                        l.branch,
                        evidence_str(l.evidence),
                        l.branch
                    );
                }
            }
            if !untracked.is_empty() {
                println!(
                    "hint: {} local branch(es) are not tracked by stacc; run `stacc track <branch>` to add or `stacc sync --no-interactive` to see them in JSON output.",
                    untracked.len()
                );
            }
            report_cleanup_pretty(cleaned, cleanup_skipped);
        }
    }
}

pub(crate) fn clear_conflict_artifacts(git: &Git) {
    if let Ok(dir) = git.git_dir() {
        recovery::clear_continuation(&dir);
        let _ = std::fs::remove_file(dir.join("stacc-conflict-context.json"));
    }
}

/// Infer the most appropriate base for an untracked branch by finding the
/// "deepest" tracked branch that is a git ancestor of `branch`. Falls back to
/// `trunk` when no tracked branch is an ancestor.
fn infer_base(
    git: &Git,
    tracked: &BTreeMap<String, BranchState>,
    trunk: &str,
    branch: &str,
) -> String {
    let ancestors: Vec<&str> = tracked
        .keys()
        .filter(|b| git.is_ancestor(b.as_str(), branch).unwrap_or(false))
        .map(String::as_str)
        .collect();

    if ancestors.is_empty() {
        return trunk.to_owned();
    }

    // Deepest ancestor: the candidate that has all others as its own ancestors.
    // (Uniquely exists when the ancestor set is a linear chain; in a fork any
    // leaf qualifies and we take the first alphabetically via BTreeMap order.)
    ancestors
        .iter()
        .find(|&&candidate| {
            ancestors
                .iter()
                .all(|&other| other == candidate || git.is_ancestor(other, candidate).unwrap_or(false))
        })
        .copied()
        .unwrap_or(trunk)
        .to_owned()
}

#[cfg(test)]
mod infer_base_tests {
    use std::collections::BTreeMap;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success();
        assert!(ok, "git {args:?} failed");
    }

    fn setup_repo() -> (TempDir, Git) {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        git(p, &["init", "-b", "main"]);
        git(p, &["config", "user.email", "test@test.com"]);
        git(p, &["config", "user.name", "Test"]);
        git(p, &["commit", "--allow-empty", "-m", "root"]);
        let g = Git::open(p);
        (dir, g)
    }

    fn make_branch_state(name: &str) -> BranchState {
        BranchState {
            base: Base { name: name.to_owned(), hash: String::new() },
            pr: None,
            pr_title: None,
            pr_description: None,
        }
    }

    #[test]
    fn falls_back_to_trunk_when_no_tracked_branch_is_ancestor() {
        let (dir, git_repo) = setup_repo();
        let p = dir.path();
        // Create a branch that diverges from main; "feature-a" does not exist as a ref.
        git(p, &["checkout", "-b", "unrelated"]);
        git(p, &["commit", "--allow-empty", "-m", "u1"]);

        let mut tracked = BTreeMap::new();
        tracked.insert("feature-a".to_owned(), make_branch_state("main"));

        let result = infer_base(&git_repo, &tracked, "main", "unrelated");
        assert_eq!(result, "main");
    }

    #[test]
    fn picks_nearest_tracked_ancestor_in_a_linear_stack() {
        let (dir, git_repo) = setup_repo();
        let p = dir.path();
        // main -> a -> b -> untracked
        git(p, &["checkout", "-b", "a"]);
        git(p, &["commit", "--allow-empty", "-m", "a1"]);
        git(p, &["checkout", "-b", "b"]);
        git(p, &["commit", "--allow-empty", "-m", "b1"]);
        git(p, &["checkout", "-b", "untracked"]);
        git(p, &["commit", "--allow-empty", "-m", "u1"]);
        git(p, &["checkout", "main"]);

        let mut tracked = BTreeMap::new();
        tracked.insert("a".to_owned(), make_branch_state("main"));
        tracked.insert("b".to_owned(), make_branch_state("a"));

        let result = infer_base(&git_repo, &tracked, "main", "untracked");
        assert_eq!(result, "b");
    }

    #[test]
    fn returns_trunk_when_tracked_map_is_empty() {
        let (_dir, git_repo) = setup_repo();
        let tracked = BTreeMap::new();
        let result = infer_base(&git_repo, &tracked, "main", "main");
        assert_eq!(result, "main");
    }
}

/// Best-effort: write the conflict context for an agent to read and resolve.
fn write_conflict_context(git: &Git, state: &State, repo: &RepoConfig, branch: &str) {
    let base = state
        .branches
        .get(branch)
        .map(|b| b.base.name.clone())
        .unwrap_or_default();
    let conflicted = git.conflicted_files().unwrap_or_default();
    let base_pr = fetch_base_pr(git, repo, state, &base).unwrap_or(Value::Null);
    let context = json!({
        "branch": branch,
        "base": base,
        "conflicted_files": conflicted,
        "base_pr": base_pr,
    });
    if let Ok(dir) = git.git_dir() {
        let _ = std::fs::write(
            dir.join("stacc-conflict-context.json"),
            serde_json::to_string_pretty(&context).unwrap_or_default(),
        );
    }
}

/// The base branch's PR (number/title/body), if it has one. `None` on any
/// failure, the context is best-effort.
fn fetch_base_pr(git: &Git, repo: &RepoConfig, state: &State, base: &str) -> Option<Value> {
    let number = state.branches.get(base)?.pr.as_ref()?.number;
    let (owner, name) = stacc_github::parse_remote(&git.remote_url(&repo.remote).ok()?)?;
    let pr = GitHub::from_env().ok()?.get_pull_request(&owner, &name, number).ok()?;
    Some(json!({ "number": pr.number, "title": pr.title, "body": pr.body }))
}

/// Fetch the trunk from `remote` and fast-forward the local trunk to it.
fn fast_forward_trunk(git: &Git, remote: &str, trunk: &str) -> Result<(), Error> {
    git.fetch(remote, trunk)?;
    let remote_tip = git.rev_parse(&format!("{remote}/{trunk}"))?;
    let local_tip = git.rev_parse(trunk)?;
    if local_tip != remote_tip && git.is_ancestor(&local_tip, &remote_tip)? {
        git.update_ref(
            &format!("refs/heads/{trunk}"),
            &remote_tip,
            Some(local_tip.as_str()),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    // STA-118: the `--watch` wait loop. Driven by closures so it is exercised
    // without real time, network, or a wall clock (`never_expired`); `no_sleep`
    // makes the poll backstop the only bound.
    fn no_sleep(_: std::time::Duration) {}
    fn never_expired() -> bool {
        false
    }

    #[test]
    fn watch_merges_as_soon_as_the_change_is_ready() {
        let samples = std::cell::RefCell::new(
            vec![WatchStep::Waiting, WatchStep::Waiting, WatchStep::Merge].into_iter(),
        );
        let outcome = run_watch(
            || samples.borrow_mut().next().expect("poll past samples"),
            no_sleep,
            never_expired,
            std::time::Duration::ZERO,
            10,
        );
        assert!(matches!(outcome, WatchOutcome::Ready));
    }

    #[test]
    fn watch_stops_hard_when_a_check_fails() {
        let samples = std::cell::RefCell::new(
            vec![
                WatchStep::Waiting,
                WatchStep::Hard(MergeRejectionReason::Blocked),
            ]
            .into_iter(),
        );
        let outcome = run_watch(
            || samples.borrow_mut().next().expect("poll past samples"),
            no_sleep,
            never_expired,
            std::time::Duration::ZERO,
            10,
        );
        assert!(matches!(
            outcome,
            WatchOutcome::Hard(MergeRejectionReason::Blocked)
        ));
    }

    #[test]
    fn watch_times_out_on_the_poll_backstop_while_waiting() {
        // Always waiting and the clock never expires: the poll backstop (2) ends it.
        let outcome = run_watch(
            || WatchStep::Waiting,
            no_sleep,
            never_expired,
            std::time::Duration::ZERO,
            2,
        );
        assert!(matches!(outcome, WatchOutcome::TimedOut));
    }

    #[test]
    fn watch_times_out_when_the_deadline_passes() {
        // The wall-clock deadline ends it even with poll budget remaining.
        let outcome = run_watch(
            || WatchStep::Waiting,
            no_sleep,
            || true,
            std::time::Duration::ZERO,
            1000,
        );
        assert!(matches!(outcome, WatchOutcome::TimedOut));
    }

    #[test]
    fn watch_aborts_on_a_permanent_error() {
        let samples = std::cell::RefCell::new(
            vec![
                WatchStep::Waiting,
                WatchStep::Permanent(Error::Usage("token revoked".into())),
            ]
            .into_iter(),
        );
        let outcome = run_watch(
            || samples.borrow_mut().next().expect("poll past samples"),
            no_sleep,
            never_expired,
            std::time::Duration::ZERO,
            10,
        );
        assert!(matches!(outcome, WatchOutcome::Errored(_)));
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn repo() -> (tempfile::TempDir, Git) {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        run_git(tmp.path(), &["config", "user.name", "Test"]);
        run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
        let git = Git::open(tmp.path());
        (tmp, git)
    }

    #[test]
    fn cleanup_deletes_a_ref_at_its_leased_tip() {
        let (tmp, git) = repo();
        run_git(tmp.path(), &["branch", "feat"]);
        let tip = git.rev_parse("feat").unwrap();
        assert!(matches!(
            cleanup_merged_ref(&git, "main", "feat", &tip),
            RefCleanup::Cleaned
        ));
        assert!(git.ref_missing("feat"));
    }

    #[test]
    fn cleanup_skips_a_ref_that_moved_off_the_lease() {
        let (tmp, git) = repo();
        run_git(tmp.path(), &["branch", "feat"]);
        let lease = git.rev_parse("feat").unwrap();
        // The branch moves after the lease was taken (the user committed to it
        // mid-command): the cleanup must keep it.
        run_git(tmp.path(), &["checkout", "-q", "feat"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "moved"]);
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        match cleanup_merged_ref(&git, "main", "feat", &lease) {
            RefCleanup::Skipped(reason) => assert!(reason.contains("moved"), "got: {reason}"),
            _ => panic!("expected a lease-miss skip"),
        }
        assert!(!git.ref_missing("feat"), "the moved ref must survive");
    }

    #[test]
    fn cleanup_releases_the_current_branch_via_the_trunk() {
        let (tmp, git) = repo();
        run_git(tmp.path(), &["checkout", "-q", "-b", "feat"]);
        let tip = git.rev_parse("feat").unwrap();
        assert!(matches!(
            cleanup_merged_ref(&git, "main", "feat", &tip),
            RefCleanup::Cleaned
        ));
        assert_eq!(git.current_branch().unwrap(), "main");
        assert!(git.ref_missing("feat"));
    }

    #[test]
    fn cleanup_skips_a_branch_checked_out_in_another_worktree() {
        let (tmp, git) = repo();
        run_git(tmp.path(), &["branch", "feat"]);
        let tip = git.rev_parse("feat").unwrap();
        let wt = tmp.path().join("wt");
        run_git(
            tmp.path(),
            &["worktree", "add", "-q", wt.to_str().unwrap(), "feat"],
        );
        match cleanup_merged_ref(&git, "main", "feat", &tip) {
            RefCleanup::Skipped(reason) => {
                assert!(reason.contains("checked out in"), "got: {reason}");
            }
            _ => panic!("expected a worktree skip"),
        }
        assert!(!git.ref_missing("feat"), "the checked-out ref must survive");
    }

    #[test]
    fn cleanup_is_silent_for_an_already_gone_ref() {
        let (_tmp, git) = repo();
        assert!(matches!(
            cleanup_merged_ref(&git, "main", "ghost", "0000000000000000000000000000000000000000"),
            RefCleanup::AlreadyGone
        ));
    }
}
