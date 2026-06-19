//! `stacc merged`: reconcile a branch already merged into trunk without a forge.
//! Drops it, re-parents its children, keeps the dropped tip reachable, and only
//! disposes on a deterministic merge proof unless `--assume-merged` overrides.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

// A github URL so `stacc init` records a remote; `merged` makes no forge call
// (it reconciles against the local trunk), so nothing here is ever contacted.
const ORIGIN: &str = "https://github.com/stacc-sandbox/example.git";

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

fn branch_ref_exists(dir: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("spawn git")
        .status
        .success()
}

fn repo_init() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", ORIGIN]);
    assert!(stacc(p, &["init"]).status.success());
    tmp
}

fn track(p: &Path, base: &str) {
    assert!(
        stacc(p, &["track", "--base", base]).status.success(),
        "track on {base} failed"
    );
}

fn parent_of(p: &Path, branch: &str) -> String {
    run_git(p, &["checkout", "-q", branch]);
    String::from_utf8_lossy(&stacc(p, &["parent"]).stdout)
        .trim()
        .to_string()
}

#[test]
fn merged_drops_a_provably_merged_branch_and_reparents_children() {
    let tmp = repo_init();
    let p = tmp.path();

    // main <- a <- b.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track(p, "a");

    // a fast-forwards into main: a is now an ancestor of main (a deterministic
    // merge proof), and b sits on a.
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["merge", "-q", "--ff-only", "a"]);
    let a_tip = rev(p, "a");

    let out = stacc(p, &["merged", "a"]);
    assert!(
        out.status.success(),
        "merged a failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // a is dropped from the stack and its git ref is gone; b survives, re-parented
    // onto main.
    assert!(!branch_ref_exists(p, "a"), "a's git ref is removed");
    assert!(branch_ref_exists(p, "b"), "b survives");
    assert_eq!(parent_of(p, "b"), "main", "b re-parented onto main");

    // The dropped tip is kept reachable for recovery.
    assert_eq!(
        rev(p, &format!("refs/stacc/dropped/a-{a_tip}")),
        a_tip,
        "keep-alive ref preserves a's tip"
    );
}

#[test]
fn merged_refuses_an_unverified_branch_until_assume_merged() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    // a is never merged into main.
    run_git(p, &["checkout", "-q", "main"]);

    let refused = stacc(p, &["merged", "a"]);
    assert!(
        !refused.status.success(),
        "an unmerged branch must be refused"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("does not look merged") && stderr.contains("--assume-merged"),
        "refusal names the override: {stderr}"
    );
    assert!(branch_ref_exists(p, "a"), "refused: a is untouched");

    // The explicit override drops the named branch.
    let forced = stacc(p, &["merged", "a", "--assume-merged"]);
    assert!(
        forced.status.success(),
        "assume-merged failed: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(!branch_ref_exists(p, "a"), "a is dropped under --assume-merged");
    let a_tip_kept = Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["for-each-ref", "--format=%(refname)", "refs/stacc/dropped/"])
        .output()
        .expect("spawn git");
    assert!(
        String::from_utf8_lossy(&a_tip_kept.stdout).contains("refs/stacc/dropped/a-"),
        "a's tip is kept even under --assume-merged"
    );
}

#[test]
fn merged_refuses_the_trunk_and_untracked_branches() {
    let tmp = repo_init();
    let p = tmp.path();

    let trunk = stacc(p, &["merged", "main"]);
    assert!(!trunk.status.success(), "cannot reconcile the trunk");
    assert!(String::from_utf8_lossy(&trunk.stderr).contains("trunk"));

    let ghost = stacc(p, &["merged", "ghost"]);
    assert!(!ghost.status.success(), "cannot reconcile an untracked branch");
    assert!(String::from_utf8_lossy(&ghost.stderr).contains("not tracked"));
}

#[test]
fn merged_preserves_the_tip_when_the_children_restack_conflicts() {
    let tmp = repo_init();
    let p = tmp.path();

    // main <- a <- b, all editing the same line of x.txt.
    write_commit(p, "x.txt", "base\n", "base");
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "x.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "x.txt", "b\n", "b1");
    track(p, "a");

    // Advance main to a conflicting value, then capture a's tip before the drop.
    run_git(p, &["checkout", "-q", "main"]);
    write_commit(p, "x.txt", "trunk\n", "trunk advance");
    let a_tip = rev(p, "a");

    // a is not provably merged, so force the drop. Reparenting b onto the advanced
    // main then conflicts during the restack, exercising the conflict path.
    let out = stacc(p, &["merged", "a", "--assume-merged"]);
    assert!(!out.status.success(), "the children restack must conflict");

    // Even though the dispose stopped on a conflict, a's tip is preserved at its
    // keep-alive ref, so the documented recovery path still works (no data loss).
    assert_eq!(
        rev(p, &format!("refs/stacc/dropped/a-{a_tip}")),
        a_tip,
        "the dropped tip is preserved despite the restack conflict"
    );
}

#[test]
fn merged_json_reports_the_branch_and_evidence() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["merge", "-q", "--ff-only", "a"]);

    let out = stacc(p, &["merged", "a", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["op"], "merged");
    assert_eq!(v["branch"], "a");
    assert_eq!(v["evidence"], "ancestor");
}
