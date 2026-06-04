//! The stack-operations engine: ordering the tracked branches and restacking
//! them onto their bases. These functions are forge-agnostic, they speak only
//! `git` and the state ref, so `submit`, `sync`, and `restack` (and later
//! `modify`/`move`/`merge`) all share one implementation.
//!
//! Conflict *recovery* (the `.git/stacc-continue.json` continuation and the
//! GitHub-enriched conflict-context file) deliberately lives in the CLI crate:
//! `restack` reports a conflict structurally (the branch plus the unfinished
//! queue) and the caller persists it. That keeps `stacc-core` off `stacc-github`.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use stacc_git::{Git, GitError, RebaseError};
use stacc_state::{BranchState, State, StateError, StateStore};
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

/// Restack `order` bottom-up, rebasing each branch onto its base's current tip
/// and updating its recorded base hash in `state`. On a conflict it saves
/// `state` and returns [`OpsError::Conflict`] with the unfinished queue, leaving
/// the rebase in progress. The caller MUST persist a continuation (or abort the
/// rebase) before returning to the user, see `restack_with_recovery`.
pub fn restack(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    order: &[String],
) -> Result<Vec<String>, OpsError> {
    let mut restacked = Vec::new();
    for (idx, branch) in order.iter().enumerate() {
        let Some(base) = state.branches.get(branch).map(|b| b.base.clone()) else {
            continue;
        };
        let base_tip = git.rev_parse(&base.name)?;
        if git.is_ancestor(&base_tip, branch)? {
            continue; // already on top of its base
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
                store.save(state)?;
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
        restacked.push(branch.clone());
    }
    Ok(restacked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stacc_state::{Base, RepoConfig};
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
        };
        assert!(matches!(
            downstack_chain(&state, "a", "main"),
            Err(OpsError::Cycle(_))
        ));

        let state = State {
            repo: None,
            branches: branch_state(&[("a", "missing")]),
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
        let (store, mut state) = linear_stack(&tmp, &git);

        // Advance main out from under the stack.
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "m.txt", "m\n", "main moves");
        let new_main = git.rev_parse("main").unwrap();

        let restacked = restack(&git, &store, &mut state, &["a".into(), "b".into()]).unwrap();
        assert_eq!(restacked, vec!["a", "b"]);
        assert!(git.is_ancestor(&new_main, "a").unwrap());
        assert!(git.is_ancestor("a", "b").unwrap());
        // Recorded base hash for `a` advanced to main's new tip.
        assert_eq!(state.branches["a"].base.hash, new_main);
    }

    #[test]
    fn restack_is_idempotent() {
        let (tmp, git) = init_repo();
        let (store, mut state) = linear_stack(&tmp, &git);
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "m.txt", "m\n", "main moves");

        restack(&git, &store, &mut state, &["a".into(), "b".into()]).unwrap();
        // Second pass: everything already sits on its base, so nothing moves.
        let again = restack(&git, &store, &mut state, &["a".into(), "b".into()]).unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn restack_recovers_via_fork_point_when_recorded_hash_is_stale() {
        let (tmp, git) = init_repo();
        let (store, mut state) = linear_stack(&tmp, &git);
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "m.txt", "m\n", "main moves");
        let new_main = git.rev_parse("main").unwrap();

        // Corrupt the recorded base hash so the recorded-ancestor check fails and
        // the engine must fall back to `merge-base --fork-point`.
        state.branches.get_mut("a").unwrap().base.hash = "1".repeat(40);

        let restacked = restack(&git, &store, &mut state, &["a".into()]).unwrap();
        assert_eq!(restacked, vec!["a"]);
        assert!(git.is_ancestor(&new_main, "a").unwrap());
    }

    #[test]
    fn restack_conflict_returns_remaining_queue() {
        let (tmp, git) = init_repo();
        let (store, mut state) = linear_stack(&tmp, &git);

        // Make `a` and `main` edit the same file so rebasing `a` onto main conflicts.
        write_commit(tmp.path(), "a.txt", "a-conflict\n", "a edits shared");
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        write_commit(tmp.path(), "a.txt", "main-conflict\n", "main edits shared");

        let err = restack(&git, &store, &mut state, &["a".into(), "b".into()]).unwrap_err();
        match err {
            OpsError::Conflict { branch, remaining } => {
                assert_eq!(branch, "a");
                assert_eq!(remaining, vec!["a", "b"]); // unfinished queue, conflicting branch first
            }
            other => panic!("expected conflict, got {other:?}"),
        }
        assert!(git.rebase_in_progress());
        // The conflict path saved state so `sync --continue` can resume.
        assert_eq!(store.load().unwrap().branches.len(), 2);
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
        };
        let store = StateStore::new(git.clone());

        let err = restack(&git, &store, &mut state, &["a".into()]).unwrap_err();
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
