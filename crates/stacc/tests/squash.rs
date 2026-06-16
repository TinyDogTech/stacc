//! `stacc squash`: collapse the current branch's own commits into one, then
//! restack the upstack. The precondition test is load-bearing: a branch not
//! restacked onto its base must be refused, or the squash silently folds in
//! commits that are not part of the branch's own diff.

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

fn blob_at(dir: &Path, rev: &str, path: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "-p", &format!("{rev}:{path}")])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The full message (`%B`) of `r`'s tip commit.
fn message_at(dir: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "-1", "--format=%B", r])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The number of commits in `base..tip`.
fn count_commits(dir: &Path, base: &str, tip: &str) -> usize {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-list", "--count", &format!("{base}..{tip}")])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().parse().expect("count")
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

fn rebase_in_progress(dir: &Path) -> bool {
    let git_dir = dir.join(".git");
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
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

/// Track the current branch on `base`.
fn track(p: &Path, base: &str) {
    assert!(
        stacc(p, &["track", "--base", base]).status.success(),
        "track on {base} failed"
    );
}

#[test]
fn squash_collapses_three_commits_and_restacks_the_upstack() {
    let tmp = repo_init();
    let p = tmp.path();

    // Branch `a` with three commits, each introducing a distinct file.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "f-content\n", "c1: add f");
    write_commit(p, "g.txt", "g-content\n", "c2: add g");
    write_commit(p, "h.txt", "h-content\n", "c3: add h");
    track(p, "main");

    // An upstack child so we can confirm it restacks onto the squashed tip.
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "i.txt", "i-content\n", "b1");
    track(p, "a");
    let b_before = rev(p, "b");

    run_git(p, &["checkout", "-q", "a"]);
    let out = stacc(p, &["squash", "--format", "json"]);
    assert!(
        out.status.success(),
        "squash failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "squash");
    assert_eq!(v["branch"], "a");
    assert_eq!(v["squashed"], 3, "three commits collapsed: {v}");

    // `git log` now shows exactly one own commit on `a`, and the reported sha
    // is the new tip.
    assert_eq!(count_commits(p, "main", "a"), 1, "a has one own commit");
    let new_tip = rev(p, "a");
    assert_eq!(v["sha"].as_str(), Some(new_tip.as_str()));

    // The tip TREE content is unchanged: every file survives byte-identically.
    assert_eq!(blob_at(p, &new_tip, "f.txt"), "f-content\n");
    assert_eq!(blob_at(p, &new_tip, "g.txt"), "g-content\n");
    assert_eq!(blob_at(p, &new_tip, "h.txt"), "h-content\n");

    // The upstack child `b` restacked onto the squashed tip.
    let restacked = v["restacked"].as_array().expect("restacked array");
    assert!(restacked.iter().any(|x| x == "b"), "b restacked: {v}");
    assert_ne!(rev(p, "b"), b_before, "b moved onto the squashed tip");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["merge-base", "--is-ancestor", &new_tip, "b"])
            .status()
            .unwrap()
            .success(),
        "b descends the squashed tip"
    );
    assert_eq!(blob_at(p, "b", "i.txt"), "i-content\n");

    // Clean finish: back on `a`, clean status, no rebase left.
    assert!(!rebase_in_progress(p), "no rebase left in progress");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--quiet", "HEAD"])
            .status()
            .unwrap()
            .success(),
        "the working tree is clean after squash"
    );
}

#[test]
fn squash_refuses_a_branch_not_restacked_onto_its_base() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "f\n", "c1");
    write_commit(p, "g.txt", "g\n", "c2");
    track(p, "main");
    let tip_before = rev(p, "a");

    // Amend the trunk under `a` with raw git: `a` no longer descends main's
    // live tip, so `main..a` would include the stale trunk commit.
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["commit", "-q", "--amend", "--allow-empty", "-m", "first, amended"]);
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["squash", "--format", "json"]);
    assert!(!out.status.success(), "squash must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage", "structured refusal: {v}");
    let msg = v["message"].as_str().expect("message");
    assert!(
        msg.contains("restack"),
        "the error names `stacc restack` as the fix: {msg}"
    );

    // Nothing mutated.
    assert_eq!(rev(p, "a"), tip_before, "a did not move");
    assert!(!rebase_in_progress(p));
}

#[test]
fn squash_on_a_single_commit_branch_is_a_reported_noop() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "f\n", "only commit");
    track(p, "main");
    let tip_before = rev(p, "a");

    let out = stacc(p, &["squash", "--format", "json"]);
    assert!(
        out.status.success(),
        "a no-op squash is not an error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "squash");
    assert_eq!(v["squashed"], 0, "nothing to squash: {v}");

    // The ref did not move and the commit is untouched.
    assert_eq!(rev(p, "a"), tip_before, "a did not move");
    assert!(!rebase_in_progress(p));
}

#[test]
fn squash_default_message_concatenates_oldest_first() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "f\n", "older subject\n\nolder body");
    write_commit(p, "g.txt", "g\n", "newer subject");
    track(p, "main");

    let out = stacc(p, &["squash", "--format", "json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // Both subjects (and the body) appear, oldest first.
    let message = message_at(p, "a");
    let older = message.find("older subject").expect("older subject present");
    let newer = message.find("newer subject").expect("newer subject present");
    assert!(older < newer, "oldest-first concatenation: {message}");
    assert!(message.contains("older body"), "bodies survive: {message}");
}

#[test]
fn squash_message_flag_overrides_the_subject() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "f\n", "c1");
    write_commit(p, "g.txt", "g\n", "c2");
    track(p, "main");

    let out = stacc(p, &["squash", "--message", "one commit to rule them all", "--format", "json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["squashed"], 2);

    let subject = Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["log", "-1", "--format=%s", "a"])
        .output()
        .expect("spawn git");
    assert_eq!(
        String::from_utf8_lossy(&subject.stdout).trim(),
        "one commit to rule them all"
    );
    assert_eq!(count_commits(p, "main", "a"), 1);
}

#[test]
fn squash_refuses_when_an_upstack_branch_is_in_another_worktree() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "f\n", "c1");
    write_commit(p, "g.txt", "g\n", "c2");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "h.txt", "h\n", "b1");
    track(p, "a");
    let a_before = rev(p, "a");
    let b_before = rev(p, "b");

    // Move off `b`, then check it out in another worktree.
    run_git(p, &["checkout", "-q", "a"]);
    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-b").to_str().unwrap(), "b"],
    );

    let out = stacc(p, &["squash", "--format", "json"]);
    assert!(!out.status.success(), "squash must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(
        v["type"], "worktree_conflict",
        "refused with the worktree_conflict discriminator: {v}"
    );

    // Nothing mutated: neither branch moved, no rebase in progress.
    assert_eq!(rev(p, "a"), a_before, "a did not move");
    assert_eq!(rev(p, "b"), b_before, "b did not move");
    assert!(!rebase_in_progress(p));
}
