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

fn init_repo(with_origin: bool) -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    if with_origin {
        run_git(
            tmp.path(),
            &["remote", "add", "origin", "https://example.com/r.git"],
        );
    }
    tmp
}

#[test]
fn init_then_idempotent() {
    let tmp = init_repo(true);

    let out = stacc(tmp.path(), &["init", "--json"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(r#""status":"initialized""#), "got: {stdout}");
    assert!(stdout.contains(r#""trunk":"main""#), "got: {stdout}");
    assert!(stdout.contains(r#""remote":"origin""#), "got: {stdout}");

    let out2 = stacc(tmp.path(), &["init", "--json"]);
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains(r#""status":"already_initialized""#),
        "got: {stdout2}"
    );
}

#[test]
fn init_errors_without_remote() {
    let tmp = init_repo(false);

    let out = stacc(tmp.path(), &["init", "--json"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("could not determine remote"), "got: {stdout}");
}

#[test]
fn init_respects_flag_overrides() {
    let tmp = init_repo(true);

    let out = stacc(
        tmp.path(),
        &[
            "init", "--json", "--trunk", "trunk-x", "--remote", "upstream",
        ],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(r#""trunk":"trunk-x""#), "got: {stdout}");
    assert!(stdout.contains(r#""remote":"upstream""#), "got: {stdout}");
}
