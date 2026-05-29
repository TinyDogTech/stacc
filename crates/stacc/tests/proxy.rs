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
    tmp
}

#[test]
fn proxies_unknown_command_to_git() {
    let tmp = repo();
    // `rev-parse` isn't a stacc command -> proxied to git.
    let out = stacc(tmp.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "main");
}

#[test]
fn proxies_git_exit_code() {
    let tmp = repo();
    let out = stacc(tmp.path(), &["rev-parse", "--verify", "no-such-ref"]);
    assert!(!out.status.success());
}

#[test]
fn respects_git_aliases() {
    let tmp = repo();
    run_git(
        tmp.path(),
        &["config", "alias.cur", "rev-parse --abbrev-ref HEAD"],
    );
    // `cur` is a git alias; the proxy lets git expand it.
    let out = stacc(tmp.path(), &["cur"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "main");
}

#[test]
fn known_command_is_not_proxied() {
    let tmp = repo();
    // `log` is a stacc command: without init it errors with stacc's message,
    // proving it isn't passed through to `git log` (which would succeed).
    let out = stacc(tmp.path(), &["log", "--format", "json"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("not initialized"));
}

#[test]
fn st_alias_runs() {
    let out = Command::new(env!("CARGO_BIN_EXE_st"))
        .arg("--version")
        .output()
        .expect("spawn st");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("stacc"));
}
