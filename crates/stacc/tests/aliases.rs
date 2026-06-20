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
    // HOME is overridden so the user-global config file can't leak in.
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .env("HOME", dir)
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

fn write_stacc_toml(dir: &std::path::Path, contents: &str) {
    std::fs::write(dir.join(".stacc.toml"), contents).expect("write .stacc.toml");
}

#[test]
fn alias_expands_to_a_stacc_command() {
    let tmp = repo();
    write_stacc_toml(tmp.path(), "[aliases]\nstatlog = \"log\"\n");
    // log requires init; if the alias expanded, we should hit stacc's "not
    // initialized" error (proving the rewrite ran AND `log` dispatched).
    let out = stacc(tmp.path(), &["statlog", "--json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not initialized"), "got: {s}");
}

#[test]
fn shipped_default_alias_expands() {
    let tmp = repo(); // no .stacc.toml: only the built-in defaults are seeded
    // `l` -> log; on an uninitialized repo log errors "not initialized",
    // proving the shipped alias was seeded and dispatched.
    let out = stacc(tmp.path(), &["l", "--json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not initialized"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn repo_alias_overrides_a_shipped_default() {
    let tmp = repo();
    // Remap `co` (shipped: checkout) to log via repo config.
    write_stacc_toml(tmp.path(), "[aliases]\nco = \"log\"\n");
    // If the override won, `co` dispatches log -> "not initialized"; the shipped
    // `co` -> checkout would instead complain it needs a branch name.
    let out = stacc(tmp.path(), &["co", "--json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not initialized"),
        "co should map to log, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn alias_expands_to_a_git_passthrough() {
    let tmp = repo();
    write_stacc_toml(tmp.path(), "[aliases]\ncur = \"rev-parse --abbrev-ref HEAD\"\n");
    // `cur` -> `rev-parse --abbrev-ref HEAD` -> not a stacc builtin -> proxied
    // to git -> prints the current branch.
    let out = stacc(tmp.path(), &["cur"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "main");
}

#[test]
fn alias_threads_quoted_tokens_via_shlex() {
    let tmp = repo();
    // Quoted argument should survive into git's argv.
    write_stacc_toml(
        tmp.path(),
        "[aliases]\nmsg = \"commit --allow-empty -m \\\"hello world\\\"\"\n",
    );
    let out = stacc(tmp.path(), &["msg"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // Last commit's subject should be the quoted message.
    let subject = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&subject.stdout).trim(),
        "hello world"
    );
}
