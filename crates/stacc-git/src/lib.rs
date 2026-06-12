//! Typed wrappers over the `git` command line.

mod error;

pub use error::{GitError, RebaseError, RebaseInterrupt};

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A handle to a git repository on disk. Every method shells out to
/// `git -C <dir> …`.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
}

/// A commit's display metadata: abbreviated hash, subject, and relative age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// Abbreviated commit hash (`%h`), e.g. `bd755054`.
    pub sha: String,
    /// Subject line (`%s`).
    pub subject: String,
    /// Committer date, relative (`%cr`), e.g. `2 hours ago`.
    pub age: String,
}

/// Aggregate diff statistics between two trees, as `git diff --numstat` counts
/// them: the number of changed files and the total inserted and deleted lines.
/// A binary change counts as a changed file with no line counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// A registered git worktree: its working-tree path and the branch checked out
/// there. `branch` is `None` for a detached-HEAD or bare worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// A contiguous range of lines in a diff side, `1`-indexed. `count` is the
/// number of lines on that side of the hunk; an insertion has `count == 0` on
/// the pre-image (old) side, a deletion `count == 0` on the post-image (new)
/// side. For a `0`-count range git reports the line *before* which the change
/// applies, so `start` is that anchor line and the range is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    /// First line of the range (1-indexed). For an empty range this is the
    /// line the change is anchored after.
    pub start: u32,
    /// Number of lines in the range. `0` for a pure insertion (old side) or a
    /// pure deletion (new side).
    pub count: u32,
}

/// How a file changed in a diff, so a hunk consumer can skip what it cannot
/// handle. Absorb needs a pre-image line range to blame, so only [`Modified`]
/// is absorbable; the rest are reported as unsupported, never silently dropped.
///
/// [`Modified`]: HunkKind::Modified
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkKind {
    /// An edit to an existing text file: it has a pre-image to blame.
    Modified,
    /// A newly added file: no pre-image, so its lines blame to nothing.
    Added,
    /// A deleted file: the post-image is empty.
    Deleted,
    /// A binary file change (`Binary files ... differ`): no line hunks.
    Binary,
    /// A rename (detected with `-M`), possibly with content edits. The old path
    /// is in [`Hunk::old_path`]; absorb maps or reports it rather than blaming
    /// the new path, whose lines have no history yet.
    ///
    /// [`Hunk::old_path`]: Hunk::old_path
    Renamed,
}

/// One addressable hunk of a staged diff. A hunk belongs to a single file and
/// carries enough to blame its pre-image (for absorb) or to report why it is
/// unsupported. A binary or pure-rename file with no `@@` body still yields one
/// hunk so the change is reported, never dropped: such a hunk has empty
/// [`old_range`]/[`new_range`] and an empty [`body`].
///
/// [`old_range`]: Hunk::old_range
/// [`new_range`]: Hunk::new_range
/// [`body`]: Hunk::body
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The post-image path (the file as it is now). For a rename this is the
    /// new name; [`old_path`] holds the source.
    ///
    /// [`old_path`]: Hunk::old_path
    pub path: String,
    /// The pre-image path. Equals [`path`] except for a rename, where it is the
    /// source name. `None` for an added file (no pre-image path).
    ///
    /// [`path`]: Hunk::path
    pub old_path: Option<String>,
    /// What kind of change the owning file is, so callers skip the unsupported.
    pub kind: HunkKind,
    /// The pre-image (old) line range this hunk edits, the input the absorb
    /// mapper blames. Empty (`count == 0`) for a pure insertion or a file with
    /// no line body (binary, pure rename).
    pub old_range: LineRange,
    /// The post-image (new) line range this hunk produces.
    pub new_range: LineRange,
    /// The raw hunk body: the lines after the `@@` header (` `/`+`/`-`
    /// prefixed), verbatim and newline-joined. Empty for a file with no `@@`
    /// body. Does not include the `@@` header line itself.
    pub body: String,
}

/// One entry of a commit's tree: a leaf blob with its full path, file mode, and
/// blob hash, as produced by `git ls-tree -r`. The `(path, mode, sha)` shape
/// feeds a tree rebuild; [`write_tree`] currently hard-codes mode `100644`, so a
/// caller preserving an executable or symlink mode needs [`mktree`] instead.
///
/// [`write_tree`]: Git::write_tree
/// [`mktree`]: Git::mktree
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Full path from the tree root, e.g. `src/lib.rs`.
    pub path: String,
    /// Git file mode, e.g. `100644` (file), `100755` (executable), `120000`
    /// (symlink).
    pub mode: String,
    /// The blob object hash.
    pub sha: String,
}

/// Evidence that a branch's changes already live in a trunk, returned by
/// [`Git::merge_equivalence`]. Ordered by strength: the first two variants are
/// deterministic proofs that the branch is contained in trunk; the third is a
/// heuristic, propose-only signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeEquivalence {
    /// The branch tip is an ancestor of trunk: every branch commit is already
    /// reachable from trunk (a plain or fast-forward merge).
    Ancestor,
    /// The branch tip's tree equals trunk's tree: the branch produced exactly
    /// trunk's current content (a squash where trunk has not advanced since).
    SameTree,
    /// The branch's net diff matches the patch-id of a commit already on trunk
    /// (a squash, even after trunk advanced on other files). Propose-only: it
    /// has known false positives (a reverted-then-overwritten change, an
    /// independent backport of the same diff), so a caller must not treat it as
    /// proof for a destructive drop.
    NetDiff,
    /// No evidence that the branch is contained in trunk.
    NotFound,
}

/// Diff options pinned for patch-id comparability. Both sides of a comparison
/// must use identical options for their ids to match: the `--no-*` flags strip
/// rename detection and config- or environment-dependent filters, and `-U0`
/// drops context lines so an unrelated trunk change on a neighbouring line does
/// not perturb the hash.
const PATCH_ID_DIFF_OPTS: &[&str] = &["--no-renames", "--no-ext-diff", "--no-textconv", "-U0"];

