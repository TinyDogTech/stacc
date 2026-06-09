//! Reading and writing the stack state to the `refs/stacc/` git ref.

use std::collections::BTreeMap;

use stacc_git::{Git, GitError};
use thiserror::Error;

use crate::model::{BranchState, RepoConfig};

const STATE_REF: &str = "refs/stacc/data";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
// Retry budget for a compare-and-swap on the state ref. Paired with jittered
// backoff (see `backoff`) so a burst of concurrent writers de-synchronizes
// instead of exhausting the budget in lockstep.
const SAVE_ATTEMPTS: usize = 10;

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
}

/// The whole stack state, held in memory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct State {
    pub repo: Option<RepoConfig>,
    pub branches: BTreeMap<String, BranchState>,
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

        Ok(State { repo, branches })
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

        let entries: Vec<(&str, &str)> = blobs
            .iter()
            .map(|(path, hash)| (path.as_str(), hash.as_str()))
            .collect();
        let tree = self.git.write_tree(&entries)?;
        let commit = self.git.commit_tree(&tree, parent, "stacc: update state")?;
        Ok(commit)
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
    use crate::model::{Base, PullRequest};
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
            },
        );
        State {
            repo: Some(RepoConfig {
                trunk: "main".into(),
                remote: "origin".into(),
            }),
            branches,
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
}
