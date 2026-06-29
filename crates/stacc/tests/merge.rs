use std::path::Path;
use std::process::{Command, Output};

use httpmock::{Method, MockServer};
use stacc_git::Git;
use stacc_state::{Base, BranchState, PullRequest, StateStore};
use tempfile::TempDir;

// A nonexistent GitHub URL: parses to owner/repo (stacc-sandbox/example); `git
// fetch` would fail, so the tests use `merge --offline`.
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

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

fn ref_exists(dir: &Path, branch: &str) -> bool {
    // Capture the output: `--verify` prints the hash on success, which would
    // otherwise leak into the test output via the inherited stdout.
    !git_out(
        dir,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )
    .is_empty()
}

fn repo() -> TempDir {
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

fn pr_open(number: u64, mergeable_state: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "html_url": "u",
        "state": "open",
        "merged": false,
        "mergeable_state": mergeable_state,
    })
}

/// Inject a recorded PR for `branch` (base `base`, hash from the live ref).
fn track_pr(p: &Path, branch: &str, base: &str, number: u64) {
    let store = StateStore::new(Git::open(p));
    let mut state = store.load().unwrap();
    state.branches.insert(
        branch.to_string(),
        BranchState {
            base: Base {
                name: base.to_string(),
                hash: git_out(p, &["rev-parse", base]),
            },
            pr: Some(PullRequest { number, url: None }),
            pr_title: None,
            pr_description: None,
        },
    );
    store.save(&state).unwrap();
}

fn commit_file(p: &Path, name: &str, content: &str, msg: &str) {
    std::fs::write(p.join(name), content).expect("write file");
    run_git(p, &["add", name]);
    run_git(p, &["commit", "-q", "-m", msg]);
}

/// A working repo whose `origin` is a GitHub-shaped URL (so `parse_remote`
/// works) but is rewritten via `insteadOf` to a local bare repo, so the online
/// merge's fetch + force-push actually succeed. Returns (work tree, bare remote).
fn online_repo() -> (TempDir, TempDir) {
    let bare = TempDir::new().expect("bare");
    run_git(bare.path(), &["init", "-q", "--bare", "-b", "main"]);
    let tmp = TempDir::new().expect("work");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    let insteadof = format!("url.{}.insteadOf", bare.path().display());
    run_git(p, &["config", &insteadof, ORIGIN]);
    run_git(p, &["remote", "add", "origin", ORIGIN]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["push", "-q", "origin", "main"]);
    assert!(stacc(p, &["init"]).status.success());
    (tmp, bare)
}