impl Git {
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir).args(args);
        // stacc is non-interactive: git must never open an editor or prompt for
        // credentials (fail fast instead).
        cmd.env("GIT_EDITOR", "true");
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        // Force the C locale so machine-read output (notably `log`'s relative
        // date `%cr`) is stable English regardless of the user's locale. Every
        // current caller parses exit codes, hashes, refnames, or `--format`
        // codes, none of which depend on locale, so this only stabilizes output.
        cmd.env("LC_ALL", "C");
        cmd
    }

    fn command_error(&self, args: &[&str], output: &Output) -> GitError {
        GitError::Command {
            args: args.iter().map(ToString::to_string).collect(),
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

    /// Whether `a` and `b` resolve to commits with identical trees (the same
    /// content), even when the commits themselves differ. Used to spot a branch
    /// already contained in its base (e.g. squash-merged) without a rebase.
    pub fn same_tree(&self, a: &str, b: &str) -> Result<bool, GitError> {
        Ok(self.rev_parse(&format!("{a}^{{tree}}"))? == self.rev_parse(&format!("{b}^{{tree}}"))?)
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

    /// Report the strongest evidence that `branch`'s changes already appear in
    /// `trunk`. Checks in order of strength: [`is_ancestor`](Git::is_ancestor)
    /// (a plain merge), [`same_tree`](Git::same_tree) (a squash where trunk has
    /// not advanced), then a net-diff patch-id scan of the commits trunk gained
    /// since the fork point (a squash after trunk advanced). The scan is bounded
    /// to `merge_base(trunk, branch)..trunk`, never all of trunk.
    ///
    /// [`MergeEquivalence::NetDiff`] is propose-only and must not authorize a
    /// destructive drop on its own; the two deterministic signals can.
    pub fn merge_equivalence(
        &self,
        trunk: &str,
        branch: &str,
    ) -> Result<MergeEquivalence, GitError> {
        if self.is_ancestor(branch, trunk)? {
            return Ok(MergeEquivalence::Ancestor);
        }
        if self.same_tree(branch, trunk)? {
            return Ok(MergeEquivalence::SameTree);
        }
        let base = self.merge_base(trunk, branch)?;
        let Some(branch_patch) = self.net_diff_patch_id(&base, branch)? else {
            // An empty net diff means the branch carries no changes of its own,
            // so there is nothing for trunk to contain.
            return Ok(MergeEquivalence::NotFound);
        };
        if self.range_patch_ids(&base, trunk)?.contains(&branch_patch) {
            Ok(MergeEquivalence::NetDiff)
        } else {
            Ok(MergeEquivalence::NotFound)
        }
    }

    /// The `git patch-id --stable` id of the net diff `from..to`, or `None` when
    /// that diff is empty.
    fn net_diff_patch_id(&self, from: &str, to: &str) -> Result<Option<String>, GitError> {
        let range = format!("{from}..{to}");
        let mut args = vec!["diff"];
        args.extend_from_slice(PATCH_ID_DIFF_OPTS);
        args.push(range.as_str());
        let diff = self.run(&args)?;
        if diff.is_empty() {
            return Ok(None);
        }
        let out = self.run_with_stdin(&["patch-id", "--stable"], diff.as_bytes())?;
        Ok(out.split_whitespace().next().map(ToString::to_string))
    }

    /// The set of per-commit patch-ids for the commits in `from..to`.
    fn range_patch_ids(
        &self,
        from: &str,
        to: &str,
    ) -> Result<std::collections::HashSet<String>, GitError> {
        let range = format!("{from}..{to}");
        let mut args = vec!["log", "-p", "--no-merges"];
        args.extend_from_slice(PATCH_ID_DIFF_OPTS);
        args.push(range.as_str());
        let patches = self.run(&args)?;
        if patches.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let out = self.run_with_stdin(&["patch-id", "--stable"], patches.as_bytes())?;
        // `git patch-id --stable` emits one `<patch-id> <commit-id>` line per
        // commit in the stream; keep the patch-id.
        Ok(out
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(ToString::to_string)
            .collect())
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

    /// Switch to an existing `branch` (`git checkout`).
    pub fn checkout(&self, branch: &str) -> Result<(), GitError> {
        self.run(&["checkout", branch]).map(|_| ())
    }

    /// Create `branch` off the current HEAD and switch to it (`git checkout -b`).
    pub fn checkout_new_branch(&self, branch: &str) -> Result<(), GitError> {
        self.run(&["checkout", "-b", branch]).map(|_| ())
    }

    /// Create `branch` off `start` and switch to it
    /// (`git checkout -b <branch> <start>`).
    pub fn checkout_new_branch_at(&self, branch: &str, start: &str) -> Result<(), GitError> {
        self.run(&["checkout", "-b", branch, start]).map(|_| ())
    }

    /// Stage every change in the working tree, tracked and untracked,
    /// including deletions (`git add -A`).
    pub fn stage_all(&self) -> Result<(), GitError> {
        self.run(&["add", "-A"]).map(|_| ())
    }

    /// Stage the changes under `paths`, including deletions and untracked
    /// files (`git add -A -- <paths>`).
    pub fn stage_paths(&self, paths: &[String]) -> Result<(), GitError> {
        let mut args = vec!["add", "-A", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args).map(|_| ())
    }

    /// Unstage the changes under `paths`, leaving the working tree untouched
    /// (`git reset -q HEAD -- <paths>`).
    pub fn unstage_paths(&self, paths: &[String]) -> Result<(), GitError> {
        let mut args = vec!["reset", "-q", "HEAD", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args).map(|_| ())
    }

    /// Commit the staged changes with `message` (`git commit -m`).
    pub fn commit(&self, message: &str) -> Result<(), GitError> {
        self.run(&["commit", "-m", message]).map(|_| ())
    }

    /// Amend the current branch's tip, folding in staged changes. `message`
    /// replaces the subject; `None` keeps it (`git commit --amend --no-edit`).
    pub fn commit_amend(&self, message: Option<&str>) -> Result<(), GitError> {
        let mut args = vec!["commit", "--amend"];
        match message {
            Some(msg) => {
                args.push("-m");
                args.push(msg);
            }
            None => args.push("--no-edit"),
        }
        self.run(&args).map(|_| ())
    }

    /// Move `branch` to point at `target` (`git branch -f`). git refuses to move
    /// the currently checked-out branch this way, which is the intended guard.
    pub fn force_branch(&self, branch: &str, target: &str) -> Result<(), GitError> {
        self.run(&["branch", "-f", branch, target]).map(|_| ())
    }

    /// Rename a local branch (`git branch -m <old> <new>`). Works on the current
    /// branch (HEAD follows to `new`). Errors if `old` does not exist or `new`
    /// already does.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<(), GitError> {
        self.run(&["branch", "-m", old, new]).map(|_| ())
    }

    /// Whether the index has staged changes. `git diff --cached --quiet` exits 0
    /// when the index is clean and 1 when something is staged.
    pub fn has_staged_changes(&self) -> Result<bool, GitError> {
        let args = ["diff", "--cached", "--quiet"];
        let output = self
            .command(&args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        match output.status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(self.command_error(&args, &output)),
        }
    }

    /// Whether the working tree (tracked files, staged or unstaged) differs from
    /// `HEAD`. `git diff --quiet HEAD` exits 0 when the tree matches HEAD and 1
    /// when it is dirty. Untracked files do not count: a `reset --hard` keeps
    /// them, so a caller guarding a hard reset only cares about tracked changes.
    pub fn has_uncommitted_changes(&self) -> Result<bool, GitError> {
        let args = ["diff", "--quiet", "HEAD"];
        let output = self
            .command(&args)
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        match output.status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(self.command_error(&args, &output)),
        }
    }

    /// Reset the current branch, `HEAD`, and the working tree to `target`
    /// (`git reset --hard`). Discards tracked-file changes, so a caller that must
    /// not lose them checks [`has_uncommitted_changes`] first.
    ///
    /// [`has_uncommitted_changes`]: Git::has_uncommitted_changes
    pub fn reset_hard(&self, target: &str) -> Result<(), GitError> {
        self.run(&["reset", "--hard", target]).map(|_| ())
    }

    /// Move the current branch and `HEAD` to `target` while leaving the working
    /// tree untouched (`git reset --mixed`). The index is reset to `target`, so
    /// the difference between `target` and the on-disk files reads back as
    /// unstaged modifications. This is the absorb / pop mechanic: after the ref
    /// moves to a rewritten tip, a mixed reset leaves only the not-yet-absorbed
    /// edits as unstaged changes.
    pub fn reset_mixed(&self, target: &str) -> Result<(), GitError> {
        self.run(&["reset", "--mixed", target]).map(|_| ())
    }

    /// Push `refspec` to `remote`.
    pub fn push(&self, remote: &str, refspec: &str) -> Result<(), GitError> {
        self.run(&["push", remote, refspec]).map(|_| ())
    }

    /// Push `refspec` to `remote` with `--force-with-lease`. Refuses to clobber
    /// a ref whose remote tip moved out from under us, the safe-by-default
    /// force push for re-submitting a rebased branch.
    pub fn push_force_with_lease(&self, remote: &str, refspec: &str) -> Result<(), GitError> {
        self.run(&["push", "--force-with-lease", remote, refspec])
            .map(|_| ())
    }

    /// Fetch `refspec` from `remote`.
    pub fn fetch(&self, remote: &str, refspec: &str) -> Result<(), GitError> {
        self.run(&["fetch", remote, refspec]).map(|_| ())
    }

    pub fn git_dir(&self) -> Result<PathBuf, GitError> {
        let dir = self.run(&["rev-parse", "--git-dir"])?;
        Ok(self.dir.join(dir))
    }

    /// The branch the in-progress merge-style rebase is replaying (from
    /// `rebase-merge/head-name`), or `None` if there is none or it can't be read.
    pub fn rebase_head_branch(&self) -> Option<String> {
        let head_name = self.git_dir().ok()?.join("rebase-merge").join("head-name");
        let contents = std::fs::read_to_string(head_name).ok()?;
        let name = contents.trim();
        Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_string())
    }

    /// Every registered worktree of this repository, parsed from
    /// `git worktree list --porcelain`. Records are blank-line separated; a
    /// `worktree <path>` line opens each one and a `branch refs/heads/<name>`
    /// line names its checkout (absent for a detached or bare worktree).
    pub fn worktrees(&self) -> Result<Vec<Worktree>, GitError> {
        let out = self.run(&["worktree", "list", "--porcelain"])?;
        let mut worktrees = Vec::new();
        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                // A new record begins; flush the one we were building.
                if let Some(prev) = path.take() {
                    worktrees.push(Worktree { path: prev, branch: branch.take() });
                }
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            }
        }
        if let Some(prev) = path.take() {
            worktrees.push(Worktree { path: prev, branch });
        }
        Ok(worktrees)
    }

    /// The path of a *different* worktree that currently has `branch` checked
    /// out, or `None` when `branch` is free or is checked out only here.
    /// Rewriting a branch checked out elsewhere desyncs that worktree, so a
    /// caller uses this to refuse or skip such a branch. Paths are compared
    /// canonically because `git worktree list` reports resolved paths while
    /// `self.dir` may be a symlink (e.g. `/var` -> `/private/var` on macOS).
    pub fn branch_checked_out_elsewhere(&self, branch: &str) -> Result<Option<PathBuf>, GitError> {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let here = canonical(&self.dir);
        for wt in self.worktrees()? {
            if wt.branch.as_deref() == Some(branch) && canonical(&wt.path) != here {
                return Ok(Some(wt.path));
            }
        }
        Ok(None)
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

    /// Create a commit object for `tree` parented on `parent`, reusing the full
    /// author identity, author date, committer identity, and commit message of
    /// an existing commit `like`. This is the absorb / rewrite primitive: a
    /// commit's tree changes (the absorbed hunks land in it) but its identity
    /// and message must survive the rewrite, unlike [`commit_tree`] which stamps
    /// a fresh stacc identity and a one-line message.
    ///
    /// The author date is preserved verbatim; the committer date is left to
    /// git's default (now), matching how `git rebase` and `git commit --amend`
    /// behave when only the tree changes.
    ///
    /// [`commit_tree`]: Git::commit_tree
    pub fn commit_tree_like(
        &self,
        tree: &str,
        parent: Option<&str>,
        like: &str,
    ) -> Result<String, GitError> {
        // Pull the source commit's identity and full message in one read,
        // NUL-delimited so a message containing any other byte round-trips.
        let meta = self.run(&[
            "log",
            "-1",
            "--format=%an%x00%ae%x00%ad%x00%cn%x00%ce%x00%B",
            like,
        ])?;
        let mut parts = meta.splitn(6, '\0');
        let author_name = parts.next().unwrap_or_default().to_string();
        let author_email = parts.next().unwrap_or_default().to_string();
        let author_date = parts.next().unwrap_or_default().to_string();
        let committer_name = parts.next().unwrap_or_default().to_string();
        let committer_email = parts.next().unwrap_or_default().to_string();
        // `%B` is the raw body (subject + body); `run` trims the trailing
        // newline, which `commit-tree` re-adds, so the message is preserved.
        let message = parts.next().unwrap_or_default().to_string();

        let mut args = vec!["commit-tree", tree];
        if let Some(parent) = parent {
            args.push("-p");
            args.push(parent);
        }
        let mut child = self
            .command(&args)
            .env("GIT_AUTHOR_NAME", &author_name)
            .env("GIT_AUTHOR_EMAIL", &author_email)
            .env("GIT_AUTHOR_DATE", &author_date)
            .env("GIT_COMMITTER_NAME", &committer_name)
            .env("GIT_COMMITTER_EMAIL", &committer_email)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|source| GitError::Spawn { source })?;
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .expect("stdin is piped")
                .write_all(message.as_bytes())
                .map_err(|source| GitError::Spawn { source })?;
        }
        let output = child
            .wait_with_output()
            .map_err(|source| GitError::Spawn { source })?;
        if !output.status.success() {
            return Err(self.command_error(&args, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// The commits in `base..tip` (i.e. reachable from `tip` but not from
    /// `base`), **oldest-first** (`--reverse`). For a stacked branch with
    /// `base` its recorded base hash and `tip` its own tip, this is exactly the
    /// branch's own commits in apply order, the candidate set absorb may
    /// rewrite. An empty range (tip == base) yields an empty vector.
    pub fn rev_list(&self, base: &str, tip: &str) -> Result<Vec<String>, GitError> {
        let range = format!("{base}..{tip}");
        let out = self.run(&["rev-list", "--reverse", &range])?;
        Ok(out
            .lines()
            .filter(|l| !l.is_empty())
            .map(ToString::to_string)
            .collect())
    }

    /// Point `name` at `new`. When `old` is given, the move only succeeds if
    /// the ref currently equals it, a compare-and-swap.
    pub fn update_ref(&self, name: &str, new: &str, old: Option<&str>) -> Result<(), GitError> {
        let mut args = vec!["update-ref", name, new];
        if let Some(old) = old {
            args.push(old);
        }
        self.run(&args).map(|_| ())
    }

    /// Delete `name`. When `old` is given, the deletion only succeeds if the
    /// ref currently equals it, a compare-and-swap.
    pub fn delete_ref(&self, name: &str, old: Option<&str>) -> Result<(), GitError> {
        let mut args = vec!["update-ref", "-d", name];
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

    /// Whether `name` resolves to no git ref. A clean not-found returns `true`;
    /// a real git error returns `false` (the ref reads as present), so a
    /// transient failure never reports a live ref as missing.
    pub fn ref_missing(&self, name: &str) -> bool {
        matches!(self.ref_commit(name), Ok(None))
    }

    /// Whether `name` is usable as a branch name, per
    /// `git check-ref-format --branch`. The exit code is the answer (any
    /// rejection, including a name git parses as an option, reads as invalid);
    /// only a failure to spawn git is an error.
    pub fn valid_branch_name(&self, name: &str) -> Result<bool, GitError> {
        let output = self
            .command(&["check-ref-format", "--branch", name])
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        Ok(output.status.success())
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

    /// List the leaf blob paths under `<rev>:<path>`, recursively, so a nested
    /// entry comes back as its full path (e.g. `jillian/foo`), not just `jillian`.
    pub fn list_tree(&self, rev: &str, path: &str) -> Result<Vec<String>, GitError> {
        let spec = format!("{rev}:{path}");
        let output = self
            .command(&["ls-tree", "-r", "--name-only", spec.as_str()])
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        if !output.status.success() {
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(ToString::to_string)
            .collect())
    }

    /// The configured remote names (e.g. `["origin"]`).
    pub fn remotes(&self) -> Result<Vec<String>, GitError> {
        Ok(self
            .run(&["remote"])?
            .lines()
            .map(ToString::to_string)
            .collect())
    }

    /// The canonical URL configured for `remote`. Reads `remote.<name>.url`
    /// rather than `git remote get-url`, which expands `url.*.insteadOf`: stacc
    /// parses the owner/repo from this, so it must be the real GitHub URL, not a
    /// local rewrite some users configure for transport.
    pub fn remote_url(&self, remote: &str) -> Result<String, GitError> {
        self.run(&["config", &format!("remote.{remote}.url")])
    }

    /// The subject line of `rev`'s tip commit.
    pub fn commit_subject(&self, rev: &str) -> Result<String, GitError> {
        self.run(&["log", "-1", "--format=%s", rev])
    }

    /// Display metadata (short hash, subject, relative age) of `rev`'s tip.
    /// The three fields are NUL-delimited (`%x00`) so a subject containing any
    /// other byte round-trips intact; `LC_ALL=C` (set in `command`) keeps the
    /// relative age in stable English.
    pub fn commit_info(&self, rev: &str) -> Result<CommitInfo, GitError> {
        let out = self.run(&["log", "-1", "--format=%h%x00%s%x00%cr", rev])?;
        let mut parts = out.splitn(3, '\0');
        Ok(CommitInfo {
            sha: parts.next().unwrap_or_default().to_string(),
            subject: parts.next().unwrap_or_default().to_string(),
            age: parts.next().unwrap_or_default().to_string(),
        })
    }

    /// `(ahead, behind)` commit counts of `branch` relative to `base`: `ahead` is
    /// commits `branch` has that `base` lacks (zero means an empty stacked
    /// branch), `behind` is commits `base` has that `branch` lacks (nonzero means
    /// `branch` has drifted off `base` and needs a restack). One `rev-list`
    /// answers both, so `log` does not need a separate ancestry check.
    pub fn ahead_behind(&self, base: &str, branch: &str) -> Result<(usize, usize), GitError> {
        let range = format!("{base}...{branch}");
        let out = self.run(&["rev-list", "--left-right", "--count", &range])?;
        let mut counts = out.split_whitespace();
        let behind = counts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let ahead = counts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        Ok((ahead, behind))
    }

    /// Aggregate diff statistics from `from` to `to` (`git diff --numstat`):
    /// changed-file count plus total inserted and deleted lines. A binary
    /// change counts as a changed file with zero line counts.
    pub fn diffstat(&self, from: &str, to: &str) -> Result<DiffStat, GitError> {
        let out = self.run(&["diff", "--numstat", from, to])?;
        Ok(parse_numstat(&out))
    }

    /// The full diff text from `from` to `to` (`git diff`), uncolored. Empty
    /// when the two trees match.
    pub fn diff_text(&self, from: &str, to: &str) -> Result<String, GitError> {
        self.run(&["diff", "--no-color", from, to])
    }

    /// The per-commit patches of `base..tip`, oldest-first (the `git log -p`
    /// view of a branch's own commits), uncolored. Empty for an empty range.
    pub fn log_patch(&self, base: &str, tip: &str) -> Result<String, GitError> {
        let range = format!("{base}..{tip}");
        self.run(&["log", "-p", "--reverse", "--no-color", &range])
    }

    /// Git's own graph history for the given branch `tips`, excluding the
    /// trunk's history (`git log --graph --oneline --decorate <tips> --not
    /// <trunk>`). Backs `stacc log long`.
    ///
    /// With no `tips` (nothing tracked beyond the trunk), the exclusion has no
    /// positive ref, so git falls back to `HEAD` and `--not <trunk>` cancels it
    /// out to nothing on the trunk. We instead show the trunk's own history so
    /// the command is never silently empty.
    pub fn log_graph(&self, tips: &[&str], trunk: &str) -> Result<String, GitError> {
        let mut args = vec!["log", "--graph", "--oneline", "--decorate"];
        if tips.is_empty() {
            args.push(trunk);
        } else {
            args.extend(tips.iter().copied());
            args.push("--not");
            args.push(trunk);
        }
        self.run(&args)
    }

    /// The local branch names (`git branch`), used to surface branches stacc is
    /// not tracking under `log --show-untracked`.
    pub fn local_branches(&self) -> Result<Vec<String>, GitError> {
        Ok(self
            .run(&["branch", "--format=%(refname:short)"])?
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect())
    }

    /// The body (the message after the subject) of `rev`'s tip commit.
    pub fn commit_body(&self, rev: &str) -> Result<String, GitError> {
        self.run(&["log", "-1", "--format=%b", rev])
    }

    /// Paths with unresolved merge conflicts in the working tree.
    pub fn conflicted_files(&self) -> Result<Vec<String>, GitError> {
        Ok(self
            .run(&["diff", "--name-only", "--diff-filter=U"])?
            .lines()
            .map(ToString::to_string)
            .collect())
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

/// Hunk-level plumbing the surgery commands (`absorb`, `split --by-file`) build
/// on: split the staged diff into addressable hunks, blame a file's lines, and
/// enumerate or carve a commit's tree so a caller can rebuild it through the
/// existing [`write_tree`]/[`commit_tree`] primitives. This stays a thin typed
/// shell over `git`; the mapping logic (which commit a hunk belongs to) lives in
/// the absorb command, not here.
///
/// [`write_tree`]: Git::write_tree
/// [`commit_tree`]: Git::commit_tree
impl Git {
    /// Split the staged diff (`git diff --cached`) into addressable hunks.
    ///
    /// Uses `-U0` so each contiguous change is its own hunk (an absorb consumer
    /// blames each independently) and `-M` so a rename is detected and reported
    /// rather than appearing as a delete + add pair. Each hunk carries its file
    /// path, a [`HunkKind`] classification, the pre-image and post-image line
    /// ranges, and the raw body. A binary or pure-rename file with no `@@` body
    /// still yields one hunk (empty ranges and body) so the change is reported,
    /// never silently dropped. An empty staged diff yields an empty vector.
    pub fn diff_hunks(&self) -> Result<Vec<Hunk>, GitError> {
        // `--no-color` and `LC_ALL=C` (set in `command`) keep the output a
        // stable machine format; `-U0` separates adjacent changes into distinct
        // hunks; `-M` enables rename detection.
        let raw = self.run(&[
            "diff",
            "--cached",
            "--no-color",
            "-M",
            "-U0",
        ])?;
        Ok(parse_diff_hunks(&raw))
    }

    /// Blame `path` at `rev`, returning the commit SHA that last touched each
    /// line, `1`-indexed: element `i` is the SHA introducing line `i + 1`.
    ///
    /// This is the absorb mapper primitive: a caller maps a hunk's pre-image
    /// line range onto this vector to find the commit that introduced those
    /// lines (KTD-2). Uses `git blame --porcelain`, whose every line group opens
    /// with a `<40-hex sha> <orig> <final> <count>` header; the full 40-char SHA
    /// is returned (not abbreviated). An empty or missing file yields an empty
    /// vector.
    pub fn blame(&self, rev: &str, path: &str) -> Result<Vec<String>, GitError> {
        let out = self.run(&["blame", "--porcelain", rev, "--", path])?;
        Ok(parse_blame_porcelain(&out))
    }

    /// Enumerate `rev`'s tree as `(path, mode, sha)` leaf entries, recursively,
    /// via `git ls-tree -r -z`. NUL-separated so a path with spaces or other
    /// special bytes round-trips literally (no C-quoting). The companion of
    /// [`write_tree`]: a caller filters this set and rebuilds a subtree from the
    /// kept `(path, sha)` pairs. For modes other than `100644`, rebuild with
    /// [`mktree`] instead, which preserves each entry's mode.
    ///
    /// [`write_tree`]: Git::write_tree
    /// [`mktree`]: Git::mktree
    pub fn tree_entries(&self, rev: &str) -> Result<Vec<TreeEntry>, GitError> {
        let spec = format!("{rev}^{{tree}}");
        let output = self
            .command(&["ls-tree", "-r", "-z", spec.as_str()])
            .output()
            .map_err(|source| GitError::Spawn { source })?;
        if !output.status.success() {
            return Err(self.command_error(&["ls-tree", "-r", "-z", spec.as_str()], &output));
        }
        Ok(parse_ls_tree_z(&output.stdout))
    }

    /// Build a tree from full `(path, mode, sha)` entries and return its hash,
    /// preserving each entry's mode (unlike [`write_tree`], which assumes
    /// `100644`). Feeds `git mktree --missing` lines of the form
    /// `<mode> blob <sha>\t<path>` through a scratch index built with
    /// `read-tree`-free `update-index --cacheinfo`, so nested paths and
    /// executable/symlink modes both survive a carve-and-rebuild.
    ///
    /// [`write_tree`]: Git::write_tree
    pub fn mktree(&self, entries: &[TreeEntry]) -> Result<String, GitError> {
        let index = self
            .git_dir()?
            .join(format!("stacc-mktree-index-{}", std::process::id()));

        let build = || -> Result<String, GitError> {
            for entry in entries {
                let cacheinfo = format!("{},{},{}", entry.mode, entry.sha, entry.path);
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
}

/// Parse `git diff --cached -M -U0 --no-color` output into hunks. Each
/// `diff --git` block opens a new file whose [`HunkKind`] is read from the
/// header lines that follow (`Binary files`, `rename`, `new`/`deleted file
/// mode`); each `@@` line opens a hunk whose body is the following `+`/`-`/` `
/// lines. A binary or bodyless rename file still emits one hunk so the change is
/// reported.
fn parse_diff_hunks(raw: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    // Per-file state, reset at each `diff --git` line.
    let mut new_path: Option<String> = None;
    let mut old_path: Option<String> = None;
    let mut kind = HunkKind::Modified;
    let mut saw_hunk = false;
    // The hunk currently accumulating a body.
    let mut pending: Option<Hunk> = None;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // A new file block. Flush the previous file first.
            flush_diff_file(
                &mut hunks,
                &mut pending,
                saw_hunk,
                new_path.as_deref(),
                old_path.as_deref(),
                kind,
            );
            let (a, b) = split_diff_git_paths(rest);
            old_path = a;
            new_path = b;
            kind = HunkKind::Modified;
            saw_hunk = false;
        } else if line.starts_with("new file mode") {
            kind = HunkKind::Added;
            // An added file has no meaningful pre-image path.
            old_path = None;
        } else if line.starts_with("deleted file mode") {
            kind = HunkKind::Deleted;
        } else if line.starts_with("Binary files") {
            kind = HunkKind::Binary;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            kind = HunkKind::Renamed;
            old_path = Some(from.to_string());
        } else if let Some(to) = line.strip_prefix("rename to ") {
            if kind == HunkKind::Renamed {
                new_path = Some(to.to_string());
            }
        } else if line.starts_with("@@") {
            // Open a new hunk; flush the previous one.
            if let Some(h) = pending.take() {
                hunks.push(h);
            }
            saw_hunk = true;
            if let (Some(np), Some((old_range, new_range))) =
                (new_path.clone(), parse_hunk_header(line))
            {
                pending = Some(Hunk {
                    path: np,
                    old_path: old_path.clone(),
                    kind,
                    old_range,
                    new_range,
                    body: String::new(),
                });
            }
        } else if let Some(h) = pending.as_mut() {
            // A body line belongs to the open hunk. Context/added/removed lines
            // begin with ` `/`+`/`-`; the `\ No newline at end of file` marker is
            // kept verbatim too.
            if line.starts_with([' ', '+', '-', '\\']) {
                if !h.body.is_empty() {
                    h.body.push('\n');
                }
                h.body.push_str(line);
            }
        }
    }
    flush_diff_file(
        &mut hunks,
        &mut pending,
        saw_hunk,
        new_path.as_deref(),
        old_path.as_deref(),
        kind,
    );
    hunks
}

/// Close out the file [`parse_diff_hunks`] was parsing: flush any open hunk,
/// then if the file had no `@@` body at all (binary / pure rename) emit a
/// placeholder hunk so the change is still reported, never dropped.
fn flush_diff_file(
    hunks: &mut Vec<Hunk>,
    pending: &mut Option<Hunk>,
    saw_hunk: bool,
    new_path: Option<&str>,
    old_path: Option<&str>,
    kind: HunkKind,
) {
    if let Some(h) = pending.take() {
        hunks.push(h);
    }
    if !saw_hunk {
        if let Some(path) = new_path {
            hunks.push(Hunk {
                path: path.to_string(),
                old_path: old_path.map(ToString::to_string),
                kind,
                old_range: LineRange { start: 0, count: 0 },
                new_range: LineRange { start: 0, count: 0 },
                body: String::new(),
            });
        }
    }
}

/// Split a `diff --git a/<x> b/<y>` suffix into `(old, new)` paths. git prefixes
/// the two sides with `a/` and `b/`; this strips them. A path containing a space
/// is the ambiguous case git itself C-quotes (handled by the rename lines, which
/// are authoritative for renames), so this best-effort split serves the common
/// unquoted case and the rename lines override it when present.
fn split_diff_git_paths(rest: &str) -> (Option<String>, Option<String>) {
    // Find the " b/" separating the two sides. Use the last occurrence so a
    // path that itself contains " b/" does not split early.
    if let Some(idx) = rest.rfind(" b/") {
        let a = rest[..idx].strip_prefix("a/").unwrap_or(&rest[..idx]);
        let b = &rest[idx + 3..];
        (Some(a.to_string()), Some(b.to_string()))
    } else {
        (None, None)
    }
}

/// Parse an `@@ -<os>[,<oc>] +<ns>[,<nc>] @@` header into `(old_range,
/// new_range)`. A missing count defaults to `1` per the unified-diff format.
fn parse_hunk_header(line: &str) -> Option<(LineRange, LineRange)> {
    // Strip the leading `@@ ` and take up to the closing ` @@`.
    let inner = line.strip_prefix("@@ ")?;
    let end = inner.find(" @@")?;
    let spec = &inner[..end];
    let mut sides = spec.split(' ');
    let old = sides.next()?.strip_prefix('-')?;
    let new = sides.next()?.strip_prefix('+')?;
    Some((parse_range(old)?, parse_range(new)?))
}

/// Parse a `<start>[,<count>]` diff range. A bare `<start>` means `count == 1`.
fn parse_range(spec: &str) -> Option<LineRange> {
    let mut parts = spec.splitn(2, ',');
    let start = parts.next()?.parse().ok()?;
    let count = match parts.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    Some(LineRange { start, count })
}

/// Parse `git diff --numstat` output: one `<insertions>\t<deletions>\t<path>`
/// record per file. A binary change reports `-` for both counts; it still
/// counts as a changed file, with the unparseable counts read as zero.
fn parse_numstat(out: &str) -> DiffStat {
    let mut stat = DiffStat::default();
    for line in out.lines() {
        let mut fields = line.split('\t');
        let (Some(insertions), Some(deletions)) = (fields.next(), fields.next()) else {
            continue;
        };
        if fields.next().is_none() {
            continue; // no path field: not a numstat record
        }
        stat.files += 1;
        stat.insertions += insertions.parse::<usize>().unwrap_or(0);
        stat.deletions += deletions.parse::<usize>().unwrap_or(0);
    }
    stat
}

/// Parse `git blame --porcelain` output into a per-line SHA vector. Every line
/// group opens with a `<40-hex sha> <orig> <final> <count>` header; the
/// `<final>` field (1-indexed) is the position the SHA is recorded at. Metadata
/// lines and the tab-prefixed source line are skipped. The vector is dense over
/// `1..=N`; a gap (which `git blame` does not produce for a contiguous file)
/// would leave an empty slot, so we index by the reported final line.
fn parse_blame_porcelain(out: &str) -> Vec<String> {
    let mut by_line: std::collections::BTreeMap<u32, String> = std::collections::BTreeMap::new();
    for line in out.lines() {
        // A header line is `<sha> <orig> <final> <count>`: a 40-hex first token
        // followed by numeric fields. Metadata lines ("author ...") and the
        // source line (tab-prefixed) never match.
        let mut fields = line.split(' ');
        let Some(sha) = fields.next() else { continue };
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // Need orig, final[, count]; we read the final line number.
        let _orig = fields.next();
        let Some(final_line) = fields.next().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        by_line.insert(final_line, sha.to_string());
    }
    // Materialize dense 1..=max. A contiguous blame fills every slot; any gap
    // (not expected) is filled with an empty string rather than panicking.
    let max = by_line.keys().copied().max().unwrap_or(0);
    (1..=max)
        .map(|i| by_line.get(&i).cloned().unwrap_or_default())
        .collect()
}

/// Parse `git ls-tree -r -z` output (NUL-separated `<mode> <type> <sha>\t<path>`
/// records) into [`TreeEntry`] values, keeping only blob entries.
fn parse_ls_tree_z(bytes: &[u8]) -> Vec<TreeEntry> {
    let text = String::from_utf8_lossy(bytes);
    let mut entries = Vec::new();
    for record in text.split('\0') {
        if record.is_empty() {
            continue;
        }
        // `<mode> <type> <sha>\t<path>`
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let mut meta_fields = meta.split(' ');
        let Some(mode) = meta_fields.next() else { continue };
        let Some(kind) = meta_fields.next() else { continue };
        let Some(sha) = meta_fields.next() else { continue };
        // `-r` still surfaces submodule commits and (without `-t`) no subtrees,
        // but guard on the type so only blobs feed a tree rebuild.
        if kind != "blob" {
            continue;
        }
        entries.push(TreeEntry {
            path: path.to_string(),
            mode: mode.to_string(),
            sha: sha.to_string(),
        });
    }
    entries
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
    fn same_tree_detects_identical_content_across_different_commits() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "f.txt", "content\n", "feature add");
        run_git(path, &["checkout", "-q", "main"]);
        write_commit(path, "f.txt", "content\n", "main add (same content)");
        // Different commits, identical trees (the squash-merge shape).
        assert_ne!(repo.rev_parse("feature").unwrap(), repo.rev_parse("main").unwrap());
        assert!(repo.same_tree("feature", "main").unwrap(), "identical trees");
    }

    #[test]
    fn same_tree_is_false_for_different_content() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "f.txt", "feature\n", "feature add");
        run_git(path, &["checkout", "-q", "main"]);
        write_commit(path, "f.txt", "main\n", "main add");
        assert!(!repo.same_tree("feature", "main").unwrap(), "different trees");
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
    fn merge_equivalence_ancestor_for_fast_forward_merge() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "f.txt", "feature\n", "feature add");
        run_git(path, &["checkout", "-q", "main"]);
        run_git(path, &["merge", "-q", "--ff-only", "feature"]);
        // feature's commits are now reachable from main.
        assert_eq!(
            repo.merge_equivalence("main", "feature").unwrap(),
            MergeEquivalence::Ancestor
        );
    }

    #[test]
    fn merge_equivalence_same_tree_for_squash_without_advance() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "a.txt", "base\n", "base");
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "a.txt", "base\nfeat\n", "feature change");
        run_git(path, &["checkout", "-q", "main"]);
        // Squash the same content onto main; trunk has not advanced otherwise,
        // so the trees match even though the commits differ.
        write_commit(path, "a.txt", "base\nfeat\n", "squash feature");
        assert_eq!(
            repo.merge_equivalence("main", "feature").unwrap(),
            MergeEquivalence::SameTree
        );
    }

    #[test]
    fn merge_equivalence_netdiff_for_single_commit_squash_after_advance() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "a.txt", "base\n", "base");
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "a.txt", "base\nfeat\n", "feature change");
        run_git(path, &["checkout", "-q", "main"]);
        // Trunk advances on an unrelated file, then the feature lands squashed.
        write_commit(path, "b.txt", "other\n", "unrelated trunk advance");
        write_commit(path, "a.txt", "base\nfeat\n", "squash feature");
        // Not an ancestor, and trees differ (main has b.txt), but the squash
        // commit's patch matches the branch's net diff.
        assert_eq!(
            repo.merge_equivalence("main", "feature").unwrap(),
            MergeEquivalence::NetDiff
        );
    }

    #[test]
    fn merge_equivalence_netdiff_for_multi_commit_squash() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "a.txt", "l1\n", "base");
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "a.txt", "l1\nl2\n", "feature c1");
        write_commit(path, "a.txt", "l1\nl2\nl3\n", "feature c2");
        run_git(path, &["checkout", "-q", "main"]);
        write_commit(path, "b.txt", "other\n", "unrelated trunk advance");
        // The two feature commits land as one squashed commit; the branch's net
        // diff equals the squash's diff even though no single per-commit patch
        // would (the `git cherry` failure mode).
        write_commit(path, "a.txt", "l1\nl2\nl3\n", "squash feature");
        assert_eq!(
            repo.merge_equivalence("main", "feature").unwrap(),
            MergeEquivalence::NetDiff
        );
    }

    #[test]
    fn merge_equivalence_not_found_for_unmerged_branch() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "a.txt", "base\n", "base");
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "a.txt", "base\nfeat\n", "feature change");
        run_git(path, &["checkout", "-q", "main"]);
        write_commit(path, "b.txt", "other\n", "unrelated trunk advance");
        // Feature's change never lands on main.
        assert_eq!(
            repo.merge_equivalence("main", "feature").unwrap(),
            MergeEquivalence::NotFound
        );
    }

    #[test]
    fn merge_equivalence_not_found_when_landed_as_separate_commits() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "a.txt", "l1\n", "base");
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "a.txt", "l1\nl2\n", "feature c1");
        write_commit(path, "a.txt", "l1\nl2\nl3\n", "feature c2");
        run_git(path, &["checkout", "-q", "main"]);
        // Trunk advances on an unrelated file so the trees differ (otherwise
        // same_tree would correctly short-circuit), then the two changes land as
        // two separate commits, not one squash. No single trunk commit's patch
        // matches the branch's combined net diff, so the heuristic reads
        // not-found (the conceded false negative).
        write_commit(path, "b.txt", "other\n", "unrelated trunk advance");
        write_commit(path, "a.txt", "l1\nl2\n", "land c1");
        write_commit(path, "a.txt", "l1\nl2\nl3\n", "land c2");
        assert_eq!(
            repo.merge_equivalence("main", "feature").unwrap(),
            MergeEquivalence::NotFound
        );
    }

    #[test]
    fn merge_equivalence_netdiff_false_positive_after_revert() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "a.txt", "base\n", "base");
        run_git(path, &["checkout", "-q", "-b", "feature"]);
        write_commit(path, "a.txt", "base\nfeat\n", "feature change");
        run_git(path, &["checkout", "-q", "main"]);
        // Feature lands squashed, then is reverted: the net effect on trunk is
        // gone, but the original landing commit's patch is still in history, so
        // the heuristic reports NetDiff. This is the safe false positive the
        // propose-only contract exists for.
        write_commit(path, "a.txt", "base\nfeat\n", "squash feature");
        write_commit(path, "a.txt", "base\n", "revert feature");
        assert_eq!(
            repo.merge_equivalence("main", "feature").unwrap(),
            MergeEquivalence::NetDiff
        );
    }

    #[test]
    fn command_error_on_bad_rev() {
        let (_tmp, repo) = init_repo();
        let err = repo.rev_parse("no-such-ref").unwrap_err();
        assert!(matches!(err, GitError::Command { .. }));
    }

    #[test]
    fn checkout_new_branch_creates_and_switches() {
        let (_tmp, repo) = init_repo();
        repo.checkout_new_branch("feature").unwrap();
        assert_eq!(repo.current_branch().unwrap(), "feature");
    }

    #[test]
    fn checkout_switches_between_branches() {
        let (tmp, repo) = init_repo();
        run_git(tmp.path(), &["branch", "side"]);
        repo.checkout("side").unwrap();
        assert_eq!(repo.current_branch().unwrap(), "side");
        repo.checkout("main").unwrap();
        assert_eq!(repo.current_branch().unwrap(), "main");
    }

    #[test]
    fn commit_and_has_staged_changes_track_the_index() {
        let (tmp, repo) = init_repo();
        assert!(!repo.has_staged_changes().unwrap());
        std::fs::write(tmp.path().join("f.txt"), "hi\n").unwrap();
        run_git(tmp.path(), &["add", "f.txt"]);
        assert!(repo.has_staged_changes().unwrap());
        repo.commit("add f").unwrap();
        assert!(!repo.has_staged_changes().unwrap());
    }

    #[test]
    fn commit_amend_replaces_the_tip_without_adding_a_parent() {
        let (tmp, repo) = init_repo();
        write_commit(tmp.path(), "f.txt", "a\n", "original");
        let before = repo.rev_parse("HEAD").unwrap();
        let parent = repo.rev_parse("HEAD~1").unwrap();
        std::fs::write(tmp.path().join("f.txt"), "b\n").unwrap();
        run_git(tmp.path(), &["add", "f.txt"]);
        repo.commit_amend(Some("reworded")).unwrap();
        let after = repo.rev_parse("HEAD").unwrap();
        assert_ne!(before, after);
        // Amend, not append: the parent is unchanged.
        assert_eq!(repo.rev_parse("HEAD~1").unwrap(), parent);
    }

    #[test]
    fn force_branch_moves_a_ref() {
        let (tmp, repo) = init_repo();
        let first = repo.rev_parse("HEAD").unwrap();
        run_git(tmp.path(), &["branch", "side"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "second"]);
        let second = repo.rev_parse("HEAD").unwrap();
        assert_eq!(repo.rev_parse("side").unwrap(), first);
        repo.force_branch("side", &second).unwrap();
        assert_eq!(repo.rev_parse("side").unwrap(), second);
    }

    #[test]
    fn valid_branch_name_accepts_good_and_rejects_bad_names() {
        let (_tmp, repo) = init_repo();
        assert!(repo.valid_branch_name("feature/foo-1").unwrap());
        assert!(!repo.valid_branch_name("has space").unwrap());
        assert!(!repo.valid_branch_name("bad..name").unwrap());
        assert!(!repo.valid_branch_name("-leading-dash").unwrap());
        assert!(!repo.valid_branch_name("").unwrap());
    }

    #[test]
    fn update_ref_with_empty_old_asserts_creation() {
        // git's documented contract: an empty old value means the ref must NOT
        // exist, so `update_ref(..., Some(""))` is an atomic create. `split`
        // leans on this to create new branch refs without a clobber window.
        let (_tmp, repo) = init_repo();
        let head = repo.rev_parse("HEAD").unwrap();
        repo.update_ref("refs/heads/fresh", &head, Some("")).unwrap();
        assert_eq!(repo.rev_parse("fresh").unwrap(), head);
        // Creating over an existing ref fails instead of moving it.
        let err = repo.update_ref("refs/heads/fresh", &head, Some(""));
        assert!(err.is_err(), "create-assert must refuse an existing ref");
        assert_eq!(repo.rev_parse("fresh").unwrap(), head);
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
            err @ RebaseError::Git(_) => panic!("expected interrupt, got {err:?}"),
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
                ("branches/jillian/foo", branch_blob.as_str()),
            ])
            .unwrap();
        let commit = repo.commit_tree(&tree, None, "state").unwrap();
        repo.update_ref("refs/stacc/data", &commit, None).unwrap();

        assert_eq!(repo.ref_commit("refs/stacc/data").unwrap(), Some(commit));
        assert_eq!(
            repo.read_blob("refs/stacc/data", "repo").unwrap().as_deref(),
            Some("{\"trunk\":\"main\"}")
        );
        // Recursive listing: a slashed branch name comes back as a full path.
        assert_eq!(
            repo.list_tree("refs/stacc/data", "branches").unwrap(),
            vec!["feature".to_string(), "jillian/foo".to_string()]
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
    fn delete_ref_cas_rejects_stale_old_then_deletes() {
        let (tmp, repo) = init_repo();
        run_git(tmp.path(), &["branch", "victim"]);
        let tip = repo.rev_parse("victim").unwrap();

        // A real commit that is not the victim's tip; the all-zero id would not
        // do here, git reads it as "skip the old-value check".
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "second"]);
        let stale = repo.rev_parse("HEAD").unwrap();
        assert_ne!(stale, tip);
        assert!(repo.delete_ref("refs/heads/victim", Some(&stale)).is_err());
        assert_eq!(repo.ref_commit("refs/heads/victim").unwrap(), Some(tip.clone()));

        repo.delete_ref("refs/heads/victim", Some(tip.as_str())).unwrap();
        assert_eq!(repo.ref_commit("refs/heads/victim").unwrap(), None);
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
    fn remote_url_returns_the_canonical_url_not_the_insteadof_rewrite() {
        let (tmp, repo) = init_repo();
        let github = "https://github.com/TinyDogTech/stacc.git";
        run_git(tmp.path(), &["remote", "add", "origin", github]);
        // A url.<x>.insteadOf rewrite (some users configure these for transport)
        // changes what `git remote get-url` reports but not `remote.origin.url`.
        run_git(
            tmp.path(),
            &["config", "url./tmp/local-mirror.insteadOf", github],
        );
        // remote_url must return the real GitHub URL so owner/repo parsing works.
        assert_eq!(repo.remote_url("origin").unwrap(), github);
    }

    #[test]
    fn symbolic_ref_resolves_head_and_rejects_direct() {
        let (_tmp, repo) = init_repo();
        assert_eq!(repo.symbolic_ref("HEAD").unwrap().as_deref(), Some("main"));
        assert_eq!(repo.symbolic_ref("refs/heads/main").unwrap(), None);
    }

    #[test]
    fn commit_info_returns_sha_subject_and_age() {
        let (tmp, repo) = init_repo();
        write_commit(tmp.path(), "f.txt", "x\n", "feat: do the thing");
        let info = repo.commit_info("HEAD").unwrap();
        let full = repo.rev_parse("HEAD").unwrap();
        assert!(full.starts_with(&info.sha), "{} not a prefix of {full}", info.sha);
        assert!(!info.sha.is_empty() && info.sha.len() < 40, "abbreviated: {}", info.sha);
        assert_eq!(info.subject, "feat: do the thing");
        // Relative age is English (LC_ALL=C), e.g. "0 seconds ago"; assert the
        // shape, not an exact value.
        assert!(info.age.ends_with("ago"), "age was {:?}", info.age);
    }

    #[test]
    fn commit_info_subject_with_pipe_and_tab_round_trips() {
        let (tmp, repo) = init_repo();
        // A subject containing the characters a naive delimiter would split on.
        write_commit(tmp.path(), "f.txt", "x\n", "fix: a | b\tc");
        assert_eq!(repo.commit_info("HEAD").unwrap().subject, "fix: a | b\tc");
    }

    #[test]
    fn ahead_behind_counts_both_sides() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        run_git(path, &["checkout", "-q", "-b", "feature"]); // even with main
        assert_eq!(repo.ahead_behind("main", "feature").unwrap(), (0, 0));
        write_commit(path, "f.txt", "x\n", "own work"); // one commit ahead
        assert_eq!(repo.ahead_behind("main", "feature").unwrap(), (1, 0));
        // Advance main so feature is also one commit behind (drifted off base).
        run_git(path, &["checkout", "-q", "main"]);
        write_commit(path, "m.txt", "m\n", "main moves");
        assert_eq!(repo.ahead_behind("main", "feature").unwrap(), (1, 1));
    }

    #[test]
    fn local_branches_lists_all_branches() {
        let (tmp, repo) = init_repo();
        run_git(tmp.path(), &["branch", "side"]);
        let mut branches = repo.local_branches().unwrap();
        branches.sort();
        assert_eq!(branches, vec!["main".to_string(), "side".to_string()]);
    }

    /// `git worktree add <path> <branch>` checks out an existing branch in a new
    /// linked worktree. Returns the TempDir holding it (kept alive by the caller)
    /// and the worktree's path.
    fn add_linked_worktree(main: &std::path::Path, branch: &str) -> (TempDir, PathBuf) {
        let linked = TempDir::new().expect("temp dir");
        let wt = linked.path().join("wt");
        run_git(main, &["worktree", "add", wt.to_str().unwrap(), branch]);
        (linked, wt)
    }

    #[test]
    fn worktrees_lists_main_and_linked_branch() {
        let (tmp, repo) = init_repo();
        run_git(tmp.path(), &["branch", "feat"]); // create without checking out in main
        let (_linked, _wt) = add_linked_worktree(tmp.path(), "feat");

        let wts = repo.worktrees().unwrap();
        assert_eq!(wts.len(), 2, "main + one linked worktree: {wts:?}");
        let branches: std::collections::BTreeSet<Option<String>> =
            wts.iter().map(|w| w.branch.clone()).collect();
        assert!(branches.contains(&Some("main".to_string())));
        assert!(branches.contains(&Some("feat".to_string())));
    }

    #[test]
    fn branch_checked_out_elsewhere_finds_the_linked_worktree() {
        let (tmp, repo) = init_repo();
        run_git(tmp.path(), &["branch", "feat"]);
        let (_linked, wt) = add_linked_worktree(tmp.path(), "feat");

        // `feat` lives in the linked worktree, not the current one.
        let found = repo.branch_checked_out_elsewhere("feat").unwrap();
        assert_eq!(
            found.map(|p| std::fs::canonicalize(p).unwrap()),
            Some(std::fs::canonicalize(&wt).unwrap())
        );
        // The current worktree's own branch is not "elsewhere".
        assert_eq!(repo.branch_checked_out_elsewhere("main").unwrap(), None);
        // A branch nothing has checked out matches nothing.
        assert_eq!(repo.branch_checked_out_elsewhere("nope").unwrap(), None);
    }

    #[test]
    fn detached_worktree_has_no_branch() {
        let (tmp, repo) = init_repo();
        let linked = TempDir::new().unwrap();
        let wt = linked.path().join("wt");
        run_git(tmp.path(), &["worktree", "add", "--detach", wt.to_str().unwrap()]);

        let wts = repo.worktrees().unwrap();
        assert_eq!(wts.len(), 2);
        assert!(
            wts.iter().any(|w| w.branch.is_none()),
            "the detached worktree reports no branch: {wts:?}"
        );
        // A detached worktree never matches a branch lookup.
        assert_eq!(repo.branch_checked_out_elsewhere("main").unwrap(), None);
    }

    #[test]
    fn single_worktree_has_nothing_checked_out_elsewhere() {
        let (tmp, repo) = init_repo();
        run_git(tmp.path(), &["branch", "feat"]);
        assert_eq!(repo.worktrees().unwrap().len(), 1);
        assert_eq!(repo.branch_checked_out_elsewhere("main").unwrap(), None);
        assert_eq!(repo.branch_checked_out_elsewhere("feat").unwrap(), None);
    }

    // --- U1: hunk / blame / tree-carve plumbing ---

    /// Write `contents` to `file` and stage it, without committing.
    fn stage(dir: &std::path::Path, file: &str, contents: &str) {
        std::fs::write(dir.join(file), contents).expect("write file");
        run_git(dir, &["add", file]);
    }

    #[test]
    fn diff_hunks_empty_index_is_empty() {
        let (_tmp, repo) = init_repo();
        assert!(repo.diff_hunks().unwrap().is_empty());
    }

    #[test]
    fn diff_hunks_splits_a_multi_hunk_edit_with_pre_image_ranges() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "f.txt", "a\nb\nc\nd\n", "seed");
        // Edit line 2 and line 4. With -U0 these are two separate hunks.
        stage(path, "f.txt", "a\nB\nc\nD\n");

        let hunks = repo.diff_hunks().unwrap();
        assert_eq!(hunks.len(), 2, "two disjoint edits, two hunks: {hunks:?}");
        for h in &hunks {
            assert_eq!(h.kind, HunkKind::Modified);
            assert_eq!(h.path, "f.txt");
            assert_eq!(h.old_path.as_deref(), Some("f.txt"));
        }
        // Pre-image ranges are the edited lines: line 2 and line 4.
        assert_eq!(hunks[0].old_range, LineRange { start: 2, count: 1 });
        assert_eq!(hunks[0].new_range, LineRange { start: 2, count: 1 });
        assert_eq!(hunks[1].old_range, LineRange { start: 4, count: 1 });
        assert_eq!(hunks[1].new_range, LineRange { start: 4, count: 1 });
        // The body carries the removed and added lines verbatim.
        assert!(hunks[0].body.contains("-b") && hunks[0].body.contains("+B"));
        assert!(hunks[1].body.contains("-d") && hunks[1].body.contains("+D"));
    }

    #[test]
    fn diff_hunks_classifies_added_file_with_no_pre_image() {
        let (tmp, repo) = init_repo();
        stage(tmp.path(), "new.txt", "x\ny\n");
        let hunks = repo.diff_hunks().unwrap();
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        let h = &hunks[0];
        assert_eq!(h.kind, HunkKind::Added);
        assert_eq!(h.path, "new.txt");
        // No pre-image to blame: old side is an empty range anchored at 0.
        assert_eq!(h.old_range, LineRange { start: 0, count: 0 });
        assert_eq!(h.new_range, LineRange { start: 1, count: 2 });
    }

    #[test]
    fn diff_hunks_classifies_a_binary_change() {
        let (tmp, repo) = init_repo();
        // A NUL byte makes git treat the file as binary.
        std::fs::write(tmp.path().join("b.dat"), [0u8, 1, 2, 0, 3]).unwrap();
        run_git(tmp.path(), &["add", "b.dat"]);
        let hunks = repo.diff_hunks().unwrap();
        assert_eq!(hunks.len(), 1, "binary change is reported, not dropped: {hunks:?}");
        assert_eq!(hunks[0].kind, HunkKind::Binary);
        assert_eq!(hunks[0].path, "b.dat");
        // No line body for a binary file.
        assert!(hunks[0].body.is_empty());
        assert_eq!(hunks[0].old_range.count, 0);
    }

    #[test]
    fn diff_hunks_classifies_a_deletion() {
        let (tmp, repo) = init_repo();
        write_commit(tmp.path(), "gone.txt", "x\n", "add");
        run_git(tmp.path(), &["rm", "gone.txt"]);
        let hunks = repo.diff_hunks().unwrap();
        assert!(
            hunks.iter().any(|h| h.kind == HunkKind::Deleted && h.path == "gone.txt"),
            "deletion is classified: {hunks:?}"
        );
    }

    #[test]
    fn diff_hunks_detects_a_rename_with_the_old_path() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "orig.txt", "alpha\nbeta\ngamma\n", "add");
        // Rename with no content change so -M reports it as a pure rename.
        run_git(path, &["mv", "orig.txt", "renamed.txt"]);
        let hunks = repo.diff_hunks().unwrap();
        let rename = hunks
            .iter()
            .find(|h| h.kind == HunkKind::Renamed)
            .expect("a rename hunk");
        assert_eq!(rename.path, "renamed.txt");
        assert_eq!(rename.old_path.as_deref(), Some("orig.txt"));
    }

    #[test]
    fn blame_attributes_each_line_to_its_introducing_commit() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        // Commit one line, then add a second in a separate commit.
        write_commit(path, "f.txt", "first\n", "c1");
        let c1 = repo.rev_parse("HEAD").unwrap();
        std::fs::write(path.join("f.txt"), "first\nsecond\n").unwrap();
        run_git(path, &["add", "f.txt"]);
        run_git(path, &["commit", "-q", "-m", "c2"]);
        let c2 = repo.rev_parse("HEAD").unwrap();

        let blame = repo.blame("HEAD", "f.txt").unwrap();
        assert_eq!(blame.len(), 2, "one sha per line: {blame:?}");
        // Line 1 came from c1, line 2 from c2 (matches `git blame`).
        assert_eq!(blame[0], c1);
        assert_eq!(blame[1], c2);
    }

    #[test]
    fn tree_entries_enumerates_nested_blobs_with_mode_and_sha() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        std::fs::create_dir_all(path.join("src/inner")).unwrap();
        std::fs::write(path.join("top.txt"), "t\n").unwrap();
        std::fs::write(path.join("src/a.rs"), "a\n").unwrap();
        std::fs::write(path.join("src/inner/b.rs"), "b\n").unwrap();
        run_git(path, &["add", "."]);
        run_git(path, &["commit", "-q", "-m", "tree"]);

        let entries = repo.tree_entries("HEAD").unwrap();
        let paths: std::collections::BTreeSet<&str> =
            entries.iter().map(|e| e.path.as_str()).collect();
        // Recursive: nested paths come back as full paths.
        assert!(paths.contains("top.txt"));
        assert!(paths.contains("src/a.rs"));
        assert!(paths.contains("src/inner/b.rs"));
        // Every entry carries a real mode and a 40-hex blob sha.
        for e in &entries {
            assert_eq!(e.mode, "100644", "{e:?}");
            assert_eq!(e.sha.len(), 40, "{e:?}");
        }
    }

    #[test]
    fn tree_entries_then_carve_rebuilds_a_subset_via_mktree() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        std::fs::write(path.join("keep.txt"), "k\n").unwrap();
        std::fs::write(path.join("drop.txt"), "d\n").unwrap();
        std::fs::create_dir_all(path.join("dir")).unwrap();
        std::fs::write(path.join("dir/nested.txt"), "n\n").unwrap();
        run_git(path, &["add", "."]);
        run_git(path, &["commit", "-q", "-m", "tree"]);

        // Carve: keep everything except drop.txt, rebuild the tree, commit it.
        let kept: Vec<TreeEntry> = repo
            .tree_entries("HEAD")
            .unwrap()
            .into_iter()
            .filter(|e| e.path != "drop.txt")
            .collect();
        let tree = repo.mktree(&kept).unwrap();
        let commit = repo.commit_tree(&tree, None, "carved").unwrap();

        let carved_paths = repo.list_tree(&commit, "").unwrap();
        assert!(carved_paths.contains(&"keep.txt".to_string()));
        assert!(carved_paths.contains(&"dir/nested.txt".to_string()));
        assert!(
            !carved_paths.contains(&"drop.txt".to_string()),
            "carved tree excludes the dropped path: {carved_paths:?}"
        );
        // The kept blob content is preserved byte-for-byte.
        assert_eq!(
            repo.read_blob(&commit, "keep.txt").unwrap().as_deref(),
            Some("k\n")
        );
    }

    #[test]
    fn mktree_preserves_an_executable_mode() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        std::fs::write(path.join("run.sh"), "#!/bin/sh\n").unwrap();
        run_git(path, &["add", "run.sh"]);
        run_git(path, &["update-index", "--chmod=+x", "run.sh"]);
        run_git(path, &["commit", "-q", "-m", "exec"]);

        let entries = repo.tree_entries("HEAD").unwrap();
        let run = entries.iter().find(|e| e.path == "run.sh").unwrap();
        assert_eq!(run.mode, "100755", "executable mode read back");
        // Rebuild through mktree and confirm the mode survives the round-trip.
        let tree = repo.mktree(&entries).unwrap();
        let rebuilt = repo.tree_entries(&tree).unwrap();
        let run2 = rebuilt.iter().find(|e| e.path == "run.sh").unwrap();
        assert_eq!(run2.mode, "100755");
    }

    #[test]
    fn rev_list_returns_own_commits_oldest_first() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        let base = repo.rev_parse("HEAD").unwrap();
        write_commit(path, "a.txt", "a\n", "c1");
        let c1 = repo.rev_parse("HEAD").unwrap();
        write_commit(path, "b.txt", "b\n", "c2");
        let c2 = repo.rev_parse("HEAD").unwrap();

        // base..tip is the two own commits, oldest-first.
        assert_eq!(repo.rev_list(&base, &c2).unwrap(), vec![c1, c2]);
        // An empty range yields nothing.
        assert!(repo.rev_list(&base, &base).unwrap().is_empty());
    }

    // --- U8: diffstat / diff / patch plumbing ---

    #[test]
    fn diffstat_counts_files_insertions_and_deletions() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "a.txt", "one\ntwo\n", "seed");
        let base = repo.rev_parse("HEAD").unwrap();
        // a.txt: one line replaced by two (+2 -1); b.txt: one new line (+1).
        std::fs::write(path.join("a.txt"), "one\nthree\nfour\n").unwrap();
        std::fs::write(path.join("b.txt"), "new\n").unwrap();
        run_git(path, &["add", "."]);
        run_git(path, &["commit", "-q", "-m", "edit"]);

        let stat = repo.diffstat(&base, "HEAD").unwrap();
        assert_eq!(
            stat,
            DiffStat {
                files: 2,
                insertions: 3,
                deletions: 1
            }
        );
        // An empty range is all zeroes.
        assert_eq!(repo.diffstat("HEAD", "HEAD").unwrap(), DiffStat::default());
    }

    #[test]
    fn diffstat_counts_a_binary_file_without_line_counts() {
        let (tmp, repo) = init_repo();
        let base = repo.rev_parse("HEAD").unwrap();
        // A NUL byte makes git treat the file as binary (`-` numstat counts).
        std::fs::write(tmp.path().join("bin.dat"), [0u8, 159, 146, 150]).unwrap();
        run_git(tmp.path(), &["add", "bin.dat"]);
        run_git(tmp.path(), &["commit", "-q", "-m", "binary"]);

        let stat = repo.diffstat(&base, "HEAD").unwrap();
        assert_eq!(
            stat,
            DiffStat {
                files: 1,
                insertions: 0,
                deletions: 0
            }
        );
    }

    #[test]
    fn diff_text_and_log_patch_carry_the_change() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        write_commit(path, "a.txt", "one\n", "seed");
        let base = repo.rev_parse("HEAD").unwrap();
        write_commit(path, "a.txt", "one\ntwo\n", "feat: add two");

        let diff = repo.diff_text(&base, "HEAD").unwrap();
        assert!(diff.contains("+two"), "got: {diff}");
        // The patch view carries the commit message AND the diff body.
        let patch_text = repo.log_patch(&base, "HEAD").unwrap();
        assert!(patch_text.contains("feat: add two"), "got: {patch_text}");
        assert!(patch_text.contains("+two"), "got: {patch_text}");
        // Empty ranges are empty text, not errors.
        assert!(repo.diff_text("HEAD", "HEAD").unwrap().is_empty());
        assert!(repo.log_patch("HEAD", "HEAD").unwrap().is_empty());
    }

    #[test]
    fn commit_tree_like_preserves_author_and_message() {
        let (tmp, repo) = init_repo();
        let path = tmp.path();
        // Author a commit with a distinct identity and a multi-line message.
        std::fs::write(path.join("f.txt"), "v1\n").unwrap();
        run_git(path, &["add", "f.txt"]);
        std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["commit", "-q", "-m", "subject line\n\nbody line"])
            .env("GIT_AUTHOR_NAME", "Original Author")
            .env("GIT_AUTHOR_EMAIL", "orig@example.com")
            .status()
            .expect("commit");
        let orig = repo.rev_parse("HEAD").unwrap();
        let parent = repo.rev_parse("HEAD~1").unwrap();

        // Rewrite its tree (a different blob) but reuse its identity + message.
        let blob = repo.hash_object(b"v2\n").unwrap();
        let mut entries = repo.tree_entries(&orig).unwrap();
        for e in &mut entries {
            if e.path == "f.txt" {
                e.sha = blob.clone();
            }
        }
        let tree = repo.mktree(&entries).unwrap();
        let rewritten = repo
            .commit_tree_like(&tree, Some(&parent), &orig)
            .unwrap();

        // Same author and message, different tree content.
        let author = repo
            .run(&["log", "-1", "--format=%an <%ae>", &rewritten])
            .unwrap();
        assert_eq!(author, "Original Author <orig@example.com>");
        let subject = repo.commit_subject(&rewritten).unwrap();
        assert_eq!(subject, "subject line");
        assert_eq!(repo.commit_body(&rewritten).unwrap(), "body line");
        assert_eq!(
            repo.read_blob(&rewritten, "f.txt").unwrap().as_deref(),
            Some("v2\n")
        );
    }
}
