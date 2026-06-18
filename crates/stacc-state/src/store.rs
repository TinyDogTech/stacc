//! Reading and writing the stack state to the `refs/stacc/` git ref.

use std::collections::BTreeMap;

use stacc_git::{Git, GitError, RefUpdate};
use thiserror::Error;

use crate::model::{BranchState, Disposal, RepoConfig};

const STATE_REF: &str = "refs/stacc/data";
/// Ref namespace for keep-alive refs: a dropped branch's tip is preserved at
/// `refs/stacc/dropped/<branch>-<tip>` so its commits are not GC-eligible until
/// pruned. Local-only: the push refspec ([`StateStore::push`]) covers
/// `refs/stacc/data` alone, so these never leave the machine.
const DROPPED_REF_PREFIX: &str = "refs/stacc/dropped/";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
// Retry budget for a compare-and-swap on the state ref. Paired with jittered
// backoff (see `backoff`) so a burst of concurrent writers de-synchronizes
// instead of exhausting the budget in lockstep.
const SAVE_ATTEMPTS: usize = 10;
/// How far back `undo` may walk the state-ref commit chain. The bound is
/// enforced at read time by [`StateStore::version_back`] (a shallow walk), not
/// by rewriting history, so the chain is never compacted and nothing is lost.
pub const UNDO_RETENTION: usize = 50;

#[derive(Debug, Error)]
pub enum StateError {
    #[error(transparent)]
    Git(#[from] GitError),

    #[error("failed to (de)serialize state: {0}")]
    Json(#[from] serde_json::Error),

    /// `update` lost the compare-and-swap race on every attempt: another writer
    /// kept advancing `refs/stacc/data` faster than this one could re-apply.
    /// Surfaced rather than silently clobbering the winner.
    #[error("state ref contention: gave up after {attempts} attempts")]
    Contention { attempts: usize },

    /// A walk back through the state-ref history asked to go further than the
    /// retention bound allows. Carries the bound so the caller can explain it.
    #[error("beyond the {bound}-version undo retention window")]
    BeyondRetention { bound: usize },
}

/// The whole stack state, held in memory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct State {
    pub repo: Option<RepoConfig>,
    pub branches: BTreeMap<String, BranchState>,
    /// Records of branches dropped by `stacc merged`, keyed by branch and tip.
    pub disposals: BTreeMap<String, Disposal>,
}

/// Reads and writes [`State`] to a git ref as a tree of JSON blobs.
pub struct StateStore {
    git: Git,
    git_ref: String,
}

impl StateStore {
    pub fn new(git: Git) -> Self {
        Self {
            git,
            git_ref: STATE_REF.to_string(),
        }
    }

    /// Load the current state, or an empty state if the ref doesn't exist yet.
    pub fn load(&self) -> Result<State, StateError> {
        let parent = self.git.ref_commit(&self.git_ref)?;
        self.load_at(parent.as_deref())
    }

    /// Load the [`State`] recorded in a specific version (commit), used by `undo`
    /// to read a prior version off the state-ref history.
    pub fn load_version(&self, commit: &str) -> Result<State, StateError> {
        self.load_at(Some(commit))
    }

    /// Load the state recorded in a specific commit (its tree of JSON blobs), or
    /// an empty state when `commit` is `None`. Reading from a captured commit
    /// hash, rather than re-resolving the ref by name, is what lets [`update`]
    /// load, mutate, and compare-and-swap against one consistent snapshot.
    ///
    /// [`update`]: StateStore::update
    fn load_at(&self, commit: Option<&str>) -> Result<State, StateError> {
        let Some(rev) = commit else {
            return Ok(State::default());
        };

        let repo = match self.git.read_blob(rev, "repo")? {
            Some(json) => Some(serde_json::from_str(&json)?),
            None => None,
        };

        let mut branches = BTreeMap::new();
        for name in self.git.list_tree(rev, "branches")? {
            let path = format!("branches/{name}");
            if let Some(json) = self.git.read_blob(rev, &path)? {
                branches.insert(name, serde_json::from_str(&json)?);
            }
        }

        // The disposals blob is absent in state written before this slice, which
        // reads back as an empty map (back-compatible).
        let disposals = match self.git.read_blob(rev, "disposals")? {
            Some(json) => serde_json::from_str(&json)?,
            None => BTreeMap::new(),
        };

        Ok(State {
            repo,
            branches,
            disposals,
        })
    }

