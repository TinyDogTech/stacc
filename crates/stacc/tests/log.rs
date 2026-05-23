use std::process::{Command, Output};

use tempfile::TempDir;

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn stacc(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
}

fn repo() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://example.com/r.git"],
    );
    tmp
}

#[test]
fn log_renders_nested_stack_json() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-1"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-2"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature-1"])
        .status
        .success());

    let out = stacc(tmp.path(), &["log", "--format", "json"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""trunk":"main""#), "got: {s}");
    assert!(s.contains(r#""name":"feature-1""#), "got: {s}");
    assert!(s.contains(r#""name":"feature-2""#), "got: {s}");
    // feature-2 is nested under feature-1
    assert!(s.contains(r#""base":"feature-1""#), "got: {s}");
}

#[test]
fn log_pretty_lists_branches() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let out = stacc(tmp.path(), &["log"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("main"), "got: {s}");
    assert!(s.contains("feature"), "got: {s}");
}

#[test]
fn log_requires_init() {
    let tmp = repo();
    let out = stacc(tmp.path(), &["log", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not initialized"), "got: {s}");
}
