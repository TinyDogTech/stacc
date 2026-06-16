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
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""op":"checkout""#), "got: {s}");
    assert!(s.contains(r#""branch":"a""#), "got: {s}");
    assert!(s.contains(r#""moved":true"#), "got: {s}");
}

#[test]
fn checkout_a_nonexistent_branch_errors_without_moving() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a"); // on a
    let out = stacc(p, &["checkout", "ghost", "--format", "json"]);
    assert!(!out.status.success());
    assert_eq!(current_branch(p), "a"); // unchanged
}

#[test]
fn checkout_rejects_a_dash_prefixed_arg() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    let before = current_branch(p);
    // `--` ends clap parsing, so without the guard `--orphan=x` would reach
    // `git checkout` as an option and mutate the repo.
    let out = stacc(p, &["checkout", "--format", "json", "--", "--orphan=x"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not a valid branch name"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(current_branch(p), before); // repo untouched
}

#[test]
fn checkout_trunk_switches_to_the_trunk() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a"); // on a
    let out = stacc(p, &["checkout", "--trunk", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(current_branch(p), "main");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""branch":"main""#), "got: {s}");
    assert!(s.contains(r#""moved":true"#), "got: {s}");
}

#[test]
fn checkout_trunk_with_a_positional_branch_conflicts() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a"); // on a
    let out = stacc(p, &["checkout", "main", "--trunk"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot be used with"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(current_branch(p), "a"); // repo untouched
}

#[test]
fn checkout_stack_with_a_positional_branch_conflicts() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    for flag in ["--stack", "--all"] {
        let out = stacc(p, &["checkout", "main", flag]);
        assert!(!out.status.success(), "{flag} with a positional should conflict");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("cannot be used with"),
            "stderr for {flag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(current_branch(p), "a");
}

#[test]
fn bare_checkout_with_scope_flags_still_errors_off_a_terminal() {
    // --stack/--all scope the picker, but the non-interactive contract is
    // unchanged: bare + no TTY is the same structured error.
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    for flag in ["--stack", "--all"] {
        let out = stacc(p, &["checkout", flag, "--no-interactive", "--format", "json"]);
        assert!(!out.status.success(), "{flag} should not bypass the gate");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("needs a branch name"),
            "got for {flag}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
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
        String::from_utf8_lossy(&out.stdout).contains(r#""type":"usage""#),
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
