//! Typed wrappers over the `git` command line.

mod error;

pub use error::GitError;

use std::path::PathBuf;
use std::process::Command;

/// A handle to a git repository on disk. Every method shells out to
/// `git -C <dir> …`.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
}

impl Git {
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;

        if !output.status.success() {
            return Err(GitError::Command {
                args: args.iter().map(|s| s.to_string()).collect(),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// The current branch name (e.g. `main`).
    pub fn current_branch(&self) -> Result<String, GitError> {
        self.run(&["symbolic-ref", "--short", "HEAD"])
    }

    /// Resolve a revision to its full commit hash.
    pub fn rev_parse(&self, rev: &str) -> Result<String, GitError> {
        self.run(&["rev-parse", rev])
    }

    /// The merge base (most recent common ancestor) of two commits.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<String, GitError> {
        self.run(&["merge-base", a, b])
    }

    /// Whether `ancestor` is an ancestor of `descendant`.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, GitError> {
        // `git merge-base --is-ancestor` reports the answer via its exit code:
        // 0 = yes, 1 = no, anything else = a real error.
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .output()
            .map_err(|source| GitError::Spawn { source })?;

        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            status => Err(GitError::Command {
                args: vec![
                    "merge-base".into(),
                    "--is-ancestor".into(),
                    ancestor.into(),
                    descendant.into(),
                ],
                status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn init_repo() -> (TempDir, Git) {
        let tmp = TempDir::new().expect("temp dir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        run_git(tmp.path(), &["config", "user.name", "Test"]);
        run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
        let repo = Git::open(tmp.path());
        (tmp, repo)
    }

    #[test]
    fn current_branch_is_main() {
        let (_tmp, repo) = init_repo();
        assert_eq!(repo.current_branch().unwrap(), "main");
    }

    #[test]
    fn rev_parse_resolves_head() {
        let (_tmp, repo) = init_repo();
        assert_eq!(repo.rev_parse("HEAD").unwrap().len(), 40);
    }

    #[test]
    fn is_ancestor_detects_lineage() {
        let (tmp, repo) = init_repo();
        let first = repo.rev_parse("HEAD").unwrap();
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "second"]);
        let second = repo.rev_parse("HEAD").unwrap();

        assert!(repo.is_ancestor(&first, &second).unwrap());
        assert!(!repo.is_ancestor(&second, &first).unwrap());
    }

    #[test]
    fn command_error_on_bad_rev() {
        let (_tmp, repo) = init_repo();
        let err = repo.rev_parse("no-such-ref").unwrap_err();
        assert!(matches!(err, GitError::Command { .. }));
    }
}
