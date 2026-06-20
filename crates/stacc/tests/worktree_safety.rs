//! Worktree-safety: a mutating operation must refuse to rewrite a branch that is
//! checked out in another worktree (rewriting it there would desync that
//! worktree). Focused ops (`modify`/`move`) fail fast with a `worktree_conflict`
//! error; bulk passes (`restack`) skip the borrowed branch and continue.

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

fn write_commit(dir: &Path, file: &str, contents: &str, msg: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run_git(dir, &["add", file]);
    run_git(dir, &["commit", "-q", "-m", msg]);
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with `main -> a -> b` tracked, where `b` is then checked out in a
/// separate linked worktree (so it is "elsewhere"), and the main worktree is
/// left on `a`. Returns the main repo dir and the worktree holder.
fn stack_with_b_elsewhere() -> (TempDir, TempDir) {
    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", "https://example.com/r.git"]);
    assert!(stacc(p, &["init"]).status.success());

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    assert!(stacc(p, &["track", "--base", "a"]).status.success());

    // Park the main worktree on `a`, then check `b` out in a linked worktree.
    run_git(p, &["checkout", "-q", "a"]);
    let holder = TempDir::new().expect("temp dir");
    let wt = holder.path().join("wt-b");
    run_git(p, &["worktree", "add", "-q", wt.to_str().unwrap(), "b"]);
    (tmp, holder)
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

#[test]
fn modify_refuses_when_an_upstack_branch_is_checked_out_elsewhere() {
    let (tmp, _holder) = stack_with_b_elsewhere();
    let p = tmp.path();
    let a_before = rev_parse(p, "a");

    // `a`'s upstack includes `b`, which lives in another worktree.
    std::fs::write(p.join("a.txt"), "a-modified\n").unwrap();
    run_git(p, &["add", "a.txt"]);
    let out = stacc(p, &["modify", "--json"]);

    assert!(!out.status.success(), "modify must refuse");
    assert_eq!(json(&out)["type"], "worktree_conflict");
    assert_eq!(json(&out)["branch"], "b");
    // `a` was not amended.
    assert_eq!(rev_parse(p, "a"), a_before, "a must be untouched");
}

#[test]
fn move_refuses_when_a_subtree_branch_is_checked_out_elsewhere() {
    let (tmp, _holder) = stack_with_b_elsewhere();
    let p = tmp.path();
    // Add a sibling base `side` off main to move `a`'s stack onto.
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["checkout", "-q", "-b", "side"]);
    write_commit(p, "side.txt", "s\n", "s1");
    assert!(stacc(p, &["track", "--base", "main"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);

    // `a`'s subtree includes `b`, checked out elsewhere, so the move must refuse
    // before re-pointing anything.
    let out = stacc(p, &["move", "--onto", "side", "--json"]);
    assert!(!out.status.success(), "move must refuse");
    assert_eq!(json(&out)["type"], "worktree_conflict");
    assert_eq!(json(&out)["branch"], "b");
}

#[test]
fn restack_skips_a_branch_checked_out_elsewhere_and_continues() {
    let (tmp, _holder) = stack_with_b_elsewhere();
    let p = tmp.path();
    let b_before = rev_parse(p, "b");

    // Advance main so the stack needs restacking.
    run_git(p, &["checkout", "-q", "main"]);
    write_commit(p, "m.txt", "m\n", "main moves");
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["restack", "--stack"]);
    assert!(out.status.success(), "restack should succeed around the skip");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another worktree") && stderr.contains("b ("),
        "restack should report b as worktree-skipped: {stderr}"
    );
    // `a` was restacked onto the new main; `b`'s tip is untouched.
    assert_eq!(rev_parse(p, "b"), b_before, "b must not be rewritten");
}

#[test]
fn modify_succeeds_when_nothing_is_checked_out_elsewhere() {
    // Same stack but no extra worktree: modifying `a` is fine.
    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", "https://example.com/r.git"]);
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    assert!(stacc(p, &["track", "--base", "a"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);

    std::fs::write(p.join("a.txt"), "a-modified\n").unwrap();
    run_git(p, &["add", "a.txt"]);
    let out = stacc(p, &["modify", "--json"]);
    assert!(out.status.success(), "modify should succeed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(json(&out)["op"], "modify");
}
