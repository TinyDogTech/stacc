//! `stacc pop`: remove the current branch but keep its changes in the working
//! tree. The ordering test is load-bearing: children must be reparented and
//! restacked onto the base (dropping the popped branch's commits) BEFORE the
//! mixed reset, while those commits still exist, and the end state must be the
//! base branch with the popped diff as unstaged modifications.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const ORIGIN: &str = "https://github.com/stacc-sandbox/example.git";

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

fn stacc(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
}

fn write_commit(dir: &Path, file: &str, contents: &str, msg: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run_git(dir, &["add", file]);
    run_git(dir, &["commit", "-q", "-m", msg]);
}

fn rev(dir: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", r])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn current_branch(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

fn log_json(dir: &Path) -> String {
    String::from_utf8_lossy(&stacc(dir, &["log", "--format", "json"]).stdout).into_owned()
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

fn file_contents(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).expect("read file")
}

fn repo_init() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", ORIGIN]);
    assert!(stacc(p, &["init"]).status.success());
    tmp
}

fn track(p: &Path, base: &str) {
    assert!(
        stacc(p, &["track", "--base", base]).status.success(),
        "track on {base} failed"
    );
}

/// main <- a (adds a.txt) <- b (modifies a.txt) <- c (adds c.txt), all
/// tracked. Leaves the repo checked out on `b`, ready to pop.
fn pop_stack(p: &Path) {
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a-content\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "a.txt", "b-version\n", "b1");
    track(p, "a");
    run_git(p, &["checkout", "-q", "-b", "c"]);
    write_commit(p, "c.txt", "c-content\n", "c1");
    track(p, "b");
    run_git(p, &["checkout", "-q", "b"]);
}

#[test]
fn pop_lands_the_diff_unstaged_and_reparents_children() {
    let tmp = repo_init();
    let p = tmp.path();
    pop_stack(p);
    let a_tip = rev(p, "a");

    let out = stacc(p, &["pop", "--format", "json"]);
    assert!(
        out.status.success(),
        "pop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "pop");
    assert_eq!(v["branch"], "b");
    assert_eq!(v["onto"], "a");
    assert_eq!(v["reparented"], serde_json::json!(["c"]));
    let restacked = v["restacked"].as_array().expect("restacked array");
    assert!(restacked.iter().any(|x| x == "c"), "c restacked: {v}");

    // End state: on the base, whose tip never moved, with b's diff unstaged.
    assert_eq!(current_branch(p), "a");
    assert_eq!(rev(p, "a"), a_tip, "the base ref did not move");
    assert_eq!(file_contents(p, "a.txt"), "b-version\n", "b's change is on disk");
    assert!(
        !git_ok(p, &["diff", "--quiet", "HEAD"]),
        "the popped diff is in the working tree"
    );
    assert!(
        git_ok(p, &["diff", "--cached", "--quiet"]),
        "nothing is staged (a mixed, not soft, reset)"
    );

    // b is gone from git and from state.
    assert!(!branch_ref_exists(p, "b"), "b's ref was deleted");
    let log = log_json(p);
    assert!(!log.contains(r#""name":"b""#), "b dropped from state: {log}");

    // c was reparented onto a and restacked WITHOUT b's commits: its history
    // is exactly a + its own commit, and a.txt at c is a's version.
    assert!(log.contains(r#""base":"a""#), "c reparented onto a: {log}");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "c"]));
    let count = Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["rev-list", "--count", "a..c"])
        .output()
        .expect("spawn git");
    assert_eq!(
        String::from_utf8_lossy(&count.stdout).trim(),
        "1",
        "c carries only its own commit"
    );
    let a_txt_at_c = Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["show", "c:a.txt"])
        .output()
        .expect("spawn git");
    assert_eq!(
        String::from_utf8_lossy(&a_txt_at_c.stdout),
        "a-content\n",
        "b's commit was dropped from c's history"
    );
    assert!(git_ok(p, &["cat-file", "-e", "c:c.txt"]), "c kept its commit");
}

#[test]
fn pop_of_a_leaf_lands_the_diff_with_no_children() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a-content\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "a.txt", "b-version\n", "b1");
    track(p, "a");

    let out = stacc(p, &["pop", "--format", "json"]);
    assert!(
        out.status.success(),
        "pop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["reparented"], serde_json::json!([]), "no children: {v}");
    assert_eq!(v["restacked"], serde_json::json!([]), "nothing to restack: {v}");
    assert_eq!(current_branch(p), "a");
    assert!(!branch_ref_exists(p, "b"));
    assert_eq!(file_contents(p, "a.txt"), "b-version\n");
    assert!(!git_ok(p, &["diff", "--quiet", "HEAD"]), "diff is unstaged");
}

#[test]
fn pop_refuses_a_dirty_working_tree() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a-content\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "a.txt", "b-version\n", "b1");
    track(p, "a");
    let b_tip = rev(p, "b");

    // Dirty the tree: the popped diff would collide with this edit.
    std::fs::write(p.join("a.txt"), "uncommitted\n").expect("write");

    let out = stacc(p, &["pop", "--format", "json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["error"], "usage", "structured refusal: {v}");
    assert!(
        v["message"].as_str().expect("message").contains("uncommitted"),
        "names the dirty tree: {v}"
    );

    // Nothing mutated, the edit survives.
    assert_eq!(current_branch(p), "b");
    assert_eq!(rev(p, "b"), b_tip);
    assert!(log_json(p).contains(r#""name":"b""#), "b still tracked");
    assert_eq!(file_contents(p, "a.txt"), "uncommitted\n");
}

#[test]
fn pop_refuses_the_trunk() {
    let tmp = repo_init();
    let p = tmp.path();

    let out = stacc(p, &["pop", "--format", "json"]);
    assert!(!out.status.success());
    let v = json(&out);
    assert_eq!(v["error"], "usage");
    assert!(
        v["message"].as_str().expect("message").contains("trunk"),
        "names the trunk: {v}"
    );
}

#[test]
fn pop_refuses_an_untracked_branch() {
    let tmp = repo_init();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "loose"]);
    write_commit(p, "loose.txt", "x\n", "l1");

    let out = stacc(p, &["pop", "--format", "json"]);
    assert!(!out.status.success());
    let v = json(&out);
    assert_eq!(v["error"], "usage");
    assert!(
        v["message"].as_str().expect("message").contains("not tracked"),
        "names the untracked branch: {v}"
    );
    assert_eq!(current_branch(p), "loose");
}

#[test]
fn pop_refuses_when_a_child_is_in_another_worktree() {
    let tmp = repo_init();
    let p = tmp.path();
    pop_stack(p);
    let b_tip = rev(p, "b");
    let c_tip = rev(p, "c");

    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-c").to_str().unwrap(), "c"],
    );

    let out = stacc(p, &["pop", "--format", "json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(
        v["error"], "worktree_conflict",
        "refused with the worktree_conflict discriminator: {v}"
    );
    assert_eq!(v["branch"], "c", "names the borrowed child: {v}");

    // Nothing mutated: b is still tracked and checked out, the tree is clean.
    assert_eq!(current_branch(p), "b");
    assert_eq!(rev(p, "b"), b_tip);
    assert_eq!(rev(p, "c"), c_tip);
    assert!(log_json(p).contains(r#""name":"b""#), "b still tracked");
    assert!(git_ok(p, &["diff", "--quiet", "HEAD"]), "clean tree");
}
