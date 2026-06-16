//! `stacc undo`: revert the most recent mutation(s), restoring prior state and
//! the affected branch tips, bounded by the retention window and safe across
//! worktrees.

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

fn rev(dir: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", r])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

fn repo_init() -> TempDir {
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
fn undo_restores_a_modified_branch_tip() {
    let tmp = repo_init();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    let tip0 = rev(p, "a");
    assert!(stacc(p, &["track"]).status.success());

    std::fs::write(p.join("a.txt"), "a-modified\n").unwrap();
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["modify"]).status.success());
    assert_ne!(tip0, rev(p, "a"), "modify amended a");

    let out = stacc(p, &["undo", "--format", "json"]);
    assert!(
        out.status.success(),
        "undo failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "undo");
    assert_eq!(v["restored"], serde_json::json!(["a"]));
    assert_eq!(rev(p, "a"), tip0, "a is restored to its pre-modify tip");
}

#[test]
fn undo_steps_walks_multiple_versions_back() {
    let tmp = repo_init();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    let tip0 = rev(p, "a");
    assert!(stacc(p, &["track"]).status.success()); // V1

    for content in ["a2\n", "a3\n"] {
        std::fs::write(p.join("a.txt"), content).unwrap();
        run_git(p, &["add", "a.txt"]);
        assert!(stacc(p, &["modify"]).status.success()); // V2, V3
    }

    let out = stacc(p, &["undo", "--steps", "2", "--format", "json"]);
    assert!(
        out.status.success(),
        "undo --steps 2 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(rev(p, "a"), tip0, "two undos back lands on a's original tip");
}

#[test]
fn undo_of_track_clears_state_without_moving_the_ref() {
    let tmp = repo_init();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    let a_tip = rev(p, "a");
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc(p, &["undo", "--format", "json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // `track` did not move the branch ref, so undo leaves it where it is.
    assert_eq!(rev(p, "a"), a_tip);
    // `a` is no longer tracked.
    let log = json(&stacc(p, &["log", "--format", "json"]));
    assert!(
        log["stack"].as_array().is_none_or(Vec::is_empty),
        "a is no longer tracked: {log}"
    );
}

#[test]
fn undo_skips_a_branch_checked_out_in_another_worktree() {
    let tmp = repo_init();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    assert!(stacc(p, &["track", "--base", "a"]).status.success());

    // Amend `a`, which restacks `b` (so undo will want to restore b's tip).
    run_git(p, &["checkout", "-q", "a"]);
    std::fs::write(p.join("a.txt"), "a-mod\n").unwrap();
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["modify"]).status.success());
    let b_restacked = rev(p, "b");

    // Now check `b` out in another worktree, then undo from the main worktree.
    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-b").to_str().unwrap(), "b"],
    );

    let out = stacc(p, &["undo", "--format", "json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    let skipped = v["worktree_skipped"].as_array().expect("array");
    assert!(
        skipped.iter().any(|e| e["branch"] == "b"),
        "b should be worktree-skipped: {v}"
    );
    assert_eq!(rev(p, "b"), b_restacked, "b in another worktree is not rewritten");
}

#[test]
fn undo_skips_the_current_branch_when_the_tree_is_dirty() {
    let tmp = repo_init();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    assert!(stacc(p, &["track"]).status.success());
    std::fs::write(p.join("a.txt"), "a-mod\n").unwrap();
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["modify"]).status.success());
    let after_modify = rev(p, "a");

    // Dirty the working tree, a hard reset would discard this.
    std::fs::write(p.join("a.txt"), "uncommitted\n").unwrap();

    let out = stacc(p, &["undo", "--format", "json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert!(
        v["dirty_skipped"].as_array().unwrap().iter().any(|b| b == "a"),
        "a should be skipped while dirty: {v}"
    );
    assert_eq!(rev(p, "a"), after_modify, "a is not reset while the tree is dirty");
}

#[test]
fn undo_beyond_retention_is_a_structured_error() {
    let tmp = repo_init();
    let p = tmp.path();
    let out = stacc(p, &["undo", "--steps", "100", "--format", "json"]);
    assert!(!out.status.success(), "should refuse beyond retention");
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    assert!(
        v["message"].as_str().unwrap().contains("50"),
        "names the retention bound: {v}"
    );
}

#[test]
fn undo_with_no_earlier_version_says_nothing_to_undo() {
    let tmp = repo_init(); // only the init version exists
    let p = tmp.path();
    let out = stacc(p, &["undo", "--format", "json"]);
    assert!(!out.status.success(), "nothing to undo");
    assert!(
        json(&out)["message"]
            .as_str()
            .unwrap()
            .contains("nothing to undo"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
