//! Stack operations: sync, restack, modify, move, and the conflict-recovery
//! (continue/abort) lifecycle they share.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use stacc_core::{ops, recovery};
use stacc_git::{Git, RebaseError};
use stacc_github::{GitHub, PrState, PullRequestUpdate};
use stacc_state::{RepoConfig, State, StateStore};

use crate::cli::{MergeArgs, ModifyArgs, MoveArgs, OutputFormat, RestackArgs, SyncArgs};
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
    let restacked = restack_with_recovery(&git, &store, &mut state, &repo, &order, |remaining| {
        recovery::Operation::Modify {
            branch: branch.clone(),
            remaining,
            pre_amend: pre_amend.clone(),
        }
    })?;

    store.save(&state).map_err(|e| {
        Error::Usage(format!(
            "amend and restack succeeded but could not save state: {e}; run `stacc restack` to re-sync"
        ))
    })?;
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

    // Re-point the recorded base name. Keep base.hash: it marks where the
    // branch's own commits start, which restack replays onto the new base's tip.
    if let Some(b) = state.branches.get_mut(&branch) {
        b.base.name.clone_from(onto);
    }

    let restacked =
        restack_with_recovery(&git, &store, &mut state, &repo, &subtree, |remaining| {
            recovery::Operation::Move {
                branch: branch.clone(),
                remaining,
                pre_base: pre_base.clone(),
            }
        })?;

    store.save(&state).map_err(|e| {
        Error::Usage(format!(
            "move succeeded but could not save state: {e}; run `stacc restack` to re-sync"
        ))
    })?;
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

