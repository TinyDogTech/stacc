//! `stacc reorder`: reorder the branches between the trunk and the current
//! branch via a structured order spec (KTD-4), restacking descendants.
//!
//! `--order <b1,b2,...>` lists the downstack in its new bottom-up order (the
//! first name sits on the trunk) and is strictly validated as a permutation of
//! exactly the current downstack set; permissively re-parenting "whatever
//! names appear" could silently drop a branch from the stack. An editor form
//! is TTY-only future work, so a missing `--order` is a structured error.
//!
//! Reorder re-points N bases and then rebases the chain, and any of those N
//! rebases can conflict after earlier ones already landed, so it records a
//! resumable continuation (KTD-1): `Operation::Reorder` carries the target
//! order, the remaining queue, and a pre-state snapshot of every chain branch,
//! letting `continue` drain the queue and `abort` restore everything no matter
//! where the conflict hit. The single-anchor Move/Modify abort guards do not
//! generalize here and are deliberately not reused.

use std::collections::BTreeSet;

use serde_json::json;
use stacc_core::{ops, recovery};
use stacc_git::Git;
use stacc_state::{State, StateStore};

use super::operations::{clear_conflict_artifacts, guard_worktree, restack_with_recovery_forced};
use crate::cli::{OutputFormat, ReorderArgs};
use crate::error::Error;

/// `stacc reorder`: see the module docs. Validate everything, snapshot the
/// pre-state, re-point the chain's bases in one transactional write, then
/// restack the chain and its descendants through the engine.
pub fn reorder(args: &ReorderArgs, format: OutputFormat) -> Result<(), Error> {
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
        Error::Usage("cannot reorder a detached HEAD; check out a branch first".into())
    })?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot reorder from the trunk branch `{}`; check out the top of the stack to reorder",
            repo.trunk
        )));
    }

    // The downstack chain, bottom-up: trunk's child first, the current branch
    // last. Errors structurally when the current branch is untracked.
    let chain = ops::downstack_chain(&state, &branch, &repo.trunk)?;

    let order = parse_order(args.order.as_deref(), &chain, &branch)?;

    if order == chain {
        match format {
            OutputFormat::Json => println!("{}", json!({ "op": "reorder", "unchanged": true })),
            OutputFormat::Pretty => println!("Already in this order; nothing to reorder."),
        }
        return Ok(());
    }

    // Everything this reorder can rewrite: the chain plus every upstack
    // descendant of any member (forked children included). The chain's bottom
    // member's upstack covers all of it, since every chain member descends it.
    // Fail fast if any of it lives in another worktree, BEFORE mutating.
    let affected = ops::upstack_order(&state.branches, &chain[0]);
    guard_worktree(&git, &affected)?;

    // The current branch rebases under HEAD; refuse a dirty working tree
    // rather than dragging uncommitted edits through a rebase (mirrors split).
    if git.has_uncommitted_changes()? {
        return Err(Error::Usage(
            "the working tree has uncommitted changes; commit or stash them, then reorder".into(),
        ));
    }

    // Pre-state for every chain branch, captured before any mutation, so a
    // conflicted reorder's `abort` can restore all of it.
    let mut pre_state = Vec::with_capacity(chain.len());
    for name in &chain {
        let b = state.branches.get(name).expect("chain members are tracked");
        pre_state.push(recovery::ReorderPre {
            branch: name.clone(),
            base_name: b.base.name.clone(),
            base_hash: b.base.hash.clone(),
            tip: git.rev_parse(name)?,
        });
    }

    // Re-point each chain branch's base name onto its new predecessor (the
    // first onto the trunk). base.hash is deliberately KEPT: it marks where
    // the branch's own commits start, which is exactly what the engine replays
    // onto the new base's live tip (the same reason `move` keeps it); the
    // engine and `persist_restack` refresh the hashes as each rebase lands.
    let new_bases: Vec<(String, String)> = order
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let pred = if i == 0 {
                repo.trunk.clone()
            } else {
                order[i - 1].clone()
            };
            (name.clone(), pred)
        })
        .collect();
    let apply_reorder = move |s: &mut State| {
        for (name, pred) in &new_bases {
            if let Some(b) = s.branches.get_mut(name) {
                b.base.name.clone_from(pred);
            }
        }
    };
    apply_reorder(&mut state);
    // ONE transactional write for the whole re-point, before the restack, so
    // the recorded stack never holds a half-reordered chain; a conflict then
    // interrupts an already-recorded reorder that `continue`/`abort` resolve.
    store.update(|s| {
        apply_reorder(s);
        Ok(())
    })?;

    // The restack order is the engine's own order computation over the
    // re-pointed graph (as `restack --stack`/`sync` compute it), scoped to the
    // affected set. The chain members are forced: a reordered branch can
    // already descend its new base's tip (the flatten case) and still need to
    // drop the commits moved out from under it.
    let affected_set: BTreeSet<String> = affected.iter().cloned().collect();
    let restack_order: Vec<String> = ops::topo_order(&state.branches, &repo.trunk)
        .into_iter()
        .filter(|b| affected_set.contains(b))
        .collect();
    let force: BTreeSet<String> = order.iter().cloned().collect();

    let restacked = restack_with_recovery_forced(
        &git,
        &store,
        &mut state,
        &repo,
        &restack_order,
        |remaining| recovery::Operation::Reorder {
            order: order.clone(),
            remaining,
            pre_state: pre_state.clone(),
        },
        &apply_reorder,
        &force,
        false,
    )?;
    clear_conflict_artifacts(&git);
    // Best-effort: the reorder is already saved, so a failure to switch back to
    // the starting branch must not report the whole reorder as failed.
    if let Err(err) = git.checkout(&branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    report_reorder(format, &order, &restacked);
    Ok(())
}

