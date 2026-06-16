//! `stacc split`: divide the current branch by commit (re-pointed refs at the
//! EXISTING commit hashes, ids unchanged) or by file (a flattened, re-authored
//! chain). The validation tests are load-bearing: a bad spec must abort before
//! any ref is written, and an unpartitionable by-file spec must be a
//! structured error, never a silent drop.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git")
        .success()
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn stacc(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
}

/// Write `file` (creating parent directories) and commit it.
fn write_commit(dir: &Path, file: &str, contents: &str, msg: &str) {
    let path = dir.join(file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create dirs");
    }
    std::fs::write(path, contents).expect("write file");
    run_git(dir, &["add", file]);
    run_git(dir, &["commit", "-q", "-m", msg]);
}

fn rev(dir: &Path, r: &str) -> String {
    git_stdout(dir, &["rev-parse", r])
}

fn current_branch(dir: &Path) -> String {
    git_stdout(dir, &["symbolic-ref", "--short", "HEAD"])
}

fn branch_ref_exists(dir: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .output()
        .expect("spawn git")
        .status
        .success()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

fn log_json(dir: &Path) -> String {
    String::from_utf8_lossy(&stacc(dir, &["log", "--format", "json"]).stdout).into_owned()
}

fn rebase_in_progress(dir: &Path) -> bool {
    let git_dir = dir.join(".git");
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

fn repo_init() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", "https://example.com/r.git"]);
    assert!(stacc(p, &["init"]).status.success());
    tmp
}

/// Track the current branch on `base`.
fn track(p: &Path, base: &str) {
    assert!(
        stacc(p, &["track", "--base", base]).status.success(),
        "track on {base} failed"
    );
}

/// Branch `a` off main with three commits (f, g, h), tracked. Leaves the repo
/// on `a`. Returns the three commit shas, oldest first.
fn three_commit_branch(p: &Path) -> (String, String, String) {
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "f\n", "c1: add f");
    write_commit(p, "g.txt", "g\n", "c2: add g");
    write_commit(p, "h.txt", "h\n", "c3: add h");
    track(p, "main");
    (rev(p, "a~2"), rev(p, "a~1"), rev(p, "a"))
}

