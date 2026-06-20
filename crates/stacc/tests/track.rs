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
fn track_records_branch_with_trunk_base() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);

    let out = stacc(tmp.path(), &["track", "--json"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(r#""status":"tracked""#), "got: {stdout}");
    assert!(stdout.contains(r#""branch":"feature""#), "got: {stdout}");
    assert!(stdout.contains(r#""base":"main""#), "got: {stdout}");

    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature"])
        .output()
        .unwrap();
    let blob = String::from_utf8_lossy(&show.stdout);
    assert!(blob.contains(r#""name": "main""#), "blob: {blob}");
}

#[test]
fn track_requires_init() {
    let tmp = repo();
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);

    let out = stacc(tmp.path(), &["track", "--json"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("not initialized"), "got: {stdout}");
}

#[test]
fn track_rejects_trunk() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    let out = stacc(tmp.path(), &["track", "--json"]); // still on main
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("trunk"), "got: {stdout}");
}

#[test]
fn track_accepts_explicit_base() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "base-branch"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);

    let out = stacc(
        tmp.path(),
        &["track", "--base", "base-branch", "--json"],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(r#""base":"base-branch""#), "got: {stdout}");
}