/// Parse and strictly validate `--order`: required, comma-separated, and
/// exactly a permutation of the current downstack `chain`. Unknown names,
/// duplicates, and missing members are each rejected with an error naming the
/// offender, before anything mutates.
fn parse_order(raw: Option<&str>, chain: &[String], branch: &str) -> Result<Vec<String>, Error> {
    let Some(raw) = raw else {
        return Err(Error::Usage(format!(
            "missing --order; pass the downstack branches in their new bottom-up order, e.g. --order {} (an editor form is TTY-only future work)",
            chain.join(",")
        )));
    };
    let order: Vec<String> = raw.split(',').map(|s| s.trim().to_string()).collect();
    if order.iter().any(String::is_empty) {
        return Err(Error::Usage(
            "--order contains an empty name; pass a comma-separated list of branch names".into(),
        ));
    }
    let chain_set: BTreeSet<&str> = chain.iter().map(String::as_str).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for name in &order {
        if !chain_set.contains(name.as_str()) {
            return Err(Error::Usage(format!(
                "`{name}` is not in the downstack of `{branch}`; --order must be a permutation of: {}",
                chain.join(", ")
            )));
        }
        if !seen.insert(name.as_str()) {
            return Err(Error::Usage(format!(
                "`{name}` appears more than once in --order; every downstack branch must appear exactly once"
            )));
        }
    }
    if order.len() != chain.len() {
        let missing: Vec<&str> = chain
            .iter()
            .map(String::as_str)
            .filter(|c| !seen.contains(*c))
            .collect();
        return Err(Error::Usage(format!(
            "--order is missing {}; every downstack branch must appear exactly once",
            missing.join(", ")
        )));
    }
    Ok(order)
}

fn report_reorder(format: OutputFormat, order: &[String], restacked: &[String]) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({ "op": "reorder", "order": order, "restacked": restacked })
        ),
        OutputFormat::Pretty => {
            println!("Reordered: {}", order.join(" -> "));
            for name in restacked {
                println!("Restacked {name}");
            }
        }
    }
}