#[test]
fn merge_squashes_ready_downstack_and_stops_at_unready() {
    let tmp = repo();
    let p = tmp.path();
    // main -> bottom -> middle -> top (empty commits, no conflicts).
    run_git(p, &["checkout", "-q", "-b", "bottom"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "middle"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "m"]);
    run_git(p, &["checkout", "-q", "-b", "top"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "t"]);
    track_pr(p, "bottom", "main", 1);
    track_pr(p, "middle", "bottom", 2);
    track_pr(p, "top", "middle", 3);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    // Trunk is unprotected.
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    // Readiness: bottom + middle clean, top blocked.
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "blocked"));
    });
    // Every non-bottom open PR is retargeted to the trunk UP FRONT, before any
    // merge and regardless of readiness (top is `blocked` yet still retargeted).
    let patch2 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/2"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    let patch3 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/3"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(3, "blocked"));
    });
    // Squash-merges for the two ready PRs.
    let merge1 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    let merge2 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "top"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // bottom (#1) and middle (#2) merged; stopped at top (#3, blocked).
    assert!(s.contains(r#""number":1"#) && s.contains(r#""number":2"#), "got: {s}");
    assert!(s.contains(r#""stopped_at""#), "got: {s}");
    assert!(s.contains(r#""readiness":"blocked""#), "got: {s}");
    assert!(s.contains(r#""trunk_protected":false"#), "got: {s}");
    merge1.assert();
    merge2.assert();
    // Both non-bottom PRs were retargeted to the trunk up front, including `top`
    // which never merges (it is blocked), proving retarget precedes readiness.
    patch2.assert();
    patch3.assert();
    // The no-protection warning is surfaced.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("has no branch protection"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // bottom + middle dropped from state; top remains.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"bottom""#) && !log.contains(r#""name":"middle""#),
        "merged branches not dropped: {log}"
    );
    assert!(log.contains(r#""name":"top""#), "got: {log}");
}

// STA-119: merging from a mid-stack branch (chain = [a, b]) must also retarget
// the PR of the direct upstack child of the chain top (c, whose state-base is b).
// Without the fix, deleting b's branch on merge orphans c's PR base.
#[test]
fn merge_retargets_upstack_child_of_chain_top() {
    let tmp = repo();
    let p = tmp.path();
    // main -> a -> b -> c (linear stack).  User is on `b`; chain = [a, b].
    // `c` is outside the merge chain but its PR base (b) will be deleted on merge.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "c"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "c"]);
    track_pr(p, "a", "main", 1);
    track_pr(p, "b", "a", 2);
    track_pr(p, "c", "b", 3);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    // c's PR is checked during retarget (open-state guard before PATCH).
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    // b (PR #2) is retargeted because it is the non-bottom member of the chain.
    let patch2 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/2"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    // STA-119: c (PR #3) must also be retargeted; its base (b) is deleted on merge.
    let patch3 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/3"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    let merge1 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    let merge2 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "b"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":1"#) && s.contains(r#""number":2"#), "got: {s}");
    merge1.assert();
    merge2.assert();
    patch2.assert();
    patch3.assert();
}

// STA-125: retarget must cover non-chain children of intermediate chain branches
// (the fork case). Stack: main -> a -> b -> {c, d} where user is on d;
// chain = [a, b, d].  c is a non-chain child of the intermediate branch b.
// When b is deleted on merge, c's PR base would be orphaned unless c is
// also retargeted.  The STA-119 fix only covered children of chain.last();
// this test asserts that children of intermediate chain members are covered too.
#[test]
fn merge_retargets_off_chain_child_of_intermediate_chain_branch() {
    let tmp = repo();
    let p = tmp.path();
    // main -> a -> b -> c  (fork 1: off-chain, NOT merged)
    //              b -> d  (fork 2: user on d; chain = [a, b, d])
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "c"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "c"]);
    run_git(p, &["checkout", "-q", "b"]);
    run_git(p, &["checkout", "-q", "-b", "d"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "d"]);
    track_pr(p, "a", "main", 1);
    track_pr(p, "b", "a", 2);
    track_pr(p, "c", "b", 3); // off-chain child of intermediate branch b
    track_pr(p, "d", "b", 4); // user is here; chain = [a, b, d]

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    // c's PR is checked during retarget (open-state guard before PATCH).
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/4"));
        t.status(200).json_body(pr_open(4, "clean"));
    });
    // b (PR #2) retargeted: non-bottom in-chain member.
    let patch2 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/2"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    // STA-125: c (PR #3) must be retargeted even though it is a child of the
    // *intermediate* chain branch b, not of chain.last() (d).
    let patch3 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/3"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    // d (PR #4) retargeted: non-bottom in-chain member.
    let patch4 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/4"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(4, "clean"));
    });
    let merge1 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    let merge2 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    let merge4 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/4/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "d"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains(r#""number":1"#) && s.contains(r#""number":2"#) && s.contains(r#""number":4"#),
        "expected PRs 1, 2, 4 in merged output; got: {s}"
    );
    merge1.assert();
    merge2.assert();
    merge4.assert();
    patch2.assert();
    patch4.assert();
    patch3.assert(); // the key STA-125 assertion: off-chain child of intermediate branch
}

// STA-118: a PR blocked only because its CI is still running is a *retryable*
// stop. The stop JSON must say so (`rejection: checks_pending`, `retryable:
// true`) so an agent (or `merge --watch`) polls and retries instead of treating
// it as a hard block.
#[test]
fn merge_marks_a_ci_pending_stop_retryable() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", "main", 1);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    // The PR is blocked, and its CI rollup is still pending: a retryable stop.
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "blocked"));
    });
    let checks = server.mock(|w, t| {
        w.method(Method::POST).path("/graphql");
        t.status(200).json_body(serde_json::json!({
            "data": { "repository": { "pr1": {
                "reviewDecision": serde_json::Value::Null,
                "commits": { "nodes": [ { "commit": {
                    "statusCheckRollup": { "state": "PENDING" }
                } } ] }
            } } }
        }));
    });

    run_git(p, &["checkout", "-q", "feat"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""kind":"not_ready""#), "got: {s}");
    assert!(s.contains(r#""rejection":"checks_pending""#), "got: {s}");
    assert!(s.contains(r#""retryable":true"#), "got: {s}");
    checks.assert();
}

// STA-118: `--watch` waits on a pending-CI stop, but gives up at the timeout and
// reports the same retryable stop rather than hanging forever.
#[test]
fn merge_watch_times_out_and_reports_the_retryable_stop() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", "main", 1);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "blocked"));
    });
    server.mock(|w, t| {
        w.method(Method::POST).path("/graphql");
        t.status(200).json_body(serde_json::json!({
            "data": { "repository": { "pr1": {
                "reviewDecision": serde_json::Value::Null,
                "commits": { "nodes": [ { "commit": {
                    "statusCheckRollup": { "state": "PENDING" }
                } } ] }
            } } }
        }));
    });

    run_git(p, &["checkout", "-q", "feat"]);
    let out = stacc_env(
        p,
        &[
            "merge", "--offline", "--watch", "--watch-timeout", "1", "--watch-interval", "1",
            "--json",
        ],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""rejection":"checks_pending""#), "got: {s}");
    assert!(s.contains(r#""retryable":true"#), "got: {s}");
    assert!(s.contains(r#""watch_outcome":"timed_out""#), "got: {s}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("waiting on CI"), "expected the watch note, got: {err}");
}

// STA-118: `--watch` only engages for pending CI. A hard block (failed checks)
// stops immediately, never entering the wait loop.
#[test]
fn merge_watch_does_not_wait_on_a_hard_block() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", "main", 1);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "blocked"));
    });
    server.mock(|w, t| {
        w.method(Method::POST).path("/graphql");
        t.status(200).json_body(serde_json::json!({
            "data": { "repository": { "pr1": {
                "reviewDecision": serde_json::Value::Null,
                "commits": { "nodes": [ { "commit": {
                    "statusCheckRollup": { "state": "FAILURE" }
                } } ] }
            } } }
        }));
    });

    run_git(p, &["checkout", "-q", "feat"]);
    let out = stacc_env(
        p,
        &[
            "merge", "--offline", "--watch", "--watch-timeout", "60", "--watch-interval", "1",
            "--json",
        ],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""rejection":"blocked""#), "got: {s}");
    assert!(s.contains(r#""retryable":false"#), "got: {s}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("waiting on CI"),
        "watch must not engage on a hard block, got: {err}"
    );
}

#[test]
fn merge_with_nothing_ready_is_a_noop() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", "main", 1);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "blocked"));
    });

    run_git(p, &["checkout", "-q", "feat"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""merged":[]"#), "got: {s}");
    assert!(s.contains(r#""readiness":"blocked""#), "got: {s}");
    // Nothing dropped: feat is still tracked.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(log.contains(r#""name":"feat""#), "got: {log}");
}

#[test]
fn merge_reconciles_the_prefix_when_a_later_pr_is_no_longer_mergeable() {
    let tmp = repo();
    let p = tmp.path();
    // main -> bottom -> top.
    run_git(p, &["checkout", "-q", "-b", "bottom"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "top"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "t"]);
    track_pr(p, "bottom", "main", 1);
    track_pr(p, "top", "bottom", 2);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    let merge1 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    // bottom merges, but top's merge 409s (head moved) -> NotMergeable -> stop.
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(409).json_body(serde_json::json!({ "message": "head moved" }));
    });

    run_git(p, &["checkout", "-q", "top"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    // NotMergeable mid-loop is a clean stop, not a hard error.
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":1"#), "bottom should have merged: {s}");
    assert!(s.contains("no longer mergeable"), "stopped reason: {s}");
    merge1.assert();
    // The merged prefix WAS reconciled: bottom is dropped, top remains.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"bottom""#),
        "merged bottom not reconciled: {log}"
    );
    assert!(log.contains(r#""name":"top""#), "got: {log}");
}

#[test]
fn merge_with_a_protected_trunk_emits_no_warning() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", "main", 1);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(200).json_body(serde_json::json!({ "required_status_checks": {} }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "blocked")); // nothing ready, keep it simple
    });

    run_git(p, &["checkout", "-q", "feat"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""trunk_protected":true"#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("has no branch protection"),
        "unexpected protection warning: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn merge_skips_a_pr_already_merged_out_of_band_and_surfaces_the_sha() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "bottom"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "top"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "t"]);
    track_pr(p, "bottom", "main", 1);
    track_pr(p, "top", "bottom", 2);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    // bottom (#1) is already merged out of band: detected, counted, not re-merged
    // (there is no PUT /pulls/1/merge mock, so a re-merge attempt would error).
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(serde_json::json!({
            "number": 1, "html_url": "u", "state": "closed", "merged": true,
        }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    let merge2 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true, "sha": "deadbeef" }));
    });

    run_git(p, &["checkout", "-q", "top"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    // Does not stall or error on the out-of-band-merged bottom.
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":1"#) && s.contains(r#""number":2"#), "got: {s}");
    assert!(s.contains(r#""sha":"deadbeef""#), "merge sha not surfaced: {s}");
    // bottom was merged out of band, top by us: both flags present.
    assert!(s.contains(r#""out_of_band":true"#), "got: {s}");
    assert!(s.contains(r#""out_of_band":false"#), "got: {s}");
    merge2.assert(); // top WAS merged by us
    // Both branches dropped from state by the reconcile.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"bottom""#) && !log.contains(r#""name":"top""#),
        "got: {log}"
    );
}

#[test]
fn merge_continues_past_a_mid_chain_out_of_band_merge() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "bottom"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "middle"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "m"]);
    run_git(p, &["checkout", "-q", "-b", "top"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "t"]);
    track_pr(p, "bottom", "main", 1);
    track_pr(p, "middle", "bottom", 2);
    track_pr(p, "top", "middle", 3);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    // middle (#2) is merged out of band: counted, not re-pointed, not re-merged
    // (no PATCH /pulls/2 or PUT /pulls/2/merge mock).
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(serde_json::json!({
            "number": 2, "html_url": "u", "state": "closed", "merged": true,
        }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    let merge1 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true, "sha": "aaa" }));
    });
    let merge3 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/3/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true, "sha": "ccc" }));
    });

    run_git(p, &["checkout", "-q", "top"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // All three counted: we merged 1 and 3, middle (2) was already merged.
    assert!(
        s.contains(r#""number":1"#) && s.contains(r#""number":2"#) && s.contains(r#""number":3"#),
        "got: {s}"
    );
    merge1.assert();
    merge3.assert();
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"bottom""#)
            && !log.contains(r#""name":"middle""#)
            && !log.contains(r#""name":"top""#),
        "got: {log}"
    );
}

#[test]
fn merge_require_protected_refuses_an_unprotected_trunk() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", "main", 1);

    let server = MockServer::start();
    server.mock(|w, t| {
        w.method(Method::GET)
            .path("/repos/stacc-sandbox/example/branches/main/protection");
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });

    run_git(p, &["checkout", "-q", "feat"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--require-protected", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("has no branch protection"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn merge_on_the_trunk_errors() {
    let tmp = repo();
    let p = tmp.path();
    let out = stacc(p, &["merge", "--json"]); // on main
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cannot merge the trunk"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn merge_online_restacks_and_force_pushes_each_child() {
    let (tmp, bare) = online_repo();
    let p = tmp.path();
    // main -> a (a.txt) -> b (b.txt): independent changes, both pushed to origin.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    commit_file(p, "a.txt", "a\n", "a1");
    run_git(p, &["push", "-q", "origin", "a"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    commit_file(p, "b.txt", "b\n", "b1");
    run_git(p, &["push", "-q", "origin", "b"]);
    track_pr(p, "a", "main", 1);
    track_pr(p, "b", "a", 2);

    // Simulate a's squash landing on the remote trunk, so the restack of b has a
    // moved trunk to rebase onto (which is what drops a's commit from b).
    run_git(p, &["checkout", "-q", "main"]);
    commit_file(p, "a-merged.txt", "merged\n", "squash: a #1");
    run_git(p, &["push", "-q", "origin", "main"]);
    run_git(p, &["checkout", "-q", "b"]);

    let server = MockServer::start();
    let api = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{api}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "not protected" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{api}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{api}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    // b (#2) is retargeted to main up front.
    let patch2 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{api}/pulls/2"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    let merge1 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{api}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    let merge2 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{api}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "b"]);
    let out = stacc_env(
        p,
        &["merge", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":1"#) && s.contains(r#""number":2"#), "both merged: {s}");
    merge1.assert();
    merge2.assert();
    patch2.assert();

    // b was restacked onto main (drop a's commit) and force-pushed: the branch on
    // the remote is `main + b1`, with no `a1`. Match subjects only (`%s`): an
    // abbreviated hash can contain `a1` by luck, which `--oneline` would trip on.
    let bare_b = git_out(bare.path(), &["log", "--format=%s", "b"]);
    assert!(bare_b.contains("b1"), "b kept its own commit: {bare_b}");
    assert!(!bare_b.contains("a1"), "b dropped a's commit via the restack: {bare_b}");

    // Both branches dropped from state.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"a""#) && !log.contains(r#""name":"b""#),
        "both dropped: {log}"
    );
}

#[test]
fn merge_online_force_pushes_the_correct_branch_at_each_level() {
    let (tmp, bare) = online_repo();
    let p = tmp.path();
    // main -> a -> b -> c, each touching its own file; all pushed.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    commit_file(p, "a.txt", "a\n", "a1");
    run_git(p, &["push", "-q", "origin", "a"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    commit_file(p, "b.txt", "b\n", "b1");
    run_git(p, &["push", "-q", "origin", "b"]);
    run_git(p, &["checkout", "-q", "-b", "c"]);
    commit_file(p, "c.txt", "c\n", "c1");
    run_git(p, &["push", "-q", "origin", "c"]);
    track_pr(p, "a", "main", 1);
    track_pr(p, "b", "a", 2);
    track_pr(p, "c", "b", 3);

    // Simulate a's squash landing on the remote trunk so the restacks drop a1.
    run_git(p, &["checkout", "-q", "main"]);
    commit_file(p, "a-merged.txt", "merged\n", "squash: a #1");
    run_git(p, &["push", "-q", "origin", "main"]);
    run_git(p, &["checkout", "-q", "c"]);

    let server = MockServer::start();
    let api = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{api}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    for n in 1..=3 {
        server.mock(move |w, t| {
            w.method(Method::GET).path(format!("{api}/pulls/{n}"));
            t.status(200).json_body(pr_open(n, "clean"));
        });
    }
    let patch2 = server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{api}/pulls/2")).json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    let patch3 = server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{api}/pulls/3")).json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    let merges: Vec<_> = (1..=3)
        .map(|n| {
            server.mock(move |w, t| {
                w.method(Method::PUT).path(format!("{api}/pulls/{n}/merge"));
                t.status(200).json_body(serde_json::json!({ "merged": true }));
            })
        })
        .collect();

    let out = stacc_env(
        p,
        &["merge", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stdout: {}\nstderr: {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":1"#) && s.contains(r#""number":2"#) && s.contains(r#""number":3"#), "all merged: {s}");
    for m in &merges {
        m.assert();
    }
    patch2.assert();
    patch3.assert();

    // Each child was force-pushed restacked onto the merged trunk, with a1
    // dropped. If a level force-pushed the wrong branch, a1 would survive here.
    // Match subjects only (`%s`): an abbreviated hash can contain `a1` by luck.
    let bare_b = git_out(bare.path(), &["log", "--format=%s", "b"]);
    assert!(bare_b.contains("b1") && !bare_b.contains("a1"), "b: {bare_b}");
    let bare_c = git_out(bare.path(), &["log", "--format=%s", "c"]);
    assert!(bare_c.contains("c1") && !bare_c.contains("a1"), "c: {bare_c}");

    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"a""#) && !log.contains(r#""name":"b""#) && !log.contains(r#""name":"c""#),
        "all dropped: {log}"
    );
}

#[test]
fn merge_restores_the_starting_branch() {
    let tmp = repo();
    let p = tmp.path();
    // main -> a -> b -> c. Merge from `b` (a middle branch): the upstack `c` gets
    // restacked, which leaves HEAD on `c` unless merge restores the start branch.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    run_git(p, &["checkout", "-q", "-b", "c"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    track_pr(p, "a", "main", 1);
    track_pr(p, "b", "a", 2);
    track_pr(p, "c", "b", 3);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    // c's PR (#3) is checked + retargeted: it is the upstack child of chain-top b.
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "b"]);
    // --keep-branches so the merged starting branch's ref survives; the default
    // cleanup would delete it and land on the trunk instead (covered by
    // merge_of_the_whole_stack_ends_on_the_trunk_with_refs_gone).
    let out = stacc_env(
        p,
        &["merge", "--offline", "--keep-branches", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // merge ran from `b` and merged a + b; HEAD is restored to `b`, not left on
    // `c` (which the upstack restack would otherwise leave it on).
    assert_eq!(git_out(p, &["rev-parse", "--abbrev-ref", "HEAD"]), "b");
}

// STA-125: when the starting branch is itself merged (its local ref deleted),
// land on the first surviving direct child instead of the trunk.  This lets
// the user run `stacc submit` immediately after the merge without a manual
// `stacc checkout` first.
#[test]
fn merge_lands_on_first_child_when_starting_branch_merges() {
    let tmp = repo();
    let p = tmp.path();
    // main -> a -> b -> c.  Merge from `b`: a and b both merge, c survives.
    // Without --keep-branches, b's ref is deleted; HEAD should land on c.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    run_git(p, &["checkout", "-q", "-b", "c"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    track_pr(p, "a", "main", 1);
    track_pr(p, "b", "a", 2);
    track_pr(p, "c", "b", 3);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "b"]);
    // No --keep-branches: b's ref is deleted after merge.
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // b merged and its ref was deleted; HEAD must be c (first surviving child),
    // not main (the old trunk fallback).
    assert_eq!(git_out(p, &["rev-parse", "--abbrev-ref", "HEAD"]), "c");
}

#[test]
fn merge_deletes_merged_refs_and_keeps_the_stopped_branch() {
    let tmp = repo();
    let p = tmp.path();
    // main -> bottom -> middle -> top; bottom + middle merge, top is blocked.
    run_git(p, &["checkout", "-q", "-b", "bottom"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "middle"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "m"]);
    run_git(p, &["checkout", "-q", "-b", "top"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "t"]);
    track_pr(p, "bottom", "main", 1);
    track_pr(p, "middle", "bottom", 2);
    track_pr(p, "top", "middle", 3);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "blocked"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/3"));
        t.status(200).json_body(pr_open(3, "blocked"));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "top"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // The merged refs were deleted, in walk order; nothing was skipped.
    assert!(s.contains(r#""cleaned":["bottom","middle"]"#), "got: {s}");
    assert!(s.contains(r#""cleanup_skipped":[]"#), "got: {s}");
    assert!(!ref_exists(p, "bottom"), "bottom's ref must be deleted");
    assert!(!ref_exists(p, "middle"), "middle's ref must be deleted");
    assert!(ref_exists(p, "top"), "the stopped-at branch's ref must survive");
}

#[test]
fn merge_keep_branches_keeps_the_merged_refs() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "bottom"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "top"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "t"]);
    track_pr(p, "bottom", "main", 1);
    track_pr(p, "top", "bottom", 2);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "top"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--keep-branches", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""cleaned":[]"#), "nothing deleted: {s}");
    assert!(s.contains(r#""cleanup_skipped":[]"#), "nothing to report: {s}");
    // Both merged and dropped from state, but the local refs survive.
    assert!(ref_exists(p, "bottom"), "bottom's ref must survive --keep-branches");
    assert!(ref_exists(p, "top"), "top's ref must survive --keep-branches");
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"bottom""#) && !log.contains(r#""name":"top""#),
        "state still drops merged branches: {log}"
    );
}

#[test]
fn merge_of_the_whole_stack_ends_on_the_trunk_with_refs_gone() {
    let tmp = repo();
    let p = tmp.path();
    // main -> a -> b, merged from `b`: the starting branch itself merges, so
    // its ref is deleted and merge ends on the trunk.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    track_pr(p, "a", "main", 1);
    track_pr(p, "b", "a", 2);

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "b"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""cleaned":["a","b"]"#), "got: {s}");
    assert!(!ref_exists(p, "a") && !ref_exists(p, "b"), "merged refs must be gone");
    // The starting branch merged and its ref is gone: merge ends on the trunk.
    assert_eq!(git_out(p, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
}

/// Track `branch` with NO recorded PR (base `base`, hash from the live ref),
/// the shape adoption acts on: tracked in stacc, PR opened outside it.
fn track_no_pr(p: &Path, branch: &str, base: &str) {
    let store = StateStore::new(Git::open(p));
    let mut state = store.load().unwrap();
    state.branches.insert(
        branch.to_string(),
        BranchState {
            base: Base {
                name: base.to_string(),
                hash: git_out(p, &["rev-parse", base]),
            },
            pr: None,
            pr_title: None,
            pr_description: None,
        },
    );
    store.save(&state).unwrap();
}

/// Mock `GET /pulls?head=stacc-sandbox:{branch}&state=all` (the adoption
/// lookup) returning `body`.
fn mock_pr_by_head(server: &MockServer, branch: &str, body: serde_json::Value) {
    let head = format!("stacc-sandbox:{branch}");
    server.mock(move |w, t| {
        w.method(Method::GET)
            .path("/repos/stacc-sandbox/example/pulls")
            .query_param("head", &head)
            .query_param("state", "all");
        t.status(200).json_body(body.clone());
    });
}

#[test]
fn merge_adopts_a_gh_created_open_pr_and_merges_it() {
    let tmp = repo();
    let p = tmp.path();
    // `feature` is tracked but its PR (#5) was opened with gh: no record in state.
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_no_pr(p, "feature", "main");

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    mock_pr_by_head(
        &server,
        "feature",
        serde_json::json!([{
            "number": 5, "html_url": "https://github.com/stacc-sandbox/example/pull/5",
            "state": "open", "merged_at": null,
        }]),
    );
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/5"));
        t.status(200).json_body(pr_open(5, "clean"));
    });
    let merge5 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/5/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    // The whole point of adoption: no prior `stacc sync`/`submit` required.
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":5"#), "adopted PR merged: {s}");
    assert!(s.contains(r#""out_of_band":false"#), "merged by us, not out of band: {s}");
    assert!(s.contains(r#""cleaned":["feature"]"#), "got: {s}");
    merge5.assert();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("adopted PR #5 for `feature`"),
        "adoption surfaced: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!ref_exists(p, "feature"), "merged ref must be gone");
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(!log.contains(r#""name":"feature""#), "feature dropped from state: {log}");
}

#[test]
fn merge_aborts_when_an_adoption_lookup_fails() {
    let tmp = repo();
    let p = tmp.path();
    // `feature` is tracked with no recorded PR, so merge runs its adoption pass.
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_no_pr(p, "feature", "main");

    // The adoption lookup 500s. Merge shares sync's adoption core, so it too must
    // abort rather than silently skip the lookup.
    let server = MockServer::start();
    server.mock(|w, t| {
        w.method(Method::GET)
            .path("/repos/stacc-sandbox/example/pulls")
            .query_param("head", "stacc-sandbox:feature");
        t.status(500);
    });

    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(!out.status.success(), "a failed adoption lookup must abort merge");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""type":"unexpected""#), "github error: {s}");
}

#[test]
fn forge_error_envelope_is_neutral_schema_versioned_and_scrubbed() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_no_pr(p, "feature", "main");

    // The adoption lookup is rejected for auth, and the response body echoes a
    // secret alongside GitHub's message.
    let server = MockServer::start();
    server.mock(|w, t| {
        w.method(Method::GET)
            .path("/repos/stacc-sandbox/example/pulls")
            .query_param("head", "stacc-sandbox:feature");
        t.status(401)
            .json_body(serde_json::json!({ "message": "Bad credentials", "token": "ghp_SECRET" }));
    });

    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&s).unwrap_or_else(|_| panic!("json error: {s}"));
    // Neutral error type (no `github`/`forge` discriminator), schema-versioned.
    assert_eq!(v["type"], "forge_auth", "got: {s}");
    assert_eq!(v["schema_version"], 3, "got: {s}");
    // R18: the response body's secret never reaches the error envelope.
    assert!(!s.contains("ghp_SECRET"), "token leaked into error: {s}");
}

#[test]
fn merge_retargets_an_adopted_mid_chain_pr_to_the_trunk() {
    let tmp = repo();
    let p = tmp.path();
    // main -> bottom (recorded #1) -> top (gh-created #2, unrecorded). Adoption
    // must land BEFORE the retarget pass, so #2 is pointed at the trunk and
    // survives bottom's branch deletion on merge.
    run_git(p, &["checkout", "-q", "-b", "bottom"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b"]);
    run_git(p, &["checkout", "-q", "-b", "top"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "t"]);
    track_pr(p, "bottom", "main", 1);
    track_no_pr(p, "top", "bottom");

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    mock_pr_by_head(
        &server,
        "top",
        serde_json::json!([{
            "number": 2, "html_url": "u", "state": "open", "merged_at": null,
        }]),
    );
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/1"));
        t.status(200).json_body(pr_open(1, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/pulls/2"));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    let patch2 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/2"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    let merge1 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/1/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });
    let merge2 = server.mock(|w, t| {
        w.method(Method::PUT).path(format!("{base}/pulls/2/merge"));
        t.status(200).json_body(serde_json::json!({ "merged": true }));
    });

    run_git(p, &["checkout", "-q", "top"]);
    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":1"#) && s.contains(r#""number":2"#), "got: {s}");
    // The adopted PR went through the same up-front retarget as a recorded one.
    patch2.assert();
    merge1.assert();
    merge2.assert();
}

#[test]
fn merge_reconciles_an_adopted_out_of_band_merged_pr() {
    let tmp = repo();
    let p = tmp.path();
    // `feature`'s PR (#7) was opened AND merged outside stacc: nothing recorded.
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_no_pr(p, "feature", "main");

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    // The list endpoint reports the merge via `merged_at`. There is no
    // GET /pulls/7 mock: the walk must trust the adoption lookup, not re-query.
    mock_pr_by_head(
        &server,
        "feature",
        serde_json::json!([{
            "number": 7, "html_url": "u", "state": "closed",
            "merged_at": "2026-06-10T18:00:00Z",
        }]),
    );

    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":7"#), "got: {s}");
    assert!(s.contains(r#""out_of_band":true"#), "counted like a merged-out-of-band PR: {s}");
    assert!(s.contains(r#""cleaned":["feature"]"#), "got: {s}");
    assert!(!ref_exists(p, "feature"), "merged ref must be gone");
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    assert!(!log.contains(r#""name":"feature""#), "feature dropped from state: {log}");
}

#[test]
fn merge_stops_with_no_pr_when_github_has_none() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_no_pr(p, "feature", "main");

    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    mock_pr_by_head(&server, "feature", serde_json::json!([]));

    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""kind":"no_pr""#), "still stops with no_pr: {s}");
    assert!(s.contains(r#""merged":[]"#), "nothing merged: {s}");
    assert!(ref_exists(p, "feature"), "branch kept");
    let state = StateStore::new(Git::open(p)).load().unwrap();
    assert!(state.branches["feature"].pr.is_none(), "state untouched");
}

#[test]
fn merge_does_not_adopt_a_closed_unmerged_pr() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_no_pr(p, "feature", "main");

    // Closed without merging: submit should open a fresh PR for this head
    // later, so merge must not resurrect the closed one into state (sync
    // parity). No merge mock exists, so adopting it would error loudly.
    let server = MockServer::start();
    let base = "/repos/stacc-sandbox/example";
    server.mock(|w, t| {
        w.method(Method::GET).path(format!("{base}/branches/main/protection"));
        t.status(404).json_body(serde_json::json!({ "message": "x" }));
    });
    mock_pr_by_head(
        &server,
        "feature",
        serde_json::json!([{
            "number": 9, "html_url": "u", "state": "closed", "merged_at": null,
        }]),
    );

    let out = stacc_env(
        p,
        &["merge", "--offline", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""kind":"no_pr""#), "closed PR reads as no PR: {s}");
    assert!(s.contains(r#""merged":[]"#), "nothing merged: {s}");
    assert!(ref_exists(p, "feature"), "branch kept");
    let state = StateStore::new(Git::open(p)).load().unwrap();
    assert!(state.branches["feature"].pr.is_none(), "no PR recorded for a closed PR");
}

// F3: the GitHub-only boundary. In forge-less or local mode, `merge` is
// unavailable with a forge-generic message, a non-zero exit, no crash, and never
// a raw remote URL (R9/R10). Asserted on `--format json` so the single-line,
// unwrapped message is matched verbatim.

#[test]
fn merge_on_a_non_github_remote_is_unavailable_with_a_forge_generic_message() {
    let tmp = repo();
    let p = tmp.path();
    // Repoint origin at a non-GitHub forge. The boundary refuses before any
    // network, so the dead URL is never contacted.
    run_git(p, &["remote", "set-url", "origin", "https://gitlab.com/acme/widgets.git"]);
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    assert!(stacc(p, &["track", "--base", "main"]).status.success());

    let out = stacc(p, &["merge", "--json"]);
    assert!(!out.status.success(), "merge on a non-GitHub remote is unavailable");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""type":"usage""#), "usage error, not a crash: {s}");
    assert!(s.contains("open a change through your forge"), "forge-generic guidance: {s}");
    assert!(s.contains("origin"), "names the remote: {s}");
    assert!(!s.contains("gitlab.com"), "no raw remote URL: {s}");
    assert!(!s.contains("widgets"), "no raw remote URL: {s}");
}

#[test]
fn merge_in_local_mode_is_unavailable_even_on_a_github_remote() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    assert!(stacc(p, &["track", "--base", "main"]).status.success());
    // Opting into local mode makes merge unavailable even though origin is a
    // github.com URL.
    assert!(stacc(p, &["config", "set", "local", "true"]).status.success());

    let out = stacc(p, &["merge", "--json"]);
    assert!(!out.status.success(), "local mode makes merge unavailable");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("local mode is on"), "names local mode: {s}");
    assert!(s.contains("open a change through your forge"), "forge-generic guidance: {s}");
}
