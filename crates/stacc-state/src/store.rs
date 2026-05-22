//! Reading and writing the stack state to the `refs/stacc/` git ref.

use std::collections::BTreeMap;

use stacc_git::{Git, GitError};
use thiserror::Error;

use crate::model::{BranchState, RepoConfig};

const STATE_REF: &str = "refs/stacc/data";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const SAVE_ATTEMPTS: usize = 5;

#[derive(Debug, Error)]
pub enum StateError {
    #[error(transparent)]
    Git(#[from] GitError),

    #[error("failed to (de)serialize state: {0}")]
    Json(#[from] serde_json::Error),
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
        if self.git.ref_commit(&self.git_ref)?.is_none() {
            return Ok(State::default());
        }

        let repo = match self.git.read_blob(&self.git_ref, "repo")? {
            Some(json) => Some(serde_json::from_str(&json)?),
            None => None,
        };

        let mut branches = BTreeMap::new();
        for name in self.git.list_tree(&self.git_ref, "branches")? {
            let path = format!("branches/{name}");
            if let Some(json) = self.git.read_blob(&self.git_ref, &path)? {
                branches.insert(name, serde_json::from_str(&json)?);
            }
        }

        Ok(State { repo, branches })
    }

    /// Serialize `state` to blobs, assemble a tree, and atomically move the ref
    /// to a new commit. Retries on a compare-and-swap miss.
    pub fn save(&self, state: &State) -> Result<(), StateError> {
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

        let mut last_err = None;
        for _ in 0..SAVE_ATTEMPTS {
            let parent = self.git.ref_commit(&self.git_ref)?;
            let commit = self
                .git
                .commit_tree(&tree, parent.as_deref(), "stacc: update state")?;
            let expected_old = parent.as_deref().or(Some(ZERO_OID));
            match self.git.update_ref(&self.git_ref, &commit, expected_old) {
                Ok(()) => return Ok(()),
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.expect("loop runs at least once").into())
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
}
