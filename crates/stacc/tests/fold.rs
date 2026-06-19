//! `stacc fold`: fold the current branch into its parent. The precondition test
//! is load-bearing: a branch not restacked onto its parent must be refused, or
//! the parent's ref move would not be a fast-forward and would absorb commits
//! that are not part of the branch's own diff.

use std::path::Path;
use std::process::{Command, Output};

use httpmock::{Method, MockServer};
use stacc_git::Git;
use stacc_state::{Base, BranchState, PullRequest, StateStore};
use tempfile::TempDir;

// A nonexistent GitHub URL: parses to owner/repo (stacc-sandbox/example) for
// the `--close` tests; nothing here fetches or pushes it.
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

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git")
        .success()
}

fn stacc_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stacc"));
    cmd.current_dir(dir).args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn stacc")
}

fn stacc(dir: &Path, args: &[&str]) -> Output {
    stacc_env(dir, args, &[])
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

fn current_branch(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

fn log_json(dir: &Path) -> String {
    String::from_utf8_lossy(&stacc(dir, &["log", "--json"]).stdout).into_owned()
}

fn rebase_in_progress(dir: &Path) -> bool {
    let git_dir = dir.join(".git");
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

fn branch_ref_exists(dir: &Path, branch: &str) -> bool {
    // `output()` rather than `git_ok`: a successful verify prints the hash.
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
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

/// Track the current branch on `base`.
fn track(p: &Path, base: &str) {
    assert!(
        stacc(p, &["track", "--base", base]).status.success(),
        "track on {base} failed"
    );
}

/// Inject a recorded PR for `branch` (base `base`, hash from the live base ref).
fn track_pr(p: &Path, branch: &str, base: &str, number: u64) {
    let store = StateStore::new(Git::open(p));
    let mut state = store.load().unwrap();
    state.branches.insert(
        branch.to_string(),
        BranchState {
            base: Base {
                name: base.to_string(),
                hash: rev(p, base),
            },
            pr: Some(PullRequest { number, url: None }),
            pr_title: None,
            pr_description: None,
        },
    );
    store.save(&state).unwrap();
}

/// main <- a <- b <- c, each branch adding its own file, all tracked. Leaves
/// the repo checked out on `c`.
fn linear_stack(p: &Path) {
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a-content\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b-content\n", "b1");
    track(p, "a");
    run_git(p, &["checkout", "-q", "-b", "c"]);
    write_commit(p, "c.txt", "c-content\n", "c1");
    track(p, "b");
}

#[test]
fn fold_merges_into_the_parent_and_reparents_children() {
    let tmp = repo_init();
    let p = tmp.path();
    linear_stack(p);
    let a_before = rev(p, "a");
    let b_tip = rev(p, "b");
    let c_tip = rev(p, "c");

    run_git(p, &["checkout", "-q", "b"]);
    let out = stacc(p, &["fold", "--json"]);
    assert!(
        out.status.success(),
        "fold failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "fold");
    assert_eq!(v["branch"], "b");
    assert_eq!(v["into"], "a");
    assert!(v["pr_closed"].is_null(), "no --close, no PR: {v}");
    assert!(
        v.get("folded_into_trunk").is_none(),
        "a is not the trunk: {v}"
    );

    // The parent fast-forwarded to b's tip; b's own commit content is on a.
    assert_ne!(rev(p, "a"), a_before, "a moved");
    assert_eq!(rev(p, "a"), b_tip, "a fast-forwarded to b's tip");

    // b is gone from git and from state.
    assert!(!branch_ref_exists(p, "b"), "b's ref was deleted");
    let log = log_json(p);
    assert!(!log.contains(r#""name":"b""#), "b dropped from state: {log}");

    // c was reparented onto a (recorded base) and already sits on the new tip,
    // so no rewrite was needed.
    assert!(log.contains(r#""name":"c""#), "c still tracked: {log}");
    assert!(log.contains(r#""base":"a""#), "c reparented onto a: {log}");
    assert_eq!(rev(p, "c"), c_tip, "c already sat on the new tip");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "c"]));

    // Clean finish: on the parent, clean tree, no rebase left.
    assert_eq!(current_branch(p), "a");
    assert!(!rebase_in_progress(p));
    assert!(
        git_ok(p, &["diff", "--quiet", "HEAD"]),
        "the working tree is clean after fold"
    );
}

#[test]
fn fold_refuses_a_branch_not_restacked_onto_its_parent() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    let a_before = rev(p, "a");

    // Amend the trunk under `a` with raw git: `a` no longer descends main's
    // live tip, so the parent ref move would not be a fast-forward.
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["commit", "-q", "--amend", "--allow-empty", "-m", "first, amended"]);
    let main_after = rev(p, "main");
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["fold", "--json"]);
    assert!(!out.status.success(), "fold must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage", "structured refusal: {v}");
    let msg = v["message"].as_str().expect("message");
    assert!(
        msg.contains("restack"),
        "the error names `stacc restack` as the fix: {msg}"
    );

    // Nothing mutated.
    assert_eq!(rev(p, "a"), a_before, "a did not move");
    assert_eq!(rev(p, "main"), main_after, "main did not move");
    assert!(branch_ref_exists(p, "a"));
    assert!(log_json(p).contains(r#""name":"a""#), "a still tracked");
    assert!(!rebase_in_progress(p));
}

#[test]
fn fold_of_a_leaf_just_fast_forwards_the_parent() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track(p, "a");
    let b_tip = rev(p, "b");

    let out = stacc(p, &["fold", "--json"]);
    assert!(
        out.status.success(),
        "fold failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "fold");
    assert_eq!(v["into"], "a");
    assert_eq!(v["restacked"], serde_json::json!([]), "no children: {v}");

    assert_eq!(rev(p, "a"), b_tip, "a fast-forwarded to b's tip");
    assert!(!branch_ref_exists(p, "b"));
    assert!(!log_json(p).contains(r#""name":"b""#), "b dropped from state");
    assert_eq!(current_branch(p), "a");
}

#[test]
fn fold_onto_the_trunk_works_but_warns() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    let a_tip = rev(p, "a");

    let out = stacc(p, &["fold", "--json"]);
    assert!(
        out.status.success(),
        "fold onto the trunk is allowed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "fold");
    assert_eq!(v["into"], "main");
    assert_eq!(v["folded_into_trunk"], true, "flagged in JSON: {v}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("trunk"),
        "warned about folding into the trunk: {stderr}"
    );

    assert_eq!(rev(p, "main"), a_tip, "the trunk fast-forwarded to a's tip");
    assert!(!branch_ref_exists(p, "a"));
    assert_eq!(current_branch(p), "main");
}

/// main <- a <- b <- c where `b` was amended out from under `c` with raw git,
/// so `c` is stale: folding `b` reparents `c` onto `a` (now at b's amended
/// tip) and the replay of c's old lineage conflicts on `shared.txt`. Leaves
/// the repo checked out on `b`, ready to fold.
fn stale_child_conflict_stack(p: &Path) {
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "shared.txt", "b-version\n", "b1");
    track(p, "a");
    run_git(p, &["checkout", "-q", "-b", "c"]);
    write_commit(p, "c.txt", "c\n", "c1");
    track(p, "b");

    // Amend b with raw git: c keeps sitting on b's OLD tip (stale state).
    run_git(p, &["checkout", "-q", "b"]);
    std::fs::write(p.join("shared.txt"), "b-amended\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    run_git(p, &["commit", "-q", "--amend", "-m", "b1 amended"]);
}

#[test]
fn abort_of_a_conflicted_fold_restores_the_pre_fold_state() {
    let tmp = repo_init();
    let p = tmp.path();
    stale_child_conflict_stack(p);
    let a_before = rev(p, "a");
    let b_tip = rev(p, "b");
    let c_tip = rev(p, "c");

    let out = stacc(p, &["fold", "--json"]);
    assert!(!out.status.success(), "expected a conflict on c");
    let v = json(&out);
    assert_eq!(v["type"], "conflict", "structured conflict: {v}");
    assert_eq!(v["branch"], "c");
    let cont = std::fs::read_to_string(p.join(".git/stacc-continue.json")).expect("continuation");
    assert!(cont.contains(r#""op":"fold""#), "got: {cont}");
    assert!(cont.contains(r#""branch":"b""#), "got: {cont}");

    let out = stacc(p, &["abort", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The parent's old tip is back, b's ref never moved, c's rebase unwound.
    assert_eq!(rev(p, "a"), a_before, "a rolled back to its pre-fold tip");
    assert_eq!(rev(p, "b"), b_tip, "b's ref survived");
    assert_eq!(rev(p, "c"), c_tip, "c's rebase was aborted");
    assert!(!rebase_in_progress(p));
    assert!(!p.join(".git/stacc-continue.json").exists());

    // b is re-tracked with its old base, and c points back at b.
    let log = log_json(p);
    assert!(log.contains(r#""name":"b""#), "b re-tracked: {log}");
    assert!(log.contains(r#""base":"b""#), "c re-pointed onto b: {log}");
}

#[test]
fn continue_of_a_conflicted_fold_finishes_the_fold() {
    let tmp = repo_init();
    let p = tmp.path();
    stale_child_conflict_stack(p);
    let b_tip = rev(p, "b");

    assert!(
        !stacc(p, &["fold", "--json"]).status.success(),
        "expected a conflict on c"
    );

    // Resolve the conflict and continue.
    std::fs::write(p.join("shared.txt"), "resolved\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    let out = stacc(p, &["continue", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "fold");
    assert_eq!(v["branch"], "b");
    assert_eq!(v["into"], "a");
    let restacked = v["restacked"].as_array().expect("restacked array");
    assert!(restacked.iter().any(|x| x == "c"), "c restacked: {v}");

    // The resumed fold still finished its ref surgery: parent at b's tip,
    // b's ref deleted, the user left on the parent, c rebased onto it.
    assert_eq!(rev(p, "a"), b_tip);
    assert!(!branch_ref_exists(p, "b"));
    assert_eq!(current_branch(p), "a");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "c"]));
    let log = log_json(p);
    assert!(!log.contains(r#""name":"b""#), "b dropped from state: {log}");
    assert!(log.contains(r#""base":"a""#), "c based on a: {log}");
    assert!(!rebase_in_progress(p));
}

#[test]
fn fold_refuses_when_the_parent_is_checked_out_in_another_worktree() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track(p, "a");
    let a_before = rev(p, "a");
    let b_before = rev(p, "b");

    // The PARENT in another worktree: folding moves a's ref, so refuse.
    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-a").to_str().unwrap(), "a"],
    );

    let out = stacc(p, &["fold", "--json"]);
    assert!(!out.status.success(), "fold must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(
        v["type"], "worktree_conflict",
        "refused with the worktree_conflict discriminator: {v}"
    );
    assert_eq!(v["branch"], "a", "names the parent: {v}");

    // Nothing mutated.
    assert_eq!(rev(p, "a"), a_before);
    assert_eq!(rev(p, "b"), b_before);
    assert!(log_json(p).contains(r#""name":"b""#), "b still tracked");
    assert!(!rebase_in_progress(p));
}

#[test]
fn fold_close_closes_the_recorded_pr() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track_pr(p, "b", "a", 7);

    let server = MockServer::start();
    let close = server.mock(|when, then| {
        when.method(Method::PATCH)
            .path("/repos/stacc-sandbox/example/pulls/7")
            .json_body(serde_json::json!({ "state": "closed" }));
        then.status(200).json_body(serde_json::json!({
            "number": 7, "html_url": "u", "state": "closed", "merged": false,
        }));
    });

    let out = stacc_env(
        p,
        &["fold", "--close", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["pr_closed"], true, "PR close reported: {v}");
    close.assert();
    assert!(!branch_ref_exists(p, "b"));
    assert_eq!(current_branch(p), "a");
}

#[test]
fn fold_close_failure_is_reported_not_fatal() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track_pr(p, "b", "a", 7);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::PATCH)
            .path("/repos/stacc-sandbox/example/pulls/7");
        then.status(500)
            .json_body(serde_json::json!({ "message": "boom" }));
    });

    let out = stacc_env(
        p,
        &["fold", "--close", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "a failed close must not fail the fold: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "fold", "the fold itself completed: {v}");
    assert_eq!(v["pr_closed"], false, "the failed close is reported: {v}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not close"),
        "stderr names the failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!branch_ref_exists(p, "b"), "the fold still completed");
}
