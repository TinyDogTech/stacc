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

fn stacc(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
}

fn json(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("bad json ({e}): {stdout}"))
}

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
fn cwd_flag_targets_repo_from_outside_directory() {
    let repo = init_repo();
    let outside = TempDir::new().expect("temp dir");
    let out = stacc(
        outside.path(),
        &[
            "-C",
            repo.path().to_str().expect("utf8 path"),
            "log",
            "--no-interactive",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "expected success; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let v = json(&out);
    assert!(v["trunk"].is_string(), "expected trunk key in: {v}");
}

#[test]
fn cwd_flag_nonexistent_path_exits_nonzero() {
    let outside = TempDir::new().expect("temp dir");
    let out = stacc(
        outside.path(),
        &["-C", "/tmp/stacc-test-nonexistent-path-xyz", "log", "--no-interactive", "--json"],
    );
    assert!(
        !out.status.success(),
        "expected failure for nonexistent path; stdout: {}",
        String::from_utf8_lossy(&out.stdout),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(!combined.is_empty(), "expected error output for nonexistent path");
}

#[test]
fn cwd_flag_non_git_directory_exits_nonzero() {
    let outside = TempDir::new().expect("temp dir");
    let non_git = TempDir::new().expect("temp dir");
    let out = stacc(
        outside.path(),
        &[
            "-C",
            non_git.path().to_str().expect("utf8 path"),
            "log",
            "--no-interactive",
            "--json",
        ],
    );
    assert!(
        !out.status.success(),
        "expected failure for non-git directory; stdout: {}",
        String::from_utf8_lossy(&out.stdout),
    );
}

#[test]
fn cwd_flag_absent_default_cwd_regression() {
    let repo = init_repo();
    let out = stacc(repo.path(), &["log", "--no-interactive", "--json"]);
    assert!(
        out.status.success(),
        "expected success without -C; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let v = json(&out);
    assert!(v["trunk"].is_string(), "expected trunk key in: {v}");
}
