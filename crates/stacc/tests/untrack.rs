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
fn untrack_removes_the_current_branch_but_keeps_the_git_branch() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc(p, &["untrack"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    // No longer tracked in stacc state...
    let s = String::from_utf8_lossy(&stacc(p, &["status", "--format", "json"]).stdout).into_owned();
    assert!(s.contains(r#""tracked":false"#), "got: {s}");
    // ...but the git branch still exists (untrack never touches git).
    run_git(p, &["rev-parse", "--verify", "feature"]);
}

#[test]
fn untrack_reparents_children_onto_the_base() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    assert!(stacc(p, &["track"]).status.success()); // a on main
    run_git(p, &["checkout", "-q", "-b", "b"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success()); // b on a

    // Untrack the middle branch `a` (named, while sitting on `b`).
    let out = stacc(p, &["untrack", "a", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains(r#""status":"untracked""#), "got: {s}");
    assert!(s.contains(r#""branch":"a""#) && s.contains(r#""base":"main""#), "got: {s}");
    assert!(s.contains(r#""reparented":["b"]"#), "reparented child expected: {s}");

    // `b` is now stacked on `main`; `a` is gone from the stack.
    let j = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(j.contains(r#""name":"b""#) && j.contains(r#""base":"main""#), "got: {j}");
    assert!(!j.contains(r#""name":"a""#), "a should be gone: {j}");
}

#[test]
fn untrack_refuses_the_trunk() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());

    let out = stacc(p, &["untrack", "main", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("cannot untrack the trunk"), "got: {s}");
}

#[test]
fn untrack_rejects_an_untracked_branch() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());

    let out = stacc(p, &["untrack", "ghost", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("is not tracked"), "got: {s}");
}