/// Reconcile a known-merged set and restack the stack, the shared core of `sync`
/// (which detects `merged` via GitHub) and `merge` (which already knows what it
/// merged): re-parent the merged branches' children onto the nearest surviving
/// base, drop them, fast-forward the trunk (unless `offline`), then restack the
/// remainder bottom-up. Persists state and best-effort pushes it.
fn reconcile_with(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    merged: BTreeSet<String>,
    offline: bool,
) -> Result<SyncOutcome, Error> {
    // Re-parent children of merged branches onto the nearest surviving base.
    let mut reparented: Vec<(String, String)> = Vec::new();
    for (name, branch) in &state.branches {
        if merged.contains(name) {
            continue;
        }
        let new_base = ops::resolve_base(&state.branches, &merged, branch.base.name.clone());
        if new_base != branch.base.name {
            reparented.push((name.clone(), new_base));
        }
    }
    for (name, new_base) in &reparented {
        if let Some(branch) = state.branches.get_mut(name) {
            branch.base.name.clone_from(new_base);
        }
    }
    for name in &merged {
        state.branches.remove(name);
    }

    // Persist the dropped/re-parented branches before the fallible fetch and
    // restack, so PRs already merged on GitHub are not stranded in local state
    // if the fetch or restack then fails (a re-run reconciles from here).
    if !merged.is_empty() {
        store.save(state)?;
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
    let restacked = restack_with_recovery(git, store, state, repo, &order, |remaining| {
        recovery::Operation::Sync { remaining }
    })?;

    store.save(state)?;
    finish_sync(git, store, repo);
    Ok(SyncOutcome {
        merged,
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

/// The result of walking the downstack chain: what merged, where it stopped
/// short (structured), and any deferred hard error.
struct MergeWalk {
    merged: Vec<MergedPr>,
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

    let MergeWalk {
        merged: merged_prs,
        stopped,
        error: mut loop_err,
    } = merge_ready_downstack(&github, &owner, &repo_name, &repo.trunk, &state, &chain);

    // Reconcile whatever merged (drop those branches, re-parent, restack), even
    // when the loop errored, so PRs already merged on GitHub are not stranded. We
    // know what merged, so skip the GitHub re-detection that `sync` does.
    let outcome = if merged_prs.is_empty() {
        None
    } else {
        let merged: BTreeSet<String> = merged_prs.iter().map(|m| m.branch.clone()).collect();
        match reconcile_with(&git, &store, &mut state, &repo, merged, args.offline) {
            Ok(outcome) => Some(outcome),
            Err(err) => {
                loop_err.get_or_insert(err);
                None
            }
        }
    };

    if args.offline && outcome.is_some() {
        eprintln!(
            "note: --offline skipped the fetch; run `stacc sync` to rebase the local stack onto the merged commits."
        );
    }

    report_merge(format, &merged_prs, stopped.as_ref(), protected, outcome.as_ref());
    if let Some(err) = loop_err {
        return Err(err);
    }
    Ok(())
}

/// Walk the downstack `chain` bottom-up, squash-merging each ready PR and
/// re-pointing the next one's base to the trunk after a merge. Returns the merged
/// `(branch, number, sha)` tuples, the structured stop reason if it stopped
/// short, and any deferred hard error (so the caller can reconcile the merged
/// prefix before surfacing it). Mutates no local state.
fn merge_ready_downstack(
    github: &GitHub,
    owner: &str,
    repo_name: &str,
    trunk: &str,
    state: &State,
    chain: &[String],
) -> MergeWalk {
    let mut merged_prs: Vec<MergedPr> = Vec::new();
    let mut stopped: Option<Value> = None;
    // A hard error is deferred, not returned immediately, so the caller can still
    // reconcile whatever already merged before surfacing it.
    let mut loop_err: Option<Error> = None;
    for (i, branch) in chain.iter().enumerate() {
        let Some(pr) = state.branches.get(branch).and_then(|b| b.pr.clone()) else {
            stopped = Some(json!({ "kind": "no_pr", "branch": branch, "reason": "no recorded PR; submit it first" }));
            break;
        };
        // Classify the PR's current state before mutating it: one merged out of
        // band is already done (count it and keep walking up); a closed one stops
        // the walk. Checking first also avoids re-pointing a non-open PR's base.
        let current = match github.get_pull_request(owner, repo_name, pr.number) {
            Ok(current) => current,
            Err(err) => {
                loop_err = Some(err.into());
                break;
            }
        };
        match current.state {
            PrState::Merged => {
                merged_prs.push(MergedPr {
                    branch: branch.clone(),
                    number: pr.number,
                    sha: None,
                    out_of_band: true,
                });
                continue;
            }
            PrState::Closed => {
                stopped = Some(json!({ "kind": "closed", "branch": branch, "number": pr.number, "reason": "PR is closed, not merged" }));
                break;
            }
            PrState::Open => {}
        }
        // After a prior merge the base of this PR (the merged branch) is gone, so
        // re-point it to the trunk and let GitHub recompute readiness against it.
        if i > 0 {
            let update = PullRequestUpdate {
                base: Some(trunk.to_string()),
                ..Default::default()
            };
            if let Err(err) = github.update_pull_request(owner, repo_name, pr.number, &update) {
                loop_err = Some(err.into());
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
            stopped = Some(json!({
                "kind": "not_ready",
                "branch": branch,
                "number": pr.number,
                "mergeable_state": live.mergeable_state,
                "reason": "not cleanly mergeable",
            }));
            break;
        }
        match github.merge_pull_request(owner, repo_name, pr.number) {
            Ok(outcome) if outcome.merged => {
                merged_prs.push(MergedPr {
                    branch: branch.clone(),
                    number: pr.number,
                    sha: outcome.sha,
                    out_of_band: false,
                });
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
    MergeWalk {
        merged: merged_prs,
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

    let restacked = restack_with_recovery(&git, &store, &mut state, &repo, &order, |remaining| {
        recovery::Operation::Restack { remaining }
    })?;

    store.save(&state)?;
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
            match store.load() {
                Ok(mut state) => {
                    let subtree = ops::upstack_order(&state.branches, branch).len();
                    if remaining.len() == subtree {
                        if let Some(b) = state.branches.get_mut(branch) {
                            b.base.name.clone_from(pre_base);
                        }
                        if let Err(err) = store.save(&state) {
                            eprintln!(
                                "warning: could not restore `{branch}`'s base to `{pre_base}`: {err}; run `stacc move --onto {pre_base}` to roll it back"
                            );
                        }
                    } else {
                        eprintln!(
                            "warning: `{branch}` stays moved; upstack branches were already restacked onto the new base. Run `stacc restack` to finish, or `stacc move --onto {pre_base}` to move it back."
                        );
                    }
                }
                Err(err) => {
                    eprintln!(
                        "warning: could not load state to restore `{branch}`'s base: {err}; run `stacc move --onto {pre_base}` to roll it back"
                    );
                }
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

/// Resume the operation recorded in the continuation: finish the conflicting
/// rebase, then drain the remaining queue. The recorded [`recovery::Operation`]
/// drives the output shape and whether the state ref is pushed, so this resumes
/// whatever was in flight (sync, restack, ...) regardless of how it was invoked.
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
    if let Some(first) = remaining.first() {
        if let Some(base_name) = state.branches.get(first).map(|b| b.base.name.clone()) {
            let base_tip = git.rev_parse(&base_name)?;
            if let Some(b) = state.branches.get_mut(first) {
                b.base.hash = base_tip;
            }
        }
        restacked.push(first.clone());
    }

    let rest: Vec<String> = remaining.into_iter().skip(1).collect();
    restacked.extend(restack_with_recovery(git, store, state, repo, &rest, |r| {
        op.with_remaining(r)
    })?);

    store.save(state)?;
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
fn restack_with_recovery(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    order: &[String],
    make_op: impl Fn(Vec<String>) -> recovery::Operation,
) -> Result<Vec<String>, Error> {
    match ops::restack(git, store, state, order) {
        Ok(outcome) => {
            if !outcome.skipped.is_empty() {
                eprintln!(
                    "warning: skipped {} branch(es) with no git ref: {}. Remove them with `stacc untrack <branch>`.",
                    outcome.skipped.len(),
                    outcome.skipped.join(", ")
                );
            }
            Ok(outcome.restacked)
        }
        Err(ops::OpsError::Conflict { branch, remaining }) => {
            // `ops::restack` already saved state before returning. Write the
            // agent-readable context first (best-effort), then the resume
            // marker; if the marker write fails we would strand the user
            // mid-rebase with no `stacc continue`, so abort back to a clean tree.
            write_conflict_context(git, state, repo, &branch);
            let dir = git.git_dir()?;
            if let Err(err) = recovery::write_continuation(&dir, &make_op(remaining)) {
                let aborted = git.rebase_abort();
                clear_conflict_artifacts(git);
                return Err(Error::Usage(match aborted {
                    Ok(()) => format!(
                        "conflict on `{branch}`, but the recovery state could not be saved ({err}); rebase aborted to a clean tree"
                    ),
                    Err(abort_err) => format!(
                        "conflict on `{branch}`, but the recovery state could not be saved ({err}) and the rebase abort also failed ({abort_err}); run `git rebase --abort` manually"
                    ),
                }));
            }
            Err(Error::Conflict { branch })
        }
        Err(err) => Err(err.into()),
    }
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

fn clear_conflict_artifacts(git: &Git) {
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
