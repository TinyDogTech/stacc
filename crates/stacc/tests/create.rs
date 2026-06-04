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

fn git_out(dir: &Path, args: &[&str]) -> String {
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

fn current_branch(dir: &Path) -> String {
    git_out(dir, &["symbolic-ref", "--short", "HEAD"])
}

/// A git repo with `main` + an initial commit and an `origin` remote, with stacc
/// initialized.
fn init_repo() -> TempDir {
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

#[test]
fn create_with_staged_changes_commits_and_tracks() {
    let tmp = init_repo();
    let p = tmp.path();
    std::fs::write(p.join("f.txt"), "hi\n").expect("write");
    run_git(p, &["add", "f.txt"]);

    let out = stacc(p, &["create", "feat-x", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""branch":"feat-x""#), "got: {s}");
    assert!(s.contains(r#""base":"main""#), "got: {s}");
    assert!(s.contains(r#""committed":true"#), "got: {s}");

    // Switched to the new branch, the staged file is committed, index is clean,
    // and the default commit message is the branch name.
    assert_eq!(current_branch(p), "feat-x");
    assert!(git_ok(p, &["cat-file", "-e", "HEAD:f.txt"]));
    assert!(git_ok(p, &["diff", "--cached", "--quiet"]));
    assert_eq!(git_out(p, &["log", "-1", "--format=%s"]), "feat-x");

    // Tracked with base main.
    let log = stacc(p, &["log", "--format", "json"]);
    let ls = String::from_utf8_lossy(&log.stdout);
    assert!(ls.contains(r#""name":"feat-x""#), "got: {ls}");
    assert!(ls.contains(r#""base":"main""#), "got: {ls}");
}

#[test]
fn create_with_nothing_staged_makes_an_empty_tracked_branch() {
    let tmp = init_repo();
    let p = tmp.path();

    let out = stacc(p, &["create", "feat-y", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""committed":false"#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(current_branch(p), "feat-y");
    // No new commit: feat-y is exactly main's tip.
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "feat-y", "main"]));
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "main", "feat-y"]));
    let log = stacc(p, &["log", "--format", "json"]);
    assert!(String::from_utf8_lossy(&log.stdout).contains(r#""name":"feat-y""#));
}

#[test]
fn create_uninitialized_errors() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);

    let out = stacc(p, &["create", "feat-z", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not initialized"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn create_stacks_on_the_current_branch() {
    let tmp = init_repo();
    let p = tmp.path();
    // First branch off main.
    assert!(stacc(p, &["create", "a"]).status.success());
    assert_eq!(current_branch(p), "a");
    // Second branch off a, with staged work and a custom message.
    std::fs::write(p.join("a.txt"), "a\n").expect("write");
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["create", "b", "-m", "b work"]).status.success());
    assert_eq!(current_branch(p), "b");
    assert_eq!(git_out(p, &["log", "-1", "--format=%s"]), "b work");

    let log = stacc(p, &["log", "--format", "json"]);
    let s = String::from_utf8_lossy(&log.stdout);
    assert!(s.contains(r#""name":"b""#), "got: {s}");
    assert!(s.contains(r#""base":"a""#), "got: {s}");
}

#[test]
fn create_from_detached_head_errors() {
    let tmp = init_repo();
    let p = tmp.path();
    let head = git_out(p, &["rev-parse", "HEAD"]);
    run_git(p, &["checkout", "-q", &head]); // detach HEAD

    let out = stacc(p, &["create", "feat-d", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("detached HEAD"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
