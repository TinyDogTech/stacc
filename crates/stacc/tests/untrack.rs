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
    let s = String::from_utf8_lossy(&stacc(p, &["status", "--json"]).stdout).into_owned();
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
    let out = stacc(p, &["untrack", "a", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains(r#""status":"untracked""#), "got: {s}");
    assert!(s.contains(r#""branch":"a""#) && s.contains(r#""base":"main""#), "got: {s}");
    assert!(s.contains(r#""reparented":["b"]"#), "reparented child expected: {s}");

    // `b` is now stacked on `main`; `a` is gone from the stack.
    let j = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(j.contains(r#""name":"b""#) && j.contains(r#""base":"main""#), "got: {j}");
    assert!(!j.contains(r#""name":"a""#), "a should be gone: {j}");
}

#[test]
fn untrack_reparents_all_children_and_leaves_grandchildren_attached() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    // main <- a <- { b <- d, c }
    run_git(p, &["checkout", "-q", "-b", "a"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "d"]);
    assert!(stacc(p, &["track", "--base", "b"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);
    run_git(p, &["checkout", "-q", "-b", "c"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success());

    let out = stacc(p, &["untrack", "a", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    // Both direct children are reparented (assert membership, not array order).
    assert!(s.contains(r#""reparented""#) && s.contains(r#""b""#) && s.contains(r#""c""#), "got: {s}");

    // b and c now sit on main; the grandchild d stays attached to b; a is gone.
    // (JSON object keys serialize alphabetically, so assert on each field, not
    // on adjacency.)
    let j = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(!j.contains(r#""name":"a""#), "a should be gone: {j}");
    assert!(!j.contains(r#""base":"a""#), "nothing should still base on a: {j}");
    assert!(j.contains(r#""base":"b""#), "grandchild d stays on b: {j}");
    assert!(
        j.contains(r#""name":"b""#) && j.contains(r#""name":"c""#) && j.contains(r#""name":"d""#),
        "b, c, d all present: {j}"
    );
}

#[test]
fn untrack_pretty_output_reports_reparenting_only_when_there_is_a_child() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success());

    // Untracking a parent reports the reparented child.
    let parent = String::from_utf8_lossy(&stacc(p, &["untrack", "a"]).stdout).into_owned();
    assert!(parent.contains("Untracked a"), "got: {parent}");
    assert!(parent.contains("reparented onto main: b"), "got: {parent}");

    // Untracking a leaf (b is now on main) reports no reparenting.
    let leaf = String::from_utf8_lossy(&stacc(p, &["untrack", "b"]).stdout).into_owned();
    assert!(leaf.contains("Untracked b"), "got: {leaf}");
    assert!(!leaf.contains("reparented onto"), "leaf has no children: {leaf}");
}

#[test]
fn untrack_requires_init() {
    let tmp = repo();
    let p = tmp.path();
    let out = stacc(p, &["untrack", "feature", "--json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("is not initialized"), "got: {s}");
}

#[test]
fn untrack_refuses_the_trunk() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());

    let out = stacc(p, &["untrack", "main", "--json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("cannot untrack the trunk"), "got: {s}");
}

#[test]
fn untrack_rejects_an_untracked_branch() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());

    let out = stacc(p, &["untrack", "ghost", "--json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("is not tracked"), "got: {s}");
}