    /// Read-modify-write the state under compare-and-swap, re-applying the
    /// *logical* change on a lost race instead of clobbering the winner.
    ///
    /// Each attempt loads the state at the current ref tip, runs `mutate`,
    /// commits the result onto that tip, and moves the ref with `expected_old`
    /// set to the tip it loaded. If a concurrent writer advanced the ref first,
    /// the compare-and-swap fails; `update` reloads the now-current state,
    /// re-applies `mutate`, and retries, up to `SAVE_ATTEMPTS`. Because `mutate`
    /// is replayed against fresh state, two writers changing *different* branches
    /// both survive (the isolation-first case). Returns the closure's value, or
    /// [`StateError::Contention`] if every attempt lost the race.
    ///
    /// This is the single seam where a future logical merge would replace the
    /// naive replay for the shared-branch case.
    pub fn update<T>(
        &self,
        mut mutate: impl FnMut(&mut State) -> Result<T, StateError>,
    ) -> Result<T, StateError> {
        for attempt in 0..SAVE_ATTEMPTS {
            let parent = self.git.ref_commit(&self.git_ref)?;
            let mut state = self.load_at(parent.as_deref())?;
            let value = mutate(&mut state)?;
            let commit = self.commit_state(&state, parent.as_deref())?;
            let expected_old = parent.as_deref().or(Some(ZERO_OID));
            match self.git.update_ref(&self.git_ref, &commit, expected_old) {
                Ok(()) => return Ok(value),
                Err(err) => {
                    // Tell a lost compare-and-swap (retry) apart from a real git
                    // failure (propagate): if the ref still points where we built,
                    // the move did not fail on contention, so surface the error.
                    // Otherwise the ref advanced; reload and re-apply next pass.
                    if self.git.ref_commit(&self.git_ref)?.as_deref() == parent.as_deref() {
                        return Err(err.into());
                    }
                    // CAS miss: back off with jitter so contending writers spread
                    // out rather than colliding on every retry. No point sleeping
                    // after the last attempt, the loop is about to give up.
                    if attempt + 1 < SAVE_ATTEMPTS {
                        backoff(attempt);
                    }
                }
            }
        }
        Err(StateError::Contention {
            attempts: SAVE_ATTEMPTS,
        })
    }

    /// Overwrite the whole stored state in one transactional write: a thin
    /// wrapper over [`update`] whose closure ignores the reloaded state and
    /// replaces it wholesale, so a concurrent writer's change is overwritten
    /// rather than merged. Callers applying a *logical* change should prefer
    /// [`update`] directly; `save` is whole-state replacement, used by the
    /// store's roundtrip tests.
    ///
    /// [`update`]: StateStore::update
    pub fn save(&self, state: &State) -> Result<(), StateError> {
        self.update(|s| {
            *s = state.clone();
            Ok(())
        })
    }

    /// Write `state` as a tree of JSON blobs in a new commit parented on
    /// `parent`, returning the commit hash. Shared by [`save`] and [`update`].
    ///
    /// [`save`]: StateStore::save
    /// [`update`]: StateStore::update
    fn commit_state(&self, state: &State, parent: Option<&str>) -> Result<String, StateError> {
        let mut blobs: Vec<(String, String)> = Vec::new();
        if let Some(repo) = &state.repo {
            let json = serde_json::to_string_pretty(repo)?;
            blobs.push(("repo".to_string(), self.git.hash_object(json.as_bytes())?));
        }
        for (name, branch) in &state.branches {
            let json = serde_json::to_string_pretty(branch)?;
            blobs.push((
                format!("branches/{name}"),
                self.git.hash_object(json.as_bytes())?,
            ));
        }

        // Disposal records, omitted when empty so unrelated state stays clean and
        // old readers that don't know the blob see no change.
        if !state.disposals.is_empty() {
            let json = serde_json::to_string_pretty(&state.disposals)?;
            blobs.push((
                "disposals".to_string(),
                self.git.hash_object(json.as_bytes())?,
            ));
        }

        // Snapshot each tracked branch's tip so `undo` can restore it. A branch
        // with no git ref is recorded absent (omitted from the map).
        let mut tips: BTreeMap<String, String> = BTreeMap::new();
        for name in state.branches.keys() {
            if let Some(tip) = self.git.ref_commit(&format!("refs/heads/{name}"))? {
                tips.insert(name.clone(), tip);
            }
        }
        let tips_json = serde_json::to_string_pretty(&tips)?;
        blobs.push(("tips".to_string(), self.git.hash_object(tips_json.as_bytes())?));

        let entries: Vec<(&str, &str)> = blobs
            .iter()
            .map(|(path, hash)| (path.as_str(), hash.as_str()))
            .collect();
        let tree = self.git.write_tree(&entries)?;
        let commit = self.git.commit_tree(&tree, parent, "stacc: update state")?;
        Ok(commit)
    }

