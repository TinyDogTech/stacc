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

fn current_branch(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

fn create(p: &Path, name: &str) {
    std::fs::write(p.join(format!("{name}.txt")), "x\n").expect("write");
    run_git(p, &["add", "."]);
    assert!(stacc(p, &["create", name, "-m", name]).status.success());
}

#[test]
fn checkout_switches_to_an_explicit_branch() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    run_git(p, &["checkout", "-q", "main"]);

    let out = stacc(p, &["checkout", "a", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(current_branch(p), "a");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""branch":"a""#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn bare_checkout_with_no_interactive_errors() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    let out = stacc(p, &["checkout", "--no-interactive", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("needs a branch name"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn bare_checkout_with_json_errors() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    let out = stacc(p, &["checkout", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""error":"usage""#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn bare_checkout_without_a_terminal_errors() {
    // Pretty mode, no --no-interactive, but stdin is not a TTY under `output()`,
    // so the gate refuses rather than prompting.
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    let out = stacc(p, &["checkout"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("needs a branch name"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