#[test]
fn split_by_commit_creates_branches_at_the_existing_shas() {
    let tmp = repo_init();
    let p = tmp.path();
    let (c1, c2, c3) = three_commit_branch(p);

    let out = stacc(p, &["split", "n1", "n2", "--format", "json"]);
    assert!(
        out.status.success(),
        "split failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "split");
    assert_eq!(v["mode"], "commit");
    assert_eq!(v["branch"], "a");
    let created = v["created"].as_array().expect("created array");
    assert_eq!(created.len(), 2, "two new branches: {v}");
    assert_eq!(created[0]["name"], "n1");
    assert_eq!(created[0]["sha"].as_str(), Some(c1.as_str()));
    assert_eq!(created[1]["name"], "n2");
    assert_eq!(created[1]["sha"].as_str(), Some(c2.as_str()));

    // The refs point at the EXISTING commits: no cherry-pick, ids unchanged.
    assert_eq!(rev(p, "n1"), c1, "n1 at the oldest commit, id unchanged");
    assert_eq!(rev(p, "n2"), c2, "n2 at the middle commit, id unchanged");
    assert_eq!(rev(p, "a"), c3, "a keeps the tip, untouched");

    // The state chain: n1 on main, n2 on n1, a on n2; the log shows it.
    let log = log_json(p);
    assert!(log.contains(r#""name":"n1""#), "n1 tracked: {log}");
    assert!(log.contains(r#""name":"n2""#), "n2 tracked: {log}");
    assert!(log.contains(r#""base":"n1""#), "n2 chained on n1: {log}");
    assert!(log.contains(r#""base":"n2""#), "a chained on n2: {log}");

    // Clean finish: still on `a`, clean tree, no rebase.
    assert_eq!(current_branch(p), "a");
    assert!(!rebase_in_progress(p));
    assert!(git_ok(p, &["diff", "--quiet", "HEAD"]), "clean working tree");
}

#[test]
fn split_by_commit_wrong_name_count_is_a_usage_error() {
    let tmp = repo_init();
    let p = tmp.path();
    let (_, _, c3) = three_commit_branch(p);

    let out = stacc(p, &["split", "n1", "--format", "json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage", "structured refusal: {v}");
    let msg = v["message"].as_str().expect("message");
    assert!(msg.contains("exactly 2"), "names the required count: {msg}");

    // Zero refs created, nothing tracked, the branch untouched.
    assert!(!branch_ref_exists(p, "n1"));
    assert!(!log_json(p).contains(r#""name":"n1""#));
    assert_eq!(rev(p, "a"), c3);

    // No names at all (no picker off a TTY): also a structured error.
    let out = stacc(p, &["split", "--format", "json"]);
    assert!(!out.status.success(), "bare split must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    assert!(
        v["message"].as_str().expect("message").contains("2 branch name"),
        "names the required count: {v}"
    );
}

#[test]
fn split_by_commit_duplicate_or_existing_name_creates_zero_refs() {
    let tmp = repo_init();
    let p = tmp.path();
    let (_, _, c3) = three_commit_branch(p);

    // Duplicate names in the spec.
    let out = stacc(p, &["split", "n1", "n1", "--format", "json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    assert!(
        v["message"].as_str().expect("message").contains("duplicate"),
        "names the duplicate: {v}"
    );
    assert!(!branch_ref_exists(p, "n1"), "zero refs created");

    // A name colliding with an existing local branch (validated before ANY ref
    // is written, so n2 is not created either).
    run_git(p, &["branch", "exists"]);
    let exists_at = rev(p, "exists");
    let out = stacc(p, &["split", "exists", "n2", "--format", "json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    assert!(
        v["message"].as_str().expect("message").contains("already exists"),
        "names the collision: {v}"
    );
    assert!(!branch_ref_exists(p, "n2"), "zero refs created");
    assert_eq!(rev(p, "exists"), exists_at, "the existing branch never moved");
    assert_eq!(rev(p, "a"), c3, "the split branch never moved");
    let log = log_json(p);
    assert!(!log.contains(r#""name":"n1""#) && !log.contains(r#""name":"n2""#));
}

#[test]
fn split_by_commit_rejects_mixing_names_with_by_file() {
    let tmp = repo_init();
    let p = tmp.path();
    three_commit_branch(p);

    let out = stacc(p, &["split", "n1", "--by-file", "src=code", "--format", "json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    assert!(
        v["message"].as_str().expect("message").contains("not both"),
        "names the exclusivity: {v}"
    );
    assert!(!branch_ref_exists(p, "n1") && !branch_ref_exists(p, "code"));
}

#[test]
fn split_by_commit_single_commit_branch_is_a_noop() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "f\n", "only commit");
    track(p, "main");
    let tip = rev(p, "a");

    let out = stacc(p, &["split", "--format", "json"]);
    assert!(
        out.status.success(),
        "nothing-to-split is a no-op, not an error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "split");
    assert_eq!(v["mode"], "commit");
    assert_eq!(v["created"], serde_json::json!([]), "nothing created: {v}");
    assert_eq!(rev(p, "a"), tip, "the branch never moved");
}

/// Branch `a` off main: commit 1 adds `src/a.rs` AND `docs/b.md` (straddling
/// both groups, the flatten case), commit 2 edits `src/a.rs`. A child `c` is
/// stacked on `a`. Leaves the repo on `a`.
fn by_file_stack(p: &Path) {
    run_git(p, &["checkout", "-q", "-b", "a"]);
    let path = p.join("src");
    std::fs::create_dir_all(&path).expect("mkdir");
    std::fs::create_dir_all(p.join("docs")).expect("mkdir");
    std::fs::write(p.join("src/a.rs"), "v1\n").expect("write");
    std::fs::write(p.join("docs/b.md"), "docs\n").expect("write");
    run_git(p, &["add", "src/a.rs", "docs/b.md"]);
    run_git(p, &["commit", "-q", "-m", "straddles both groups"]);
    write_commit(p, "src/a.rs", "v2\n", "edits code only");
    track(p, "main");

    run_git(p, &["checkout", "-q", "-b", "c"]);
    write_commit(p, "c.txt", "c\n", "child work");
    track(p, "a");
    run_git(p, &["checkout", "-q", "a"]);
}

#[test]
fn split_by_file_partitions_changes_into_stacked_branches() {
    let tmp = repo_init();
    let p = tmp.path();
    by_file_stack(p);
    let a_before = rev(p, "a");
    let a_tree_before = rev(p, "a^{tree}");

    let out = stacc(
        p,
        &["split", "--by-file", "src=code", "--by-file", "docs=docs-branch", "--format", "json"],
    );
    assert!(
        out.status.success(),
        "split failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "split");
    assert_eq!(v["mode"], "file");
    assert_eq!(v["branch"], "a");
    let created = v["created"].as_array().expect("created array");
    assert_eq!(created.len(), 2, "two new branches: {v}");
    assert_eq!(created[0]["name"], "code");
    assert_eq!(created[0]["paths"], serde_json::json!(["src/a.rs"]));
    assert_eq!(created[1]["name"], "docs-branch");
    assert_eq!(created[1]["paths"], serde_json::json!(["docs/b.md"]));
    let restacked = v["restacked"].as_array().expect("restacked array");
    assert!(restacked.iter().any(|x| x == "c"), "the child restacked: {v}");

    // Each new branch carries only its group's changes vs its predecessor,
    // even though one original commit straddled both groups (flattened).
    assert_eq!(
        git_stdout(p, &["diff", "--name-only", "main", "code"]),
        "src/a.rs"
    );
    assert_eq!(
        git_stdout(p, &["diff", "--name-only", "code", "docs-branch"]),
        "docs/b.md"
    );
    assert_eq!(
        git_stdout(p, &["show", "code:src/a.rs"]),
        "v2",
        "code carries the FINAL content of its group"
    );
    assert!(
        !git_ok(p, &["cat-file", "-e", "code:docs/b.md"]),
        "docs changes are not on the code branch"
    );

    // The original branch: same content (tree unchanged), new commit ids,
    // sitting at the final commit of the chain.
    let a_after = rev(p, "a");
    assert_ne!(a_after, a_before, "a was re-authored");
    assert_eq!(rev(p, "a^{tree}"), a_tree_before, "a's content is preserved");
    assert_eq!(a_after, rev(p, "docs-branch"), "a sits at the chain's final commit");

    // The child restacked onto the moved tip; the state chain is
    // main <- code <- docs-branch <- a <- c.
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "c"]));
    let log = log_json(p);
    assert!(log.contains(r#""name":"code""#), "code tracked: {log}");
    assert!(log.contains(r#""base":"code""#), "docs-branch chained on code: {log}");
    assert!(log.contains(r#""base":"docs-branch""#), "a chained on docs-branch: {log}");
    assert!(log.contains(r#""base":"a""#), "c still chained on a: {log}");

    // Clean finish: back on `a`, clean tree, no rebase left behind.
    assert_eq!(current_branch(p), "a");
    assert!(!rebase_in_progress(p));
    assert!(git_ok(p, &["diff", "--quiet", "HEAD"]), "clean working tree");
}

#[test]
fn split_by_file_orphan_path_is_a_structured_error() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "src/a.rs", "code\n", "code");
    write_commit(p, "other.txt", "stray\n", "stray file");
    track(p, "main");
    let a_before = rev(p, "a");

    let out = stacc(p, &["split", "--by-file", "src=code", "--format", "json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage", "structured refusal: {v}");
    let msg = v["message"].as_str().expect("message");
    assert!(msg.contains("other.txt"), "lists the orphan path: {msg}");

    // Nothing mutated: no new ref, the branch and state untouched.
    assert!(!branch_ref_exists(p, "code"));
    assert_eq!(rev(p, "a"), a_before);
    assert!(!log_json(p).contains(r#""name":"code""#));
    assert!(!rebase_in_progress(p));
}

#[test]
fn split_by_file_refuses_a_dirty_working_tree() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "src/a.rs", "code\n", "code");
    track(p, "main");
    let a_before = rev(p, "a");

    // Dirty a TRACKED file (untracked files do not count).
    std::fs::write(p.join("src/a.rs"), "edited, uncommitted\n").expect("write");

    let out = stacc(p, &["split", "--by-file", "src=code", "--format", "json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage", "structured refusal: {v}");
    assert!(
        v["message"].as_str().expect("message").contains("uncommitted"),
        "names the dirty tree: {v}"
    );
    assert!(!branch_ref_exists(p, "code"));
    assert_eq!(rev(p, "a"), a_before);
    assert_eq!(
        std::fs::read_to_string(p.join("src/a.rs")).unwrap(),
        "edited, uncommitted\n",
        "the user's edits survive"
    );
}

#[test]
fn split_by_file_refuses_when_a_child_is_in_another_worktree() {
    let tmp = repo_init();
    let p = tmp.path();
    by_file_stack(p);
    let a_before = rev(p, "a");
    let c_before = rev(p, "c");

    // The child `c` checked out elsewhere: its ref would rewrite, so refuse.
    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-c").to_str().unwrap(), "c"],
    );

    let out = stacc(
        p,
        &["split", "--by-file", "src=code", "--by-file", "docs=docs-branch", "--format", "json"],
    );
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(
        v["type"], "worktree_conflict",
        "refused with the worktree_conflict discriminator: {v}"
    );
    assert_eq!(v["branch"], "c", "names the borrowed branch: {v}");

    // Nothing mutated.
    assert!(!branch_ref_exists(p, "code") && !branch_ref_exists(p, "docs-branch"));
    assert_eq!(rev(p, "a"), a_before);
    assert_eq!(rev(p, "c"), c_before);
    assert!(!rebase_in_progress(p));
}
