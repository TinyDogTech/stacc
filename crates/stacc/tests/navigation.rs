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

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn stacc(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
}

fn current_branch(dir: &Path) -> String {
    git_out(dir, &["symbolic-ref", "--short", "HEAD"])
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

/// `main -> a -> b -> c`, left checked out on `c`.
fn linear_stack() -> TempDir {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    create(p, "b");
    create(p, "c");
    tmp
}

/// `main -> a -> { b, c }`, left checked out on `a`.
fn forked_stack() -> TempDir {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a");
    create(p, "b");
    run_git(p, &["checkout", "-q", "a"]);
    create(p, "c");
    run_git(p, &["checkout", "-q", "a"]);
    tmp
}

#[test]
fn down_and_up_move_one_level() {
    let tmp = linear_stack();
    let p = tmp.path();
    assert!(stacc(p, &["down"]).status.success());
    assert_eq!(current_branch(p), "b");
    assert!(stacc(p, &["up"]).status.success());
    assert_eq!(current_branch(p), "c");
}

#[test]
fn down_with_steps_clamps_at_trunk() {
    let tmp = linear_stack();
    let p = tmp.path();
    let out = stacc(p, &["down", "5", "--format", "json"]);
    assert!(out.status.success());
    assert_eq!(current_branch(p), "main");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""branch":"main""#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn up_from_the_tip_is_a_no_op() {
    let tmp = linear_stack(); // on c, the tip
    let p = tmp.path();
    let out = stacc(p, &["up", "--format", "json"]);
    assert!(out.status.success());
    assert_eq!(current_branch(p), "c");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""moved":false"#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn top_and_bottom_jump_the_stack() {
    let tmp = linear_stack();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "b"]);
    assert!(stacc(p, &["bottom"]).status.success());
    assert_eq!(current_branch(p), "a"); // the trunk's child
    assert!(stacc(p, &["top"]).status.success());
    assert_eq!(current_branch(p), "c"); // the tip
}

#[test]
fn up_with_multiple_children_errors_with_choices() {
    let tmp = forked_stack(); // on a, children b + c
    let p = tmp.path();
    let out = stacc(p, &["up", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""type":"ambiguous""#), "got: {s}");
    assert!(s.contains(r#""b""#) && s.contains(r#""c""#), "got: {s}");
    // No move happened.
    assert_eq!(current_branch(p), "a");
}

#[test]
fn top_at_a_fork_errors_with_choices() {
    let tmp = forked_stack();
    let p = tmp.path();
    let out = stacc(p, &["top", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""type":"ambiguous""#), "got: {s}");
    assert!(s.contains(r#""b""#) && s.contains(r#""c""#), "got: {s}");
}

#[test]
fn up_with_steps_climbs_multiple_levels() {
    let tmp = linear_stack();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "a"]);
    assert!(stacc(p, &["up", "2"]).status.success());
    assert_eq!(current_branch(p), "c");
}

#[test]
fn pretty_output_reports_the_move() {
    let tmp = linear_stack(); // on c
    let p = tmp.path();
    let out = stacc(p, &["down"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Switched to b."),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(stacc(p, &["up"]).status.success()); // back on c (the tip)
    let out = stacc(p, &["up"]); // c is the tip: no move
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Already at c."),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