    /// The branch-tip snapshot recorded in `commit` (its `tips` blob): a map of
    /// tracked branch name to the tip it had when that version was written.
    /// Branches with no ref at write time are absent. A version written before
    /// tips were captured yields an empty map.
    pub fn tips_at(&self, commit: &str) -> Result<BTreeMap<String, String>, StateError> {
        match self.git.read_blob(commit, "tips")? {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(BTreeMap::new()),
        }
    }

    /// The commit `steps` versions back from the current state-ref tip, or `None`
    /// if the recorded history does not reach that far. Refuses to look past the
    /// retention bound ([`UNDO_RETENTION`]) with [`StateError::BeyondRetention`],
    /// a shallow walk that never rewrites the chain.
    pub fn version_back(&self, steps: usize) -> Result<Option<String>, StateError> {
        if steps > UNDO_RETENTION {
            return Err(StateError::BeyondRetention {
                bound: UNDO_RETENTION,
            });
        }
        Ok(self.git.ref_commit(&format!("{}~{steps}", self.git_ref))?)
    }

    /// Push the state ref to `remote`.
    pub fn push(&self, remote: &str) -> Result<(), StateError> {
        self.git.push(remote, &format!("{0}:{0}", self.git_ref))?;
        Ok(())
    }

    /// Fetch the state ref from `remote`.
    pub fn fetch(&self, remote: &str) -> Result<(), StateError> {
        self.git.fetch(remote, &format!("{0}:{0}", self.git_ref))?;
        Ok(())
    }

    /// Atomically preserve `branch`'s dropped `tip` at its keep-alive ref and
    /// delete `refs/heads/<branch>` (guarded by `tip`) as one ref transaction:
    /// the branch ref is never removed without the keep-alive landing, so the
    /// dropped commits stay reachable. Meant to run *after* the disposal record
    /// is written to state, so a failure here leaves the branch ref intact and
    /// re-trackable rather than losing it.
    pub fn keep_alive_and_delete(&self, branch: &str, tip: &str) -> Result<(), StateError> {
        self.git.update_refs(&[
            RefUpdate::Create {
                name: dropped_ref(branch, tip),
                new: tip.to_string(),
            },
            RefUpdate::Delete {
                name: format!("refs/heads/{branch}"),
                old: Some(tip.to_string()),
            },
        ])?;
        Ok(())
    }

    /// Idempotently write `branch`'s keep-alive ref at `tip` WITHOUT touching the
    /// branch ref. Used on the conflict path of a dispose, where the children
    /// restack stops before the branch ref can be removed: the dropped tip is
    /// preserved immediately so recovery works even though `refs/heads/<branch>`
    /// lingers until the user removes it. Setting the ref to `tip` (which it
    /// always points at) is idempotent, so a retry or a re-drop is safe.
    pub fn preserve_tip(&self, branch: &str, tip: &str) -> Result<(), StateError> {
        self.git.update_ref(&dropped_ref(branch, tip), tip, None)?;
        Ok(())
    }

