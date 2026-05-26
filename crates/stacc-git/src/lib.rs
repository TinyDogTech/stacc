//! Typed wrappers over the `git` command line.

mod error;

pub use error::{GitError, RebaseError, RebaseInterrupt};

use std::path::PathBuf;
use std::process::{Command, Output};

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

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir).args(args);
        // stacc is non-interactive: git must never open an editor.
        cmd.env("GIT_EDITOR", "true");
        cmd
    }

    fn command_error(&self, args: &[&str], output: &Output) -> GitError {
        GitError::Command {
            args: args.iter().map(|s| s.to_string()).collect(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let output = self
            .command(args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;

        if !output.status.success() {
            return Err(self.command_error(args, &output));
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
        let args = ["merge-base", "--is-ancestor", ancestor, descendant];
        let output = self
            .command(&args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;

        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self.command_error(&args, &output)),
        }
    }

    /// The point at which `branch` forked from `base`, or `None` if git can't
    /// determine one from the reflog.
    pub fn fork_point(&self, base: &str, branch: &str) -> Result<Option<String>, GitError> {
        let args = ["merge-base", "--fork-point", base, branch];
        let output = self
            .command(&args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;

        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(self.command_error(&args, &output)),
        }
    }

    /// Rebase `branch` onto `onto`, replaying the commits after `upstream`.
    pub fn rebase_onto(&self, onto: &str, upstream: &str, branch: &str) -> Result<(), RebaseError> {
        let args = ["rebase", "--onto", onto, upstream, branch, "--autostash"];
        let output = self
            .command(&args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;

        if output.status.success() {
            return Ok(());
        }
        if let Some(stopped) = self.rebase_head_branch() {
            return Err(RebaseInterrupt { branch: stopped }.into());
        }
        Err(self.command_error(&args, &output).into())
    }

    /// Continue an in-progress rebase after conflicts are resolved.
    pub fn rebase_continue(&self) -> Result<(), RebaseError> {
        let args = ["rebase", "--continue"];
        let output = self
            .command(&args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;

        if output.status.success() {
            return Ok(());
        }
        if let Some(stopped) = self.rebase_head_branch() {
            return Err(RebaseInterrupt { branch: stopped }.into());
        }
        Err(self.command_error(&args, &output).into())
    }

    /// Abort an in-progress rebase, restoring the prior state.
    pub fn rebase_abort(&self) -> Result<(), GitError> {
        self.run(&["rebase", "--abort"]).map(|_| ())
    }

    /// Whether a rebase is currently in progress.
    pub fn rebase_in_progress(&self) -> bool {
        match self.git_dir() {
            Ok(dir) => dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists(),
            Err(_) => false,
        }
    }

    /// Push `refspec` to `remote`.
    pub fn push(&self, remote: &str, refspec: &str) -> Result<(), GitError> {
        self.run(&["push", remote, refspec]).map(|_| ())
    }

    /// Fetch `refspec` from `remote`.
    pub fn fetch(&self, remote: &str, refspec: &str) -> Result<(), GitError> {
        self.run(&["fetch", remote, refspec]).map(|_| ())
    }

    fn git_dir(&self) -> Result<PathBuf, GitError> {
        let dir = self.run(&["rev-parse", "--git-dir"])?;
        Ok(self.dir.join(dir))
    }

    fn rebase_head_branch(&self) -> Option<String> {
        let head_name = self.git_dir().ok()?.join("rebase-merge").join("head-name");
        let contents = std::fs::read_to_string(head_name).ok()?;
        let name = contents.trim();
        Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_string())
    }
}

impl Git {
    /// Write `content` as a blob object and return its hash.
    pub fn hash_object(&self, content: &[u8]) -> Result<String, GitError> {
        self.run_with_stdin(&["hash-object", "-w", "--stdin"], content)
    }

    /// Build a tree from `(path, blob_hash)` entries and return its hash.
    /// Paths may contain `/`; git creates the intermediate subtrees.
    pub fn write_tree(&self, entries: &[(&str, &str)]) -> Result<String, GitError> {
        let index = self
            .git_dir()?
            .join(format!("stacc-index-{}", std::process::id()));

        let build = || -> Result<String, GitError> {
            for (path, hash) in entries {
                let cacheinfo = format!("100644,{hash},{path}");
                let args = ["update-index", "--add", "--cacheinfo", cacheinfo.as_str()];
                let output = self
                    .command(&args)
                    .env("GIT_INDEX_FILE", &index)
                    .output()
                    .map_err(|source| GitError::Spawn { source })?;
                if !output.status.success() {
                    return Err(self.command_error(&args, &output));
                }
            }
            let args = ["write-tree"];
            let output = self
                .command(&args)
                .env("GIT_INDEX_FILE", &index)
                .output()
                .map_err(|source| GitError::Spawn { source })?;
            if !output.status.success() {
                return Err(self.command_error(&args, &output));
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        };

        let result = build();
        let _ = std::fs::remove_file(&index);
        result
    }

    /// Create a commit object for `tree`, optionally parented on `parent`.
    pub fn commit_tree(
        &self,
        tree: &str,
        parent: Option<&str>,
        message: &str,
    ) -> Result<String, GitError> {
        let mut args = vec!["commit-tree", tree, "-m", message];
        if let Some(parent) = parent {
            args.push("-p");
            args.push(parent);
        }
        let output = self
            .command(&args)
            .env("GIT_AUTHOR_NAME", "stacc")
            .env("GIT_AUTHOR_EMAIL", "stacc@localhost")
            .env("GIT_COMMITTER_NAME", "stacc")
            .env("GIT_COMMITTER_EMAIL", "stacc@localhost")
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        if !output.status.success() {
            return Err(self.command_error(&args, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Point `name` at `new`. When `old` is given, the move only succeeds if
    /// the ref currently equals it — a compare-and-swap.
    pub fn update_ref(&self, name: &str, new: &str, old: Option<&str>) -> Result<(), GitError> {
        let mut args = vec!["update-ref", name, new];
        if let Some(old) = old {
            args.push(old);
        }
        self.run(&args).map(|_| ())
    }

    /// The commit a ref points at, or `None` if the ref does not exist.
    pub fn ref_commit(&self, name: &str) -> Result<Option<String>, GitError> {
        let args = ["rev-parse", "--verify", "--quiet", name];
        let output = self
            .command(&args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(self.command_error(&args, &output)),
        }
    }

    /// Read the blob at `<rev>:<path>`, or `None` if it is not present.
    pub fn read_blob(&self, rev: &str, path: &str) -> Result<Option<String>, GitError> {
        let spec = format!("{rev}:{path}");
        let output = self
            .command(&["cat-file", "-p", spec.as_str()])
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        if output.status.success() {
            Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
        } else {
            Ok(None)
        }
    }

    /// List the entry names directly under `<rev>:<path>`.
    pub fn list_tree(&self, rev: &str, path: &str) -> Result<Vec<String>, GitError> {
        let spec = format!("{rev}:{path}");
        let output = self
            .command(&["ls-tree", "--name-only", spec.as_str()])
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        if !output.status.success() {
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.to_string())
            .collect())
    }

    /// The configured remote names (e.g. `["origin"]`).
    pub fn remotes(&self) -> Result<Vec<String>, GitError> {
        Ok(self
            .run(&["remote"])?
            .lines()
            .map(|line| line.to_string())
            .collect())
    }

    /// The URL configured for `remote` (e.g. its fetch URL).
    pub fn remote_url(&self, remote: &str) -> Result<String, GitError> {
        self.run(&["remote", "get-url", remote])
    }

    /// Resolve a symbolic ref to its short target, or `None` if `name` is not a
    /// symbolic ref.
    pub fn symbolic_ref(&self, name: &str) -> Result<Option<String>, GitError> {
        let args = ["symbolic-ref", "--quiet", "--short", name];
        let output = self
            .command(&args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(self.command_error(&args, &output)),
        }
    }

    fn run_with_stdin(&self, args: &[&str], input: &[u8]) -> Result<String, GitError> {
        use std::io::Write;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|source| GitError::Spawn { source })?;

        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(input)
            .map_err(|source| GitError::Spawn { source })?;

        let output = child
            .wait_with_output()
            .map_err(|source| GitError::Spawn { source })?;

        if !output.status.success() {
            return Err(self.command_error(args, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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

    fn write_commit(dir: &std::path::Path, file: &str, contents: &str, msg: &str) {
        std::fs::write(dir.join(file), contents).expect("write file");
        run_git(dir, &["add", file]);
        run_git(dir, &["commit", "-q", "-m", msg]);
    }

    /// A repo where `feature` and `main` both edit `conflict.txt` differently,
    /// so rebasing `feature` onto `main` conflicts.
    fn setup_conflict() -> (TempDir, Git, String, String) {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "conflict.txt", "base\n", "add file");
        let base = repo.rev_parse("HEAD").unwrap();
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "conflict.txt", "feature\n", "feature change");
        run_git(path, &["checkout", "-q", "main"]);
        write_commit(path, "conflict.txt", "main\n", "main change");
        let main_tip = repo.rev_parse("HEAD").unwrap();
        (tmp, repo, base, main_tip)
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

    #[test]
    fn rebase_onto_completes_without_conflict() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        let base = repo.rev_parse("HEAD").unwrap();
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "feature.txt", "hi\n", "feature work");
        run_git(path, &["checkout", "-q", "main"]);
        write_commit(path, "main.txt", "hi\n", "main work");
        let main_tip = repo.rev_parse("HEAD").unwrap();

        repo.rebase_onto(&main_tip, &base, "feature").unwrap();
        assert!(repo.is_ancestor(&main_tip, "feature").unwrap());
    }

    #[test]
    fn rebase_onto_reports_conflict_with_branch() {
        let (_tmp, repo, base, main_tip) = setup_conflict();
        let err = repo.rebase_onto(&main_tip, &base, "feature").unwrap_err();
        match err {
            RebaseError::Interrupt(RebaseInterrupt { branch }) => assert_eq!(branch, "feature"),
            other => panic!("expected interrupt, got {other:?}"),
        }
        assert!(repo.rebase_in_progress());
        repo.rebase_abort().unwrap();
        assert!(!repo.rebase_in_progress());
    }

    #[test]
    fn rebase_continue_after_resolution() {
        let (tmp, repo, base, main_tip) = setup_conflict();
        repo.rebase_onto(&main_tip, &base, "feature").unwrap_err();
        std::fs::write(tmp.path().join("conflict.txt"), "resolved\n").unwrap();
        run_git(tmp.path(), &["add", "conflict.txt"]);
        repo.rebase_continue().unwrap();
        assert!(!repo.rebase_in_progress());
        assert!(repo.is_ancestor(&main_tip, "feature").unwrap());
    }

    #[test]
    fn fork_point_found() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        let base = repo.rev_parse("HEAD").unwrap();
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "f.txt", "x\n", "feature");
        assert_eq!(repo.fork_point("main", "feature").unwrap(), Some(base));
    }

    #[test]
    fn push_and_fetch_roundtrip() {
        let (tmp, repo) = init_repo();
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
        repo.push("origin", "main").unwrap();
        repo.fetch("origin", "main").unwrap();
    }

    #[test]
    fn objects_round_trip_through_tree_and_ref() {
        let (_tmp, repo) = init_repo();
        let repo_blob = repo.hash_object(b"{\"trunk\":\"main\"}").unwrap();
        let branch_blob = repo.hash_object(b"{\"base\":\"x\"}").unwrap();
        let tree = repo
            .write_tree(&[
                ("repo", repo_blob.as_str()),
                ("branches/feature", branch_blob.as_str()),
            ])
            .unwrap();
        let commit = repo.commit_tree(&tree, None, "state").unwrap();
        repo.update_ref("refs/stacc/data", &commit, None).unwrap();

        assert_eq!(repo.ref_commit("refs/stacc/data").unwrap(), Some(commit));
        assert_eq!(
            repo.read_blob("refs/stacc/data", "repo").unwrap().as_deref(),
            Some("{\"trunk\":\"main\"}")
        );
        assert_eq!(
            repo.list_tree("refs/stacc/data", "branches").unwrap(),
            vec!["feature".to_string()]
        );
    }

    #[test]
    fn read_blob_absent_is_none() {
        let (_tmp, repo) = init_repo();
        assert_eq!(repo.read_blob("HEAD", "nope").unwrap(), None);
    }

    #[test]
    fn ref_commit_missing_is_none() {
        let (_tmp, repo) = init_repo();
        assert_eq!(repo.ref_commit("refs/stacc/data").unwrap(), None);
    }

    #[test]
    fn update_ref_cas_rejects_stale_old() {
        let (_tmp, repo) = init_repo();
        let blob = repo.hash_object(b"a").unwrap();
        let tree = repo.write_tree(&[("k", blob.as_str())]).unwrap();
        let c1 = repo.commit_tree(&tree, None, "one").unwrap();
        repo.update_ref("refs/stacc/data", &c1, None).unwrap();

        let blob2 = repo.hash_object(b"b").unwrap();
        let tree2 = repo.write_tree(&[("k", blob2.as_str())]).unwrap();
        let c2 = repo.commit_tree(&tree2, Some(c1.as_str()), "two").unwrap();

        let zero = "0000000000000000000000000000000000000000";
        assert!(repo.update_ref("refs/stacc/data", &c2, Some(zero)).is_err());
        repo.update_ref("refs/stacc/data", &c2, Some(c1.as_str()))
            .unwrap();
        assert_eq!(repo.ref_commit("refs/stacc/data").unwrap(), Some(c2));
    }

    #[test]
    fn remotes_lists_configured_remotes() {
        let (tmp, repo) = init_repo();
        assert!(repo.remotes().unwrap().is_empty());
        run_git(
            tmp.path(),
            &["remote", "add", "origin", "https://example.com/r.git"],
        );
        assert_eq!(repo.remotes().unwrap(), vec!["origin".to_string()]);
    }

    #[test]
    fn symbolic_ref_resolves_head_and_rejects_direct() {
        let (_tmp, repo) = init_repo();
        assert_eq!(repo.symbolic_ref("HEAD").unwrap().as_deref(), Some("main"));
        assert_eq!(repo.symbolic_ref("refs/heads/main").unwrap(), None);
    }
}
