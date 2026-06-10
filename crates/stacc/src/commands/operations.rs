//! Stack operations: sync, restack, modify, move, and the conflict-recovery
//! (continue/abort) lifecycle they share.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use stacc_core::{ops, recovery};
use stacc_git::{Git, RebaseError};
use stacc_github::{GitHub, PrState, PullRequestUpdate};
use stacc_state::{RepoConfig, State, StateError, StateStore};

use crate::cli::{
    MergeArgs, ModifyArgs, MoveArgs, OutputFormat, RestackArgs, SquashArgs, SyncArgs, UndoArgs,
};
use crate::error::Error;

/// `stacc modify`: fold staged changes into the current branch (amend its tip by
/// default, append with `--commit`), then restack its upstack onto the new tip.
/// On conflict, records an `Operation::Modify` whose `pre_amend` anchor lets
/// `abort` undo the amend. Local-only: no push.
pub fn modify(args: &ModifyArgs, format: OutputFormat) -> Result<(), Error> {
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

    // Fail fast if any branch we would restack is checked out in another
    // worktree, rather than amending and then skipping that child mid-pass.
    guard_worktree(&git, &ops::upstack_order(&state.branches, &branch))?;

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
pub fn squash(args: &SquashArgs, format: OutputFormat) -> Result<(), Error> {
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

/// `stacc move`: re-parent the current branch (and its upstack) onto `--onto`.
/// Rejects a move onto the branch's own upstack (a cycle). On conflict records
/// an `Operation::Move` whose `pre_base` lets `abort` roll the recorded base
/// back. Local-only: no push.
pub fn move_cmd(args: &MoveArgs, format: OutputFormat) -> Result<(), Error> {
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

    // Fail fast if any branch in the subtree we would restack is checked out in
    // another worktree.
    guard_worktree(&git, &subtree)?;

    // Re-point the recorded base name. Keep base.hash: it marks where the
    // branch's own commits start, which restack replays onto the new base's tip.
    if let Some(b) = state.branches.get_mut(&branch) {
        b.base.name.clone_from(onto);
    }

    let restacked = restack_with_recovery(
        &git,
        &store,
        &mut state,
        &repo,
        &subtree,
        |remaining| recovery::Operation::Move {
            branch: branch.clone(),
            remaining,
            pre_base: pre_base.clone(),
        },
        // Re-point the moved branch's base name onto fresh state, so a concurrent
        // change to another branch is preserved by the re-apply.
        &|s| {
            if let Some(b) = s.branches.get_mut(&branch) {
                b.base.name.clone_from(onto);
            }
        },
    )?;
    clear_conflict_artifacts(&git);
    // Best-effort: the move is already saved, so a failure to switch back to the
    // moved branch must not report the whole move as failed.
    if let Err(err) = git.checkout(&branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    let tip = git.rev_parse(&branch)?;
    report_move(format, &branch, onto, &tip, &restacked);
    Ok(())
}

fn report_move(format: OutputFormat, branch: &str, base: &str, sha: &str, restacked: &[String]) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({ "op": "move", "branch": branch, "base": base, "sha": sha, "restacked": restacked })
        ),
        OutputFormat::Pretty => {
            println!("Moved {branch} onto {base}");
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
pub fn sync(args: &SyncArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    if args.continue_ {
        return continue_op(&git, &store, &mut state, &repo, format);
    }

    let merged = detect_merged(&git, &state, &repo)?;
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
    let outcome = reconcile_with(&git, &store, &mut state, &repo, drop, args.offline)?;
    report_sync(
        format,
        "sync",
        &merged,
        &pruned,
        &outcome.reparented,
        &outcome.restacked,
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

/// Ask GitHub which recorded PRs have merged, returning their branch names.
fn detect_merged(
    git: &Git,
    state: &State,
    repo: &RepoConfig,
) -> Result<BTreeSet<String>, Error> {
    let with_prs: Vec<(String, u64)> = state
        .branches
        .iter()
        .filter_map(|(name, b)| b.pr.as_ref().map(|pr| (name.clone(), pr.number)))
        .collect();
    let mut merged: BTreeSet<String> = BTreeSet::new();
    if !with_prs.is_empty() {
        let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
            .ok_or_else(|| Error::Usage(format!("remote `{}` is not a GitHub URL", repo.remote)))?;
        let github = GitHub::from_env()?;
        for (name, number) in &with_prs {
            if github.get_pull_request(&owner, &repo_name, *number)?.state == PrState::Merged {
                merged.insert(name.clone());
            }
        }
    }
    Ok(merged)
}

/// Reconcile a caller-supplied drop set and restack the stack, the shared core
/// of `sync` (merged-plus-pruned branches) and `merge` (the branches it just
/// merged): re-parent the dropped branches' children onto the nearest surviving
/// base, drop them, fast-forward the trunk (unless `offline`), then restack the
/// remainder bottom-up. Persists state and best-effort pushes it.
fn reconcile_with(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    dropped: BTreeSet<String>,
    offline: bool,
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

    // The command's own state change, re-applied onto fresh state whenever the
    // transactional save retries: drop the merged/pruned branches and re-parent
    // their children. Both operations are idempotent, so re-applying after a CAS
    // miss is safe.
    let dropped_delta = dropped.clone();
    let reparented_delta = reparented.clone();
    let apply_drops = move |s: &mut State| {
        for (name, new_base) in &reparented_delta {
            if let Some(b) = s.branches.get_mut(name) {
                b.base.name.clone_from(new_base);
            }
        }
        for name in &dropped_delta {
            s.branches.remove(name);
        }
    };

    // Persist the dropped/re-parented branches transactionally before the
    // fallible fetch and restack, so PRs already merged on GitHub are not
    // stranded in local state if the fetch or restack then fails (a re-run
    // reconciles from here).
    if !dropped.is_empty() {
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
    let restacked = restack_with_recovery(
        git,
        store,
        state,
        repo,
        &order,
        |remaining| recovery::Operation::Sync { remaining },
        &apply_drops,
    )?;

    finish_sync(git, store, repo);
    Ok(SyncOutcome {
        merged: dropped,
        reparented,
        restacked,
    })
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
/// re-parented and restacked onto the trunk between merges, where it stopped
/// short (structured), and any deferred hard error.
struct MergeWalk {
    merged: Vec<MergedPr>,
    reparented: Vec<(String, String)>,
    restacked: Vec<String>,
    stopped: Option<Value>,
    error: Option<Error>,
}

/// `stacc merge`: squash-merge the ready PRs from the trunk up to the current
/// branch, bottom-up, then reconcile via the `sync` logic. Stops at the first PR
/// that is not cleanly mergeable. No-op (with a message) when nothing is ready.
pub fn merge(args: &MergeArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
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

    let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
        .ok_or_else(|| Error::Usage(format!("remote `{}` is not a GitHub URL", repo.remote)))?;
    let github = GitHub::from_env()?;

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

    // Retarget every non-bottom open PR to the trunk UP FRONT, before any parent
    // merges. A child PR whose base is the parent's branch is closed (un-
    // reopenably) when GitHub deletes that branch on merge; pointing it at the
    // trunk first keeps it open regardless of branch-deletion timing. (API only,
    // so it runs in `--offline` too.)
    retarget_children_to_trunk(&github, &owner, &repo_name, &repo.trunk, &state, &chain)?;

    // Walk bottom-up: merge each PR, then restack the rest onto the freshly
    // merged trunk and force-push the next branch, so its PR is `trunk + its own
    // commits` and merges cleanly (no squash-cascade conflict).
    let MergeWalk {
        merged: merged_prs,
        reparented,
        restacked,
        stopped,
        error: loop_err,
    } = merge_stack(
        &git, &store, &mut state, &repo, &github, &owner, &repo_name, &chain, args.offline,
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

    report_merge(format, &merged_prs, stopped.as_ref(), protected, outcome.as_ref());

    // Restore the user's branch. Each restack leaves HEAD on whichever branch it
    // rebased last, so without this `merge` silently strands you on a different
    // branch. Skip when a conflict left a rebase in progress (HEAD must stay on
    // the conflicting branch for `stacc continue`). The starting branch may have
    // been merged and dropped from state but its local ref still exists; fall
    // back to the trunk only if it is truly gone.
    if !git.rebase_in_progress() {
        let target = if git.ref_missing(&current) {
            &repo.trunk
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
    for branch in chain.iter().skip(1) {
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
/// prefix. Mutates `state` (the reconcile drops/restacks as it goes).
#[allow(clippy::too_many_arguments)]
fn merge_stack(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    chain: &[String],
    offline: bool,
) -> MergeWalk {
    let mut merged_prs: Vec<MergedPr> = Vec::new();
    let mut reparented_all: Vec<(String, String)> = Vec::new();
    let mut restacked_all: Vec<String> = Vec::new();
    let mut stopped: Option<Value> = None;
    // A hard error is deferred, not returned immediately, so the caller can still
    // report whatever already merged before surfacing it.
    let mut loop_err: Option<Error> = None;

    for (i, branch) in chain.iter().enumerate() {
        let Some(pr) = state.branches.get(branch).and_then(|b| b.pr.clone()) else {
            stopped = Some(json!({ "kind": "no_pr", "branch": branch, "reason": "no recorded PR; submit it first" }));
            break;
        };
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
                let live = match poll_pr_ready(github, owner, repo_name, pr.number) {
                    Ok(live) => live,
                    Err(err) => {
                        loop_err = Some(err);
                        break;
                    }
                };
                if !live.ready() {
                    stopped = Some(json!({ "kind": "not_ready", "branch": branch, "number": pr.number, "mergeable_state": live.mergeable_state, "reason": "not cleanly mergeable" }));
                    break;
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

        // Reaching here means `branch` merged (out of band or just now): every
        // non-merging arm above breaks. Drop it and restack the remaining stack
        // onto the merged trunk, so the next branch becomes `trunk + its own
        // commits` (force-pushed at the top of the next iteration). Online fetches
        // the trunk; offline restacks against the local trunk.
        let drop: BTreeSet<String> = std::iter::once(branch.clone()).collect();
        match reconcile_with(git, store, state, repo, drop, offline) {
            Ok(outcome) => {
                reparented_all.extend(outcome.reparented);
                restacked_all.extend(outcome.restacked);
            }
            Err(err) => {
                loop_err = Some(err);
                break;
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
            println!(
                "{}",
                json!({
                    "op": "merge",
                    "merged": merged_json,
                    "stopped_at": stopped,
                    "trunk_protected": protected,
                    "synced": synced,
                })
            );
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
                println!("Stopped at {branch}.");
            }
            if let Some(o) = outcome {
                for name in &o.restacked {
                    println!("Restacked {name}");
                }
            }
        }
    }
}

/// `stacc restack`: rebase tracked branches back onto their bases, repairing a
/// drifted stack. Defaults to the current branch and its upstack; `--stack`
/// restacks the whole stack. Unlike `sync`, this is purely local: no fetch, no
/// merge detection.
pub fn restack(args: &RestackArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
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
        ops::upstack_order(&state.branches, &current)
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
pub fn continue_cmd(format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
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
pub fn abort_cmd(format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
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

/// `stacc undo`: revert the most recent stacc mutation(s) by restoring a prior
/// version of the stack state and the affected branch tips. `--steps N` walks N
/// versions back (default 1). The restore is appended as a new version, so undo
/// is itself undoable. Non-interactive and JSON-complete.
pub fn undo(args: &UndoArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
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

    let rest: Vec<String> = remaining.into_iter().skip(1).collect();
    restacked.extend(restack_with_recovery(
        git,
        store,
        state,
        repo,
        &rest,
        |r| op.with_remaining(r),
        &apply_resumed,
    )?);

    clear_conflict_artifacts(git);
    if op.pushes_state() {
        if let Err(err) = store.push(&repo.remote) {
            eprintln!("warning: could not push state to `{}`: {err}", repo.remote);
        }
    }

    match &op {
        recovery::Operation::Sync { .. } => {
            report_sync(
                format,
                op.tag(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &[],
                &restacked,
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
    let mut applied: Vec<(String, String)> = Vec::new();
    match ops::restack(git, state, order, &mut applied) {
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

fn report_sync(
    format: OutputFormat,
    op: &str,
    merged: &BTreeSet<String>,
    pruned: &BTreeSet<String>,
    reparented: &[(String, String)],
    restacked: &[String],
) {
    match format {
        OutputFormat::Json => {
            let merged_list: Vec<&String> = merged.iter().collect();
            let pruned_list: Vec<&String> = pruned.iter().collect();
            let reparented_list: Vec<Value> = reparented
                .iter()
                .map(|(branch, base)| json!({ "branch": branch, "base": base }))
                .collect();
            println!(
                "{}",
                json!({
                    "op": op,
                    "merged": merged_list,
                    "pruned": pruned_list,
                    "reparented": reparented_list,
                    "restacked": restacked,
                })
            );
        }
        OutputFormat::Pretty => {
            if merged.is_empty() && pruned.is_empty() && reparented.is_empty() && restacked.is_empty()
            {
                println!("Already up to date.");
            } else {
                for name in merged {
                    println!("Merged, untracked: {name}");
                }
                for name in pruned {
                    println!("Pruned (no git ref): {name}");
                }
                for (name, base) in reparented {
                    println!("Re-parented {name} -> {base}");
                }
                for name in restacked {
                    println!("Restacked {name}");
                }
            }
        }
    }
}

pub(crate) fn clear_conflict_artifacts(git: &Git) {
    if let Ok(dir) = git.git_dir() {
        recovery::clear_continuation(&dir);
        let _ = std::fs::remove_file(dir.join("stacc-conflict-context.json"));
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