    /// Delete keep-alive refs beyond the [`UNDO_RETENTION`] cap, oldest *drop*
    /// first, so `refs/stacc/dropped/*` cannot grow without bound. Ordered by each
    /// disposal's recorded drop time, NOT the dropped tip's commit date: a
    /// long-lived branch can have an old tip, and ordering by commit date could
    /// prune the just-dropped ref while keeping stale ones. Refs with no disposal
    /// record (orphans, or pre-slice drops) have drop time 0 and sort oldest.
    pub fn prune_dropped(&self) -> Result<(), StateError> {
        let state = self.load()?;
        let mut refs = self.git.list_refs(DROPPED_REF_PREFIX)?;
        refs.sort_by_key(|r| std::cmp::Reverse(drop_time(&state, r)));
        for stale in refs.iter().skip(UNDO_RETENTION) {
            self.git.delete_ref(stale, None)?;
        }
        Ok(())
    }
}

/// The recorded drop time (unix millis) of the disposal whose keep-alive ref is
/// `ref_name`, or 0 when no disposal matches (an orphan or a pre-slice ref), so
/// such refs sort oldest under retention.
fn drop_time(state: &State, ref_name: &str) -> u64 {
    state
        .disposals
        .values()
        .find(|d| dropped_ref(&d.branch, &d.tip) == ref_name)
        .map_or(0, |d| d.dropped_at)
}

/// The keep-alive ref name for `branch` dropped at `tip`. Recovery re-creates
/// the branch at this ref.
pub fn dropped_ref(branch: &str, tip: &str) -> String {
    format!("{DROPPED_REF_PREFIX}{branch}-{tip}")
}

/// Exponential backoff with a few ms of jitter between lost compare-and-swap
/// attempts, so writers that keep colliding spread out rather than retrying in
/// lockstep. The jitter is seeded from the wall clock; its precision does not
/// matter, only that two racing processes pick different sleeps.
fn backoff(attempt: usize) {
    let base = 1u64 << attempt.min(6); // 1, 2, 4, ... capped at 64 ms
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()) % 8);
    std::thread::sleep(std::time::Duration::from_millis(base + jitter));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Base, Disposal, PullRequest};
    use tempfile::TempDir;

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo() -> (TempDir, StateStore) {
        let tmp = TempDir::new().expect("temp dir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        run_git(tmp.path(), &["config", "user.name", "Test"]);
        run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
        let store = StateStore::new(Git::open(tmp.path()));
        (tmp, store)
    }

    fn sample_state() -> State {
        let mut branches = BTreeMap::new();
        branches.insert(
            "feature".to_string(),
            BranchState {
                base: Base {
                    name: "main".into(),
                    hash: "deadbeef".into(),
                },
                pr: Some(PullRequest {
                    number: 3,
                    url: None,
                }),
                pr_title: None,
                pr_description: None,
            },
        );
        State {
            repo: Some(RepoConfig {
                trunk: "main".into(),
                remote: "origin".into(),
            }),
            branches,
            ..State::default()
        }
    }

    #[test]
    fn load_on_fresh_repo_is_empty() {
        let (_tmp, store) = init_repo();
        assert_eq!(store.load().unwrap(), State::default());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let (_tmp, store) = init_repo();
        let state = sample_state();
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
    }

    #[test]
    fn save_updates_existing_state() {
        let (_tmp, store) = init_repo();
        store.save(&sample_state()).unwrap();
        let mut updated = sample_state();
        updated.branches.get_mut("feature").unwrap().base.hash = "cafef00d".into();
        store.save(&updated).unwrap();
        assert_eq!(store.load().unwrap(), updated);
    }

    #[test]
    fn state_survives_push_and_fetch() {
        let (tmp, store) = init_repo();
        store.save(&sample_state()).unwrap();

        let remote = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(remote.path())
            .status()
            .expect("init bare");
        run_git(
            tmp.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        store.push("origin").unwrap();

        run_git(tmp.path(), &["update-ref", "-d", "refs/stacc/data"]);
        assert_eq!(store.load().unwrap(), State::default());

        store.fetch("origin").unwrap();
        assert_eq!(store.load().unwrap(), sample_state());
    }

    #[test]
    fn slashed_branch_name_roundtrips() {
        let (_tmp, store) = init_repo();
        let mut state = State {
            repo: Some(RepoConfig {
                trunk: "main".into(),
                remote: "origin".into(),
            }),
            ..State::default()
        };
        // A `user/feature`-style name nests under branches/ in the tree.
        state.branches.insert(
            "jillian/foo".to_string(),
            BranchState {
                base: Base {
                    name: "main".into(),
                    hash: "abc".into(),
                },
                pr: None,
                pr_title: None,
                pr_description: None,
            },
        );
        store.save(&state).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, state);
        assert!(loaded.branches.contains_key("jillian/foo"));
    }

    fn branch_on_main() -> BranchState {
        BranchState {
            base: Base {
                name: "main".into(),
                hash: "0".repeat(40),
            },
            pr: None,
            pr_title: None,
            pr_description: None,
        }
    }

    #[test]
    fn update_inserts_and_roundtrips() {
        let (_tmp, store) = init_repo();
        store
            .update(|s| {
                s.repo = Some(RepoConfig {
                    trunk: "main".into(),
                    remote: "origin".into(),
                });
                s.branches.insert("feature".into(), branch_on_main());
                Ok(())
            })
            .unwrap();

        let loaded = store.load().unwrap();
        assert!(loaded.branches.contains_key("feature"));
        assert_eq!(loaded.repo.unwrap().trunk, "main");
    }

    #[test]
    fn update_reapplies_its_change_after_losing_a_race() {
        // Characterizes the fix for the lost-update bug. With the old whole-state
        // `save`, a concurrent writer's branch is clobbered; `update` re-applies
        // its logical change onto the winner's state so both survive.
        let (tmp, store) = init_repo();
        let racer = StateStore::new(Git::open(tmp.path()));

        let mut injected = false;
        store
            .update(|state| {
                if !injected {
                    injected = true;
                    // A concurrent writer commits branch "a" between our load and
                    // our compare-and-swap, so our first attempt loses the race.
                    racer
                        .update(|s| {
                            s.branches.insert("a".into(), branch_on_main());
                            Ok(())
                        })
                        .unwrap();
                }
                state.branches.insert("b".into(), branch_on_main());
                Ok(())
            })
            .unwrap();

        let loaded = store.load().unwrap();
        assert!(loaded.branches.contains_key("a"), "the racer's branch survived");
        assert!(
            loaded.branches.contains_key("b"),
            "our re-applied branch is present"
        );
    }

    #[test]
    fn update_reports_contention_when_every_attempt_loses() {
        let (tmp, store) = init_repo();
        let racer = StateStore::new(Git::open(tmp.path()));

        let mut n = 0;
        let result = store.update(|state| {
            // Advance the ref before each of our compare-and-swaps, so every
            // attempt loses and the bounded retry eventually gives up.
            n += 1;
            racer
                .update(|s| {
                    s.branches.insert(format!("r{n}"), branch_on_main());
                    Ok(())
                })
                .unwrap();
            state.branches.insert("mine".into(), branch_on_main());
            Ok(())
        });

        let gave_up = matches!(
            &result,
            Err(StateError::Contention { attempts }) if *attempts == SAVE_ATTEMPTS
        );
        assert!(gave_up, "expected contention after {SAVE_ATTEMPTS}, got {result:?}");
    }

    #[test]
    fn update_propagates_a_closure_error_without_moving_the_ref() {
        let (_tmp, store) = init_repo();
        store
            .update(|s| {
                s.branches.insert("a".into(), branch_on_main());
                Ok(())
            })
            .unwrap();
        let before = store.load().unwrap();

        let result: Result<(), StateError> = store.update(|_state| {
            Err(serde_json::from_str::<i32>("not a number")
                .unwrap_err()
                .into())
        });
        assert!(matches!(result, Err(StateError::Json(_))));

        // The failed closure left the ref untouched.
        assert_eq!(store.load().unwrap(), before);
    }

    fn rev(dir: &std::path::Path, r: &str) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", r])
            .output()
            .expect("spawn git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn update_captures_branch_tips() {
        let (tmp, store) = init_repo();
        // A real branch with a tip.
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
        let feature_tip = rev(tmp.path(), "feature");

        store
            .update(|s| {
                s.branches.insert("feature".into(), branch_on_main());
                Ok(())
            })
            .unwrap();

        let tip_commit = store.version_back(0).unwrap().expect("a version exists");
        let tips = store.tips_at(&tip_commit).unwrap();
        assert_eq!(tips.get("feature"), Some(&feature_tip));
    }

    #[test]
    fn tips_omit_a_branch_with_no_ref() {
        let (_tmp, store) = init_repo();
        store
            .update(|s| {
                s.branches.insert("ghost".into(), branch_on_main());
                Ok(())
            })
            .unwrap();
        let tip = store.version_back(0).unwrap().unwrap();
        assert!(
            !store.tips_at(&tip).unwrap().contains_key("ghost"),
            "a branch with no git ref is omitted from the tips snapshot"
        );
    }

    #[test]
    fn tips_at_is_empty_when_there_is_no_tips_blob() {
        let (tmp, store) = init_repo();
        // A plain git commit (no `tips` path in its tree) reads back empty.
        let head = rev(tmp.path(), "HEAD");
        assert!(store.tips_at(&head).unwrap().is_empty());
    }

    #[test]
    fn version_back_walks_the_chain_and_stops_at_the_root() {
        let (_tmp, store) = init_repo();
        for i in 0..3 {
            store
                .update(|s| {
                    s.branches.insert(format!("b{i}"), branch_on_main());
                    Ok(())
                })
                .unwrap();
        }
        let (v0, v1, v2) = (
            store.version_back(0).unwrap(),
            store.version_back(1).unwrap(),
            store.version_back(2).unwrap(),
        );
        assert!(v0.is_some() && v1.is_some() && v2.is_some());
        assert_ne!(v0, v1);
        assert_ne!(v1, v2);
        // Only three versions exist; walking past the root yields nothing.
        assert_eq!(store.version_back(3).unwrap(), None);
    }

    #[test]
    fn version_back_beyond_retention_is_a_structured_error() {
        let (_tmp, store) = init_repo();
        store
            .update(|s| {
                s.branches.insert("a".into(), branch_on_main());
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            store.version_back(UNDO_RETENTION + 1),
            Err(StateError::BeyondRetention { bound }) if bound == UNDO_RETENTION
        ));
    }

    #[test]
    fn disposals_blob_roundtrips_and_absent_reads_empty() {
        let (_tmp, store) = init_repo();
        // Old-shape state (no disposals) loads with an empty map (back-compat).
        store.save(&sample_state()).unwrap();
        assert!(store.load().unwrap().disposals.is_empty());

        // A disposal record survives a save/load round-trip.
        let mut state = sample_state();
        state.disposals.insert(
            "feature@abc".into(),
            Disposal {
                branch: "feature".into(),
                tip: "abc".into(),
                base: "main".into(),
                children: vec!["child".into()],
                evidence: "net_diff".into(),
                dropped_at: 1234,
            },
        );
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
    }

    #[test]
    fn keep_alive_and_delete_preserves_the_tip_and_removes_the_branch() {
        let (tmp, store) = init_repo();
        let git = Git::open(tmp.path());
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
        let tip = rev(tmp.path(), "feature");
        run_git(tmp.path(), &["checkout", "-q", "main"]);

        store.keep_alive_and_delete("feature", &tip).unwrap();

        assert!(git.ref_missing("refs/heads/feature"), "branch removed");
        let keep = dropped_ref("feature", &tip);
        assert_eq!(git.ref_commit(&keep).unwrap().as_deref(), Some(tip.as_str()));
    }

    #[test]
    fn keep_alive_with_a_stale_tip_leaves_the_branch() {
        let (tmp, store) = init_repo();
        let git = Git::open(tmp.path());
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
        let tip = rev(tmp.path(), "feature");
        run_git(tmp.path(), &["checkout", "-q", "main"]);

        // A wrong tip is the moved-between-check-and-drop case; the guard aborts
        // and the branch ref survives.
        let result = store.keep_alive_and_delete("feature", &"0".repeat(40));
        assert!(result.is_err());
        assert_eq!(
            git.ref_commit("refs/heads/feature").unwrap().as_deref(),
            Some(tip.as_str())
        );
    }

    #[test]
    fn reused_branch_name_gets_distinct_keep_alive_refs() {
        let (tmp, store) = init_repo();
        let git = Git::open(tmp.path());
        // Drop `foo` at one tip, recreate `foo` at a different tip, drop again:
        // the SHA suffix keeps both keep-alive refs distinct.
        run_git(tmp.path(), &["checkout", "-q", "-b", "foo"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "one"]);
        let tip1 = rev(tmp.path(), "foo");
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        store.keep_alive_and_delete("foo", &tip1).unwrap();

        run_git(tmp.path(), &["checkout", "-q", "-b", "foo"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "two"]);
        let tip2 = rev(tmp.path(), "foo");
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        store.keep_alive_and_delete("foo", &tip2).unwrap();

        assert_ne!(tip1, tip2);
        assert_eq!(
            git.ref_commit(&dropped_ref("foo", &tip1)).unwrap().as_deref(),
            Some(tip1.as_str())
        );
        assert_eq!(
            git.ref_commit(&dropped_ref("foo", &tip2)).unwrap().as_deref(),
            Some(tip2.as_str())
        );
    }

    #[test]
    fn prune_dropped_caps_the_namespace() {
        let (tmp, store) = init_repo();
        let git = Git::open(tmp.path());
        let head = rev(tmp.path(), "HEAD");
        // More keep-alive refs than the retention cap, all on the same commit;
        // prune keeps exactly the cap.
        let creates: Vec<RefUpdate> = (0..UNDO_RETENTION + 3)
            .map(|i| RefUpdate::Create {
                name: format!("{DROPPED_REF_PREFIX}b{i}-{head}"),
                new: head.clone(),
            })
            .collect();
        git.update_refs(&creates).unwrap();
        assert_eq!(
            git.list_refs(DROPPED_REF_PREFIX).unwrap().len(),
            UNDO_RETENTION + 3
        );

        store.prune_dropped().unwrap();
        assert_eq!(git.list_refs(DROPPED_REF_PREFIX).unwrap().len(), UNDO_RETENTION);
    }

    #[test]
    fn prune_dropped_keeps_the_newest_drops_not_the_newest_commits() {
        let (tmp, store) = init_repo();
        let git = Git::open(tmp.path());
        // `recent` is committed now; `old` carries an old committer date, so
        // commit-date order disagrees with drop order.
        let recent = rev(tmp.path(), "HEAD");
        std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["commit", "-q", "--allow-empty", "-m", "old"])
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00")
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00")
            .status()
            .expect("spawn git");
        let old = rev(tmp.path(), "HEAD");

        // Fill the retention window with keep-alive refs on the RECENT commit,
        // each recorded as an OLDER drop (dropped_at 1..=UNDO_RETENTION).
        let mut state = State::default();
        let mut creates = Vec::new();
        for i in 0..UNDO_RETENTION {
            let branch = format!("r{i}");
            creates.push(RefUpdate::Create {
                name: dropped_ref(&branch, &recent),
                new: recent.clone(),
            });
            state.disposals.insert(
                format!("{branch}@{recent}"),
                Disposal {
                    branch,
                    tip: recent.clone(),
                    base: "main".into(),
                    children: vec![],
                    evidence: "ancestor".into(),
                    dropped_at: u64::try_from(i).unwrap() + 1,
                },
            );
        }
        // The just-dropped branch: the NEWEST drop, but its tip is the OLD commit.
        creates.push(RefUpdate::Create {
            name: dropped_ref("survivor", &old),
            new: old.clone(),
        });
        state.disposals.insert(
            format!("survivor@{old}"),
            Disposal {
                branch: "survivor".into(),
                tip: old.clone(),
                base: "main".into(),
                children: vec![],
                evidence: "ancestor".into(),
                dropped_at: 9_999,
            },
        );
        git.update_refs(&creates).unwrap();
        store.save(&state).unwrap();
        assert_eq!(git.list_refs(DROPPED_REF_PREFIX).unwrap().len(), UNDO_RETENTION + 1);

        store.prune_dropped().unwrap();

        let kept = git.list_refs(DROPPED_REF_PREFIX).unwrap();
        assert_eq!(kept.len(), UNDO_RETENTION, "prune caps at the retention window");
        // The newest DROP survives even though its tip is the oldest COMMIT: prune
        // orders by drop time, not commit date.
        assert!(
            kept.contains(&dropped_ref("survivor", &old)),
            "the just-dropped ref is kept, not pruned by its old commit date"
        );
        // The oldest drop (r0, dropped_at = 1) is the one evicted.
        assert!(
            !kept.contains(&dropped_ref("r0", &recent)),
            "the oldest drop is pruned"
        );
    }

    #[test]
    fn dropped_refs_are_local_only_and_not_pushed() {
        let (tmp, store) = init_repo();
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
        let tip = rev(tmp.path(), "feature");
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        store.save(&sample_state()).unwrap();
        store.keep_alive_and_delete("feature", &tip).unwrap();

        let remote = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(remote.path())
            .status()
            .expect("init bare");
        run_git(
            tmp.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        store.push("origin").unwrap();

        let remote_git = Git::open(remote.path());
        assert!(
            remote_git.ref_commit("refs/stacc/data").unwrap().is_some(),
            "the data ref is pushed"
        );
        assert!(
            remote_git.list_refs(DROPPED_REF_PREFIX).unwrap().is_empty(),
            "keep-alive refs are local-only"
        );
    }
}
