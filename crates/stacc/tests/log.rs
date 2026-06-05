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

#[test]
fn log_marks_current_branch_and_needs_restack() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());

    // On `a`, up to date: `a` is marked current and shows no restack marker.
    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    assert!(s.contains("* a"), "current branch not marked: {s}");
    assert!(!s.contains("needs restack"), "unexpected restack marker: {s}");

    // Advance main so `a` drifts off its base.
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "main moves"]);
    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    assert!(s.contains("* main"), "current trunk not marked: {s}");
    assert!(
        s.contains("o a") && s.contains("needs restack"),
        "expected restack marker on a: {s}"
    );
}

#[test]
fn log_short_emits_one_line_per_branch() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success());

    let s = String::from_utf8_lossy(&stacc(p, &["log", "--short"]).stdout).into_owned();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 2, "one line per branch expected: {s}"); // a, b (no trunk header)
    assert!(s.contains("o a") && s.contains("* b"), "got: {s}");
    assert!(!s.contains("main"), "trunk should not appear in --short: {s}");
    assert!(!s.contains("needs restack"), "clean stack should have no marker: {s}");
}

#[test]
fn log_renders_a_forked_stack() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    // main -> a and main -> b: two children of the trunk.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track"]).status.success());

    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    // Both children render at depth 1 (a two-space indent); b is current.
    assert!(s.contains("  o a"), "got: {s}");
    assert!(s.contains("  * b"), "got: {s}");
}

#[test]
fn log_json_is_not_changed_by_drift() {
    // R15: the JSON contract must never gain pretty-only fields like needs-restack.
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "main moves"]); // a drifts

    let s = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(s.contains(r#""name":"a""#), "got: {s}");
    assert!(!s.contains("restack"), "JSON leaked a pretty marker: {s}");
    assert!(!s.contains("needs"), "JSON leaked a pretty marker: {s}");
}
