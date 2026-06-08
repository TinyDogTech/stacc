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
    // Base re-points for middle and top (after the prior merge).
    let patch2 = server.mock(|w, t| {
        w.method(Method::PATCH)
            .path(format!("{base}/pulls/2"))
            .json_body(serde_json::json!({ "base": "main" }));
        t.status(200).json_body(pr_open(2, "clean"));
    });
    server.mock(|w, t| {
        w.method(Method::PATCH).path(format!("{base}/pulls/3"));
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
        &["merge", "--offline", "--format", "json"],
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
    assert!(s.contains(r#""mergeable_state":"blocked""#), "got: {s}");
    assert!(s.contains(r#""trunk_protected":false"#), "got: {s}");
    merge1.assert();
    merge2.assert();
    patch2.assert(); // middle's base was re-pointed to trunk after bottom merged
    // The no-protection warning is surfaced.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("has no branch protection"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // bottom + middle dropped from state; top remains.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"bottom""#) && !log.contains(r#""name":"middle""#),
        "merged branches not dropped: {log}"
    );
    assert!(log.contains(r#""name":"top""#), "got: {log}");
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
        &["merge", "--offline", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""merged":[]"#), "got: {s}");
    assert!(s.contains(r#""mergeable_state":"blocked""#), "got: {s}");
    // Nothing dropped: feat is still tracked.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
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
        &["merge", "--offline", "--format", "json"],
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
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
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
        &["merge", "--offline", "--format", "json"],
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
        &["merge", "--offline", "--format", "json"],
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
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
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
        &["merge", "--offline", "--format", "json"],
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
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
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
        &["merge", "--offline", "--require-protected", "--format", "json"],
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
    let out = stacc(p, &["merge", "--format", "json"]); // on main
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
        &["merge", "--format", "json"],
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
    // the remote is `main + b1`, with no `a1`.
    let bare_b = git_out(bare.path(), &["log", "--oneline", "b"]);
    assert!(bare_b.contains("b1"), "b kept its own commit: {bare_b}");
    assert!(!bare_b.contains("a1"), "b dropped a's commit via the restack: {bare_b}");

    // Both branches dropped from state.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(
        !log.contains(r#""name":"a""#) && !log.contains(r#""name":"b""#),
        "both dropped: {log}"
    );
}
