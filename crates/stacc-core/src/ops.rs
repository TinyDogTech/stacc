//! The stack-operations engine: ordering the tracked branches and restacking
//! them onto their bases. These functions are forge-agnostic, they speak only
//! `git` and the state ref, so `submit`, `sync`, and `restack` (and later
//! `modify`/`move`/`merge`) all share one implementation.
//!
//! Conflict *recovery* is split: the [`crate::recovery`] module owns the typed
//! continuation record and its I/O, while the GitHub-enriched conflict-*context*
//! file is written by the CLI. `restack` here reports a conflict structurally
//! (the branch plus the unfinished queue); the caller persists the continuation
//! and the context. That keeps `stacc-core` off `stacc-github`.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use stacc_git::{Git, GitError, RebaseError};
use stacc_state::{BranchState, State, StateError};
use thiserror::Error;

/// Failures from the operations engine. The CLI maps these onto its own
/// user-facing error type; `Conflict` is handled specially so the caller can
/// write the recovery artifacts.
#[derive(Debug, Error)]
pub enum OpsError {
    #[error(transparent)]
    Git(#[from] GitError),

    #[error(transparent)]
    State(#[from] StateError),

    /// A rebase stopped on a conflict; `remaining` is the unfinished queue with
    /// the conflicting `branch` first, so the caller can resume from here.
    #[error("rebase conflict on `{branch}`")]
    Conflict {
        branch: String,
        remaining: Vec<String>,
    },

    #[error("cannot recover the fork point of `{branch}` from `{base}`; rebase manually")]
    ForkPointLost { branch: String, base: String },

    #[error("branch `{0}` is not tracked; run `stacc track` first")]
    Untracked(String),

    #[error("circular base chain reached at `{0}`")]
    Cycle(String),
}

/// Order the tracked branches bottom-up: a branch appears after its base.
pub fn topo_order(branches: &BTreeMap<String, BranchState>, trunk: &str) -> Vec<String> {
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut order: Vec<String> = Vec::new();
    loop {
        let mut progressed = false;
        for (name, branch) in branches {
            if emitted.contains(name) {
                continue;
            }
            if branch.base.name == trunk || emitted.contains(&branch.base.name) {
                order.push(name.clone());
                emitted.insert(name.clone());
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    order
}

/// The current branch plus its upstack (transitive children), bottom-up, a
/// base always precedes its dependents. Unlike [`topo_order`], this is scoped
/// to one branch's subtree, which is what `modify`/`move` need.
pub fn upstack_order(branches: &BTreeMap<String, BranchState>, current: &str) -> Vec<String> {
    let mut order = vec![current.to_string()];
    let mut idx = 0;
    while idx < order.len() {
        let node = order[idx].clone();
        for (name, branch) in branches {
            if branch.base.name == node && !order.contains(name) {
                order.push(name.clone());
            }
        }
        idx += 1;
    }
    order
}

/// The recorded parent (base) of `branch`, or `None` if it is not tracked.
pub fn parent(branches: &BTreeMap<String, BranchState>, branch: &str) -> Option<String> {
    branches.get(branch).map(|b| b.base.name.clone())
}

/// The branches stacked directly on `branch` (recorded base == `branch`), in
/// name order.
pub fn children(branches: &BTreeMap<String, BranchState>, branch: &str) -> Vec<String> {
    branches
        .iter()
        .filter(|(_, b)| b.base.name == branch)
        .map(|(name, _)| name.clone())
        .collect()
}

/// The bottom of `branch`'s stack: the first non-trunk ancestor (the branch
/// whose recorded base is the trunk). Returns `branch` itself when it already
/// sits on the trunk or is untracked.
pub fn bottom(branches: &BTreeMap<String, BranchState>, branch: &str, trunk: &str) -> String {
    let mut current = branch.to_string();
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        match branches.get(&current).map(|b| b.base.name.clone()) {
            Some(base) if base != trunk => current = base,
            _ => break,
        }
    }
    current
}

/// The top of `branch`'s stack, reached by following single children upward.
/// `Ok` is the tip (a leaf); `Err` carries the children of the fork where the
/// walk could not continue without a choice.
pub fn top(branches: &BTreeMap<String, BranchState>, branch: &str) -> Result<String, Vec<String>> {
    let mut current = branch.to_string();
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        let kids = children(branches, &current);
        match kids.len() {
            0 => return Ok(current),
            1 => current = kids.into_iter().next().expect("one child"),
            _ => return Err(kids),
        }
    }
    Ok(current)
}

/// Walk from `current` up the base chain to the trunk (exclusive). Returns the
/// branches in **bottom-up** order, base before dependent, so each push/PR
/// sees its parent already on the remote.
pub fn downstack_chain(state: &State, current: &str, trunk: &str) -> Result<Vec<String>, OpsError> {
    let mut chain = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut name = current.to_string();
    loop {
        if name == trunk {
            break;
        }
        if !visited.insert(name.clone()) {
            return Err(OpsError::Cycle(name));
        }
        match state.branches.get(&name) {
            Some(bs) => {
                chain.push(name.clone());
                name = bs.base.name.clone();
            }
            None => return Err(OpsError::Untracked(name)),
        }
    }
    chain.reverse();
    Ok(chain)
}

/// Follow `base` up the stack, skipping any merged branches, to the nearest
/// surviving base (eventually the trunk).
pub fn resolve_base(
    branches: &BTreeMap<String, BranchState>,
    merged: &BTreeSet<String>,
    mut base: String,
) -> String {
    while merged.contains(&base) {
        match branches.get(&base) {
            Some(branch) => base = branch.base.name.clone(),
            None => break,
        }
    }
    base
}

/// What a restack pass did: the branches it rebased, any it skipped because
/// their own ref or their base's ref no longer resolves (a deleted branch still
/// left in state), and any it skipped because they are checked out in another
/// worktree (rewriting them there would desync that worktree). Skipping keeps a
/// single dead or borrowed branch from aborting the pass.
#[derive(Debug)]
pub struct RestackOutcome {
    pub restacked: Vec<String>,
    pub skipped: Vec<String>,
    /// `(branch, other worktree path)` for branches skipped because they are
    /// checked out elsewhere.
    pub worktree_skipped: Vec<(String, String)>,
    /// Branches skipped because their tree already matches their base's tip
    /// (they look squash-merged). Populated only when the caller enables the
    /// tree-identical guard (sync's restack pass).
    pub tree_identical_skipped: Vec<String>,
}

/// Restack `order` bottom-up, rebasing each branch onto its base's current tip
/// and updating its recorded base hash in `state`. Each completed branch's new
/// `(name, base_hash)` is pushed onto `applied` so the caller can persist the
/// change transactionally (via `StateStore::update`) rather than writing the
/// whole state. On a conflict it returns [`OpsError::Conflict`] with the
/// unfinished queue, leaving the rebase in progress and `applied` holding the
/// branches finished so far; the caller MUST persist those plus a continuation
/// (or abort the rebase) before returning to the user, see
/// `restack_with_recovery`.
///
/// A branch whose own ref or whose base's ref no longer resolves is skipped
/// (not rebased) and collected into [`RestackOutcome::skipped`], so a deleted
/// branch left in state does not abort the whole pass with a fatal git error.
pub fn restack(
    git: &Git,
    state: &mut State,
    order: &[String],
    applied: &mut Vec<(String, String)>,
) -> Result<RestackOutcome, OpsError> {
    restack_forced(git, state, order, applied, &BTreeSet::new(), false)
}

/// Like [`restack`], but rebases the branches named in `force` even when they
/// already descend their base's tip. `reorder` needs this: a reordered branch
/// whose new base is an ancestor of its old lineage looks "already on top of
/// its base" to the skip check, yet still has to drop the commits the reorder
/// moved out from under it. The forced rebase replays exactly the branch's own
/// commits (`base.hash..branch`) onto the base's live tip; when nothing
/// actually changed, git's own up-to-date check makes it a no-op.
///
/// With `tree_guard` set (sync's restack pass), a branch whose tip tree already
/// matches its base's tip, yet is not an ancestor of that base, is skipped as
/// "looks squash-merged" instead of being rebased into a phantom conflict.
pub fn restack_forced(
    git: &Git,
    state: &mut State,
    order: &[String],
    applied: &mut Vec<(String, String)>,
    force: &BTreeSet<String>,
    tree_guard: bool,
) -> Result<RestackOutcome, OpsError> {
    let mut restacked = Vec::new();
    let mut skipped = Vec::new();
    let mut worktree_skipped = Vec::new();
    let mut tree_identical_skipped = Vec::new();
    for (idx, branch) in order.iter().enumerate() {
        let Some(base) = state.branches.get(branch).map(|b| b.base.clone()) else {
            continue;
        };
        if git.ref_missing(branch) || git.ref_missing(&base.name) {
            skipped.push(branch.clone());
            continue;
        }
        // Refuse to rewrite a branch checked out in another worktree: rebasing it
        // would desync that worktree's HEAD/index. Skip it (and its dependents
        // will skip in turn once their base no longer resolves), reporting where
        // it lives so the user can act.
        if let Some(wt) = git.branch_checked_out_elsewhere(branch)? {
            worktree_skipped.push((branch.clone(), wt.to_string_lossy().into_owned()));
            continue;
        }
        let base_tip = git.rev_parse(&base.name)?;
        if !force.contains(branch) && git.is_ancestor(&base_tip, branch)? {
            continue; // already on top of its base
        }
        // Tree-identical guard (sync's restack pass only): a branch whose tip
        // tree already matches its base tip's tree, yet is not an ancestor of
        // that base (handled above), looks squash-merged. Rebasing it replays an
        // empty diff and can only conflict (the STA-90 shape), so skip it.
        if tree_guard && !force.contains(branch) && git.same_tree(branch, &base_tip)? {
            tree_identical_skipped.push(branch.clone());
            continue;
        }
        // Prefer the recorded base hash if it's still reachable from the branch;
        // otherwise (stale, force-pushed away, or invalid) recover via
        // `merge-base --fork-point` using the base's reflog.
        let recorded_ok = git.is_ancestor(&base.hash, branch).unwrap_or(false);
        let upstream = if recorded_ok {
            base.hash.clone()
        } else {
            git.fork_point(&base.name, branch)?
                .ok_or_else(|| OpsError::ForkPointLost {
                    branch: branch.clone(),
                    base: base.name.clone(),
                })?
        };
        match git.rebase_onto(&base_tip, &upstream, branch) {
            Ok(()) => {}
            Err(RebaseError::Interrupt(_)) => {
                // Persisting `applied` so far is the caller's job now, through a
                // transactional update; the engine only reports the conflict.
                return Err(OpsError::Conflict {
                    branch: branch.clone(),
                    remaining: order[idx..].to_vec(),
                });
            }
            Err(RebaseError::Git(err)) => return Err(err.into()),
        }
        if let Some(b) = state.branches.get_mut(branch) {
            b.base.hash.clone_from(&base_tip);
        }
        applied.push((branch.clone(), base_tip));
        restacked.push(branch.clone());
    }
    Ok(RestackOutcome {
        restacked,
        skipped,
        worktree_skipped,
        tree_identical_skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use stacc_state::{Base, RepoConfig, StateStore};

    /// Build a branch map from (branch, base) pairs.
    fn nav_stack(pairs: &[(&str, &str)]) -> BTreeMap<String, BranchState> {
        pairs
            .iter()
            .map(|(name, base)| {
                (
                    (*name).to_string(),
                    BranchState {
                        base: Base {
                            name: (*base).to_string(),
                            hash: "h".into(),
                        },
                        pr: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn nav_helpers_on_a_linear_stack() {
        // main -> a -> b -> c
        let s = nav_stack(&[("a", "main"), ("b", "a"), ("c", "b")]);
        assert_eq!(parent(&s, "b").as_deref(), Some("a"));
        assert_eq!(parent(&s, "missing"), None);
        assert_eq!(children(&s, "a"), ["b"]);
        assert!(children(&s, "c").is_empty());
        assert_eq!(bottom(&s, "c", "main"), "a");
        assert_eq!(bottom(&s, "a", "main"), "a");
        assert_eq!(top(&s, "a"), Ok("c".to_string()));
    }

    #[test]
    fn nav_helpers_on_a_branched_stack() {
        // main -> a -> { b, c }
        let s = nav_stack(&[("a", "main"), ("b", "a"), ("c", "a")]);
        assert_eq!(children(&s, "a"), ["b", "c"]);
        assert_eq!(top(&s, "a"), Err(vec!["b".to_string(), "c".to_string()])); // fork
        assert_eq!(top(&s, "b"), Ok("b".to_string())); // leaf
        assert_eq!(bottom(&s, "b", "main"), "a");
    }

    #[test]
    fn nav_helpers_terminate_on_a_cycle() {
        // a -> b -> a (corrupt state): the visited guard must stop the walk.
        let s = nav_stack(&[("a", "b"), ("b", "a")]);
        let _ = bottom(&s, "a", "main");
        let _ = top(&s, "a");
    }
    use std::path::Path;
    use tempfile::TempDir;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn write_commit(dir: &Path, file: &str, contents: &str, msg: &str) {
        std::fs::write(dir.join(file), contents).expect("write file");
        run_git(dir, &["add", file]);
        run_git(dir, &["commit", "-q", "-m", msg]);
    }

    fn init_repo() -> (TempDir, Git) {
        let tmp = TempDir::new().expect("temp dir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        run_git(tmp.path(), &["config", "user.name", "Test"]);
        run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "root"]);
        let git = Git::open(tmp.path());
        (tmp, git)
    }

    fn branch_state(branches: &[(&str, &str)]) -> BTreeMap<String, BranchState> {
        branches
            .iter()
            .map(|(name, base)| {
                (
                    (*name).to_string(),
                    BranchState {
                        base: Base {
                            name: (*base).to_string(),
                            hash: "0".repeat(40),
                        },
                        pr: None,
                    },
                )
            })
            .collect()
    }

    /// A two-branch stack: `a` on `main`, `b` on `a`, each with one commit, with
    /// recorded base hashes captured at track time. Returns the store + state.
    fn linear_stack(tmp: &TempDir, git: &Git) -> (StateStore, State) {
        let path = tmp.path();
        let main_root = git.rev_parse("main").unwrap();
        run_git(path, &["checkout", "-q", "-b", "a"]);
        write_commit(path, "a.txt", "a\n", "a1");
        let a_tip = git.rev_parse("a").unwrap();
        run_git(path, &["checkout", "-q", "-b", "b"]);
        write_commit(path, "b.txt", "b\n", "b1");

        let mut branches = BTreeMap::new();
        branches.insert(
            "a".to_string(),
            BranchState {
                base: Base {
                    name: "main".into(),
                    hash: main_root,
                },
                pr: None,
            },
        );
        branches.insert(
            "b".to_string(),
            BranchState {
                base: Base {
                    name: "a".into(),
                    hash: a_tip,
                },
                pr: None,
            },
        );
        let state = State {
            repo: Some(RepoConfig {
                trunk: "main".into(),
                remote: "origin".into(),
            }),
            branches,
            ..State::default()
        };
        (StateStore::new(git.clone()), state)
    }

    #[test]
    fn topo_order_bases_before_dependents() {
        // Branched stack: a -> main, b -> a, c -> a.
        let branches = branch_state(&[("b", "a"), ("c", "a"), ("a", "main")]);
        let order = topo_order(&branches, "main");
        let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("a") < pos("c"));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn upstack_order_is_current_plus_upstack() {
        // a -> main, b -> a, c -> b, sib -> main (unrelated).
        let branches = branch_state(&[("c", "b"), ("b", "a"), ("a", "main"), ("sib", "main")]);
        let order = upstack_order(&branches, "a");
        assert_eq!(order, vec!["a", "b", "c"]); // excludes the sibling stack
    }

    #[test]
    fn downstack_chain_is_bottom_up() {
        let branches = branch_state(&[("c", "b"), ("b", "a"), ("a", "main")]);
        let state = State {
            repo: None,
            branches,
            ..State::default()
        };
        assert_eq!(
            downstack_chain(&state, "c", "main").unwrap(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn downstack_chain_detects_cycle_and_untracked() {
        let cyclic = branch_state(&[("a", "b"), ("b", "a")]);
        let state = State {
            repo: None,
            branches: cyclic,
            ..State::default()
        };
        assert!(matches!(
            downstack_chain(&state, "a", "main"),
            Err(OpsError::Cycle(_))
        ));

        let state = State {
            repo: None,
            branches: branch_state(&[("a", "missing")]),
            ..State::default()
        };
        let err = downstack_chain(&state, "a", "main").unwrap_err();
        assert!(matches!(err, OpsError::Untracked(ref n) if n == "missing"));
    }

    #[test]
    fn resolve_base_skips_merged() {
        let branches = branch_state(&[("b", "a"), ("a", "main")]);
        let mut merged = BTreeSet::new();
        merged.insert("a".to_string());
        assert_eq!(resolve_base(&branches, &merged, "a".into()), "main");
    }

    #[test]
    fn restack_rebases_chain_and_updates_base_hashes() {
        let (tmp, git) = init_repo();
        let (_store, mut state) = linear_stack(&tmp, &git);

        // Advance main out from under the stack.
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "m.txt", "m\n", "main moves");
        let new_main = git.rev_parse("main").unwrap();

        let restacked = restack(&git, &mut state, &["a".into(), "b".into()], &mut Vec::new())
            .unwrap()
            .restacked;
        assert_eq!(restacked, vec!["a", "b"]);
        assert!(git.is_ancestor(&new_main, "a").unwrap());
        assert!(git.is_ancestor("a", "b").unwrap());
        // Recorded base hash for `a` advanced to main's new tip.
        assert_eq!(state.branches["a"].base.hash, new_main);
    }

    #[test]
    fn restack_is_idempotent() {
        let (tmp, git) = init_repo();
        let (_store, mut state) = linear_stack(&tmp, &git);
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "m.txt", "m\n", "main moves");

        restack(&git, &mut state, &["a".into(), "b".into()], &mut Vec::new()).unwrap();
        // Second pass: everything already sits on its base, so nothing moves.
        let again = restack(&git, &mut state, &["a".into(), "b".into()], &mut Vec::new()).unwrap();
        assert!(again.restacked.is_empty());
    }

    #[test]
    fn restack_recovers_via_fork_point_when_recorded_hash_is_stale() {
        let (tmp, git) = init_repo();
        let (_store, mut state) = linear_stack(&tmp, &git);
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "m.txt", "m\n", "main moves");
        let new_main = git.rev_parse("main").unwrap();

        // Corrupt the recorded base hash so the recorded-ancestor check fails and
        // the engine must fall back to `merge-base --fork-point`.
        state.branches.get_mut("a").unwrap().base.hash = "1".repeat(40);

        let restacked = restack(&git, &mut state, &["a".into()], &mut Vec::new()).unwrap().restacked;
        assert_eq!(restacked, vec!["a"]);
        assert!(git.is_ancestor(&new_main, "a").unwrap());
    }

    #[test]
    fn restack_skips_a_branch_with_no_git_ref() {
        let (tmp, git) = init_repo();
        let (_store, mut state) = linear_stack(&tmp, &git); // a on main, b on a (real)
        // A tracked branch whose git ref does not exist (a deleted-but-tracked ghost).
        state.branches.insert(
            "ghost".to_string(),
            BranchState {
                base: Base {
                    name: "main".into(),
                    hash: "0".repeat(40),
                },
                pr: None,
            },
        );
        // Advance main so the real branches need restacking.
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "m.txt", "m\n", "main moves");

        // The ghost is skipped (not fatal); the real branches still restack.
        let outcome = restack(
            &git,
            &mut state,
            &["a".into(), "b".into(), "ghost".into()],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(outcome.skipped, vec!["ghost"]);
        assert!(outcome.restacked.contains(&"a".to_string()), "real branches restack");
        assert!(git.is_ancestor("main", "a").unwrap());
    }

    #[test]
    fn restack_skips_a_branch_whose_base_ref_is_gone() {
        let (tmp, git) = init_repo();
        let (_store, mut state) = linear_stack(&tmp, &git); // a on main, b on a (real)
        // Delete `a`'s git ref (the base of `b`); `b`'s own ref survives.
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        run_git(tmp.path(), &["branch", "-D", "a"]);

        let outcome = restack(&git, &mut state, &["a".into(), "b".into()], &mut Vec::new()).unwrap();
        // `b` is skipped because its base `a` no longer resolves (the missing-base leg).
        assert!(
            outcome.skipped.contains(&"b".to_string()),
            "b skipped via missing base: {:?}",
            outcome.skipped
        );
        assert!(outcome.restacked.is_empty());
    }

    #[test]
    fn restack_conflict_queue_excludes_a_preceding_skipped_ghost() {
        let (tmp, git) = init_repo();
        let (_store, mut state) = linear_stack(&tmp, &git);
        // A ghost (no git ref) tracked ahead of the conflict in the queue.
        state.branches.insert(
            "ghost".to_string(),
            BranchState {
                base: Base {
                    name: "main".into(),
                    hash: "0".repeat(40),
                },
                pr: None,
            },
        );
        // Make `a` conflict with main on a shared file.
        write_commit(tmp.path(), "a.txt", "a-conflict\n", "a edits shared");
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "a.txt", "main-conflict\n", "main edits shared");

        let err = restack(&git, &mut state, &["ghost".into(), "a".into(), "b".into()], &mut Vec::new())
            .unwrap_err();
        match err {
            OpsError::Conflict { branch, remaining } => {
                assert_eq!(branch, "a");
                // The ghost is consumed by the skip, not stranded in the resume queue.
                assert_eq!(remaining, vec!["a", "b"]);
            }
            other => panic!("expected conflict, got {other:?}"),
        }
        git.rebase_abort().unwrap();
    }

    #[test]
    fn restack_conflict_returns_remaining_queue() {
        let (tmp, git) = init_repo();
        let (_store, mut state) = linear_stack(&tmp, &git);

        // Make `a` and `main` edit the same file so rebasing `a` onto main conflicts.
        write_commit(tmp.path(), "a.txt", "a-conflict\n", "a edits shared");
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "a.txt", "main-conflict\n", "main edits shared");

        let err = restack(&git, &mut state, &["a".into(), "b".into()], &mut Vec::new()).unwrap_err();
        match err {
            OpsError::Conflict { branch, remaining } => {
                assert_eq!(branch, "a");
                assert_eq!(remaining, vec!["a", "b"]); // unfinished queue, conflicting branch first
            }
            other => panic!("expected conflict, got {other:?}"),
        }
        assert!(git.rebase_in_progress());
        // Persistence moved to the CLI's `restack_with_recovery`; the engine no
        // longer saves, so there is nothing to assert on the store here.
        git.rebase_abort().unwrap();
    }

    #[test]
    fn restack_errors_when_fork_point_unrecoverable() {
        let (tmp, git) = init_repo();
        let path = tmp.path();
        // `a` is an orphan with no shared history with main, so
        // `merge-base --fork-point main a` cannot recover a divergence point.
        run_git(path, &["checkout", "-q", "--orphan", "a"]);
        write_commit(path, "a.txt", "a\n", "orphan a");
        run_git(path, &["checkout", "-q", "main"]);
        write_commit(path, "m.txt", "m\n", "main moves");

        let mut branches = BTreeMap::new();
        branches.insert(
            "a".to_string(),
            BranchState {
                base: Base {
                    name: "main".into(),
                    hash: "1".repeat(40),
                },
                pr: None,
            },
        );
        let mut state = State {
            repo: None,
            branches,
            ..State::default()
        };
        let err = restack(&git, &mut state, &["a".into()], &mut Vec::new()).unwrap_err();
        assert!(matches!(err, OpsError::ForkPointLost { ref branch, .. } if branch == "a"));
    }

    #[test]
    fn topo_order_drops_orphans_unreachable_from_trunk() {
        // `orphan`'s base is neither trunk nor a tracked branch.
        let branches = branch_state(&[("a", "main"), ("orphan", "ghost")]);
        assert_eq!(topo_order(&branches, "main"), vec!["a"]);
    }

    #[test]
    fn upstack_order_handles_branched_upstack() {
        // a -> main, b -> a, c -> a (two children of `a`).
        let branches = branch_state(&[("b", "a"), ("c", "a"), ("a", "main")]);
        let order = upstack_order(&branches, "a");
        assert_eq!(order[0], "a");
        assert_eq!(order.len(), 3);
        assert!(order.contains(&"b".to_string()) && order.contains(&"c".to_string()));
    }

    #[test]
    fn resolve_base_skips_consecutive_merged() {
        // a -> main, b -> a, c -> b; a and b both merged -> resolve to main.
        let branches = branch_state(&[("c", "b"), ("b", "a"), ("a", "main")]);
        let mut merged = BTreeSet::new();
        merged.insert("a".to_string());
        merged.insert("b".to_string());
        assert_eq!(resolve_base(&branches, &merged, "b".into()), "main");
    }
}
