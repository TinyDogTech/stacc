//! `stacc delete`: delete a tracked branch, reparenting and restacking its
//! children onto its base. The safety-predicate tests are load-bearing: an
//! unmerged, non-empty branch whose PR is not closed/merged must be refused
//! without `--force`, and an unfetchable PR state must read as unknown (not
//! safe), so a network failure can never wave a destructive delete through.

use std::path::Path;
use std::process::{Command, Output};

use httpmock::{Method, MockServer};
use stacc_git::Git;
use stacc_state::{Base, BranchState, PullRequest, StateStore};
use tempfile::TempDir;

// A nonexistent GitHub URL: parses to owner/repo (stacc-sandbox/example) for
// the mocked PR calls; nothing here fetches or pushes it.
const ORIGIN: &str = "https://github.com/stacc-sandbox/example.git";
// An unroutable API URL: forces the best-effort PR fetch to fail fast, so the
// predicate's "unknown is not safe" path is deterministic even on a machine
// with a real GITHUB_TOKEN in the environment.
const DEAD_API: &str = "http://127.0.0.1:1";

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

fn branch_ref_exists(dir: &Path, branch: &str) -> bool {
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

#[test]
fn delete_of_a_merged_branch_reparents_and_restacks_children() {
    let tmp = repo_init();
    let p = tmp.path();

    // main <- a <- b (with a PR) <- c.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a-content\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b-content\n", "b1");
    track_pr(p, "b", "a", 7);
    run_git(p, &["checkout", "-q", "-b", "c"]);
    write_commit(p, "c.txt", "c-content\n", "c1");
    track(p, "b");

    // Merge b into a with raw git: b is now an ancestor of a's live tip, the
    // first safety escape hatch.
    run_git(p, &["checkout", "-q", "a"]);
    run_git(p, &["merge", "-q", "--no-ff", "-m", "merge b", "b"]);
    run_git(p, &["checkout", "-q", "c"]);

    // A mock API with no GET expectation: the predicate must short-circuit on
    // merged-into-base, and without --close the PR must be left untouched.
    let server = MockServer::start();
    let close = server.mock(|when, then| {
        when.method(Method::PATCH)
            .path("/repos/stacc-sandbox/example/pulls/7");
        then.status(200).json_body(serde_json::json!({
            "number": 7, "html_url": "u", "state": "closed", "merged": false,
        }));
    });

    let out = stacc_env(
        p,
        &["delete", "b", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "delete failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "delete");
    assert_eq!(v["branch"], "b");
    assert_eq!(v["reparented"], serde_json::json!(["c"]));
    let restacked = v["restacked"].as_array().expect("restacked array");
    assert!(restacked.iter().any(|x| x == "c"), "c restacked: {v}");
    assert!(v["pr_closed"].is_null(), "no --close: {v}");
    close.assert_hits(0);

    // b is gone from git and from state; c sits on a with only its own commit.
    assert!(!branch_ref_exists(p, "b"), "b's ref was deleted");
    let log = log_json(p);
    assert!(!log.contains(r#""name":"b""#), "b dropped from state: {log}");
    assert!(log.contains(r#""base":"a""#), "c reparented onto a: {log}");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "c"]));
    assert!(git_ok(p, &["cat-file", "-e", "c:c.txt"]), "c kept its commit");

    // The user was restored to where they started.
    assert_eq!(current_branch(p), "c");
}

#[test]
fn delete_of_an_empty_diff_branch_is_safe_without_force() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    // b has commits but a net-empty diff vs a: add a file, then remove it.
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "tmp.txt", "tmp\n", "b1");
    run_git(p, &["rm", "-q", "tmp.txt"]);
    run_git(p, &["commit", "-q", "-m", "b2"]);
    track(p, "a");
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["delete", "b", "--json"]);
    assert!(
        out.status.success(),
        "an empty-diff delete needs no --force: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!branch_ref_exists(p, "b"));
    assert!(!log_json(p).contains(r#""name":"b""#), "b dropped from state");
}

#[test]
fn delete_refuses_an_unmerged_branch_and_force_overrides() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track(p, "a");
    run_git(p, &["checkout", "-q", "a"]);
    let b_tip = rev(p, "b");

    let out = stacc(p, &["delete", "b", "--json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage", "structured refusal: {v}");
    let msg = v["message"].as_str().expect("message");
    assert!(msg.contains("--force"), "the error names --force: {msg}");
    assert!(
        msg.contains("reparented"),
        "the error warns about the children: {msg}"
    );
    assert_eq!(rev(p, "b"), b_tip, "nothing mutated");
    assert!(log_json(p).contains(r#""name":"b""#), "b still tracked");

    let out = stacc(p, &["delete", "b", "--force", "--json"]);
    assert!(
        out.status.success(),
        "--force overrides: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!branch_ref_exists(p, "b"));
    assert!(!log_json(p).contains(r#""name":"b""#), "b dropped from state");
}

#[test]
fn delete_refuses_when_the_open_pr_state_says_open() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track_pr(p, "b", "a", 7);
    run_git(p, &["checkout", "-q", "a"]);

    let server = MockServer::start();
    let get = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/repos/stacc-sandbox/example/pulls/7");
        then.status(200).json_body(serde_json::json!({
            "number": 7, "html_url": "u", "state": "open", "merged": false,
        }));
    });

    let out = stacc_env(
        p,
        &["delete", "b", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(!out.status.success(), "an open PR is not safe: {:?}", json(&out));
    assert_eq!(json(&out)["type"], "usage");
    get.assert();
    assert!(branch_ref_exists(p, "b"), "nothing mutated");
}

#[test]
fn delete_allows_when_the_pr_is_closed() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track_pr(p, "b", "a", 7);
    run_git(p, &["checkout", "-q", "a"]);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET)
            .path("/repos/stacc-sandbox/example/pulls/7");
        then.status(200).json_body(serde_json::json!({
            "number": 7, "html_url": "u", "state": "closed", "merged": false,
        }));
    });

    let out = stacc_env(
        p,
        &["delete", "b", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "a closed PR is safe to delete: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!branch_ref_exists(p, "b"));
}

#[test]
fn delete_treats_an_unfetchable_pr_state_as_unsafe() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track_pr(p, "b", "a", 7);
    run_git(p, &["checkout", "-q", "a"]);

    // The API is unreachable: the PR state is unknown, which must NOT count as
    // safe, so the delete is refused.
    let out = stacc_env(
        p,
        &["delete", "b", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", DEAD_API)],
    );
    assert!(
        !out.status.success(),
        "unknown PR state must refuse: {:?}",
        json(&out)
    );
    assert_eq!(json(&out)["type"], "usage");
    assert!(branch_ref_exists(p, "b"), "nothing mutated");
    assert!(log_json(p).contains(r#""name":"b""#), "b still tracked");
}

#[test]
fn delete_close_closes_the_recorded_pr() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track_pr(p, "b", "a", 7);
    run_git(p, &["checkout", "-q", "a"]);

    let server = MockServer::start();
    let close = server.mock(|when, then| {
        when.method(Method::PATCH)
            .path("/repos/stacc-sandbox/example/pulls/7")
            .json_body(serde_json::json!({ "state": "closed" }));
        then.status(200).json_body(serde_json::json!({
            "number": 7, "html_url": "u", "state": "closed", "merged": false,
        }));
    });

    // --force skips the predicate (and its GET), so only the PATCH is mocked.
    let out = stacc_env(
        p,
        &["delete", "b", "--force", "--close", "--json"],
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
}

#[test]
fn delete_close_failure_is_reported_not_fatal() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track_pr(p, "b", "a", 7);
    run_git(p, &["checkout", "-q", "a"]);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::PATCH)
            .path("/repos/stacc-sandbox/example/pulls/7");
        then.status(500)
            .json_body(serde_json::json!({ "message": "boom" }));
    });

    let out = stacc_env(
        p,
        &["delete", "b", "--force", "--close", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "a failed close must not fail the delete: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "delete", "the delete itself completed: {v}");
    assert_eq!(v["pr_closed"], false, "the failed close is reported: {v}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not close"),
        "stderr names the failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!branch_ref_exists(p, "b"), "the delete still completed");
}

#[test]
fn delete_refuses_the_trunk() {
    let tmp = repo_init();
    let p = tmp.path();

    let out = stacc(p, &["delete", "main", "--json"]);
    assert!(!out.status.success());
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    assert!(
        v["message"].as_str().expect("message").contains("trunk"),
        "names the trunk: {v}"
    );
}

#[test]
fn delete_refuses_an_untracked_branch_with_a_raw_git_hint() {
    let tmp = repo_init();
    let p = tmp.path();
    run_git(p, &["branch", "-q", "loose"]);

    let out = stacc(p, &["delete", "loose", "--json"]);
    assert!(!out.status.success());
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    let msg = v["message"].as_str().expect("message");
    assert!(
        msg.contains("git branch -D"),
        "hints at raw git for untracked branches: {msg}"
    );
    assert!(branch_ref_exists(p, "loose"), "the raw branch is untouched");
}

#[test]
fn delete_of_the_current_branch_checks_out_the_trunk_first() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track(p, "main");

    // On b, deleting b: stacc must move HEAD to the trunk before deleting.
    let out = stacc(p, &["delete", "b", "--force", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(current_branch(p), "main");
    assert!(!branch_ref_exists(p, "b"));
    assert!(!log_json(p).contains(r#""name":"b""#), "b dropped from state");
}

#[test]
fn delete_refuses_when_a_child_is_in_another_worktree() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    track(p, "a");
    run_git(p, &["checkout", "-q", "-b", "c"]);
    write_commit(p, "c.txt", "c\n", "c1");
    track(p, "b");
    run_git(p, &["checkout", "-q", "a"]);
    let b_tip = rev(p, "b");

    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-c").to_str().unwrap(), "c"],
    );

    let out = stacc(p, &["delete", "b", "--force", "--json"]);
    assert!(!out.status.success(), "must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(
        v["type"], "worktree_conflict",
        "refused with the worktree_conflict discriminator: {v}"
    );
    assert_eq!(v["branch"], "c", "names the borrowed child: {v}");

    // Nothing mutated.
    assert_eq!(rev(p, "b"), b_tip);
    assert!(branch_ref_exists(p, "b"));
    assert!(log_json(p).contains(r#""name":"b""#), "b still tracked");
}
