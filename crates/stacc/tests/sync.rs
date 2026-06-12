use std::process::{Command, Output};

use httpmock::MockServer;
use stacc_git::Git;
use stacc_state::{Base, BranchState, PullRequest, StateStore};
use tempfile::TempDir;

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_ok(dir: &std::path::Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git")
        .success()
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ref_exists(dir: &std::path::Path, branch: &str) -> bool {
    // Capture the output: `--verify` prints the hash on success, which would
    // otherwise leak into the test output via the inherited stdout.
    !git_out(
        dir,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )
    .is_empty()
}

/// Inject a recorded PR for `branch` (base `base`, hash from the live ref).
fn track_pr(p: &std::path::Path, branch: &str, base: &str, number: u64) {
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

/// Mock `GET /pulls/{number}` returning a merged (or open) PR.
fn mock_pr_state(server: &MockServer, number: u64, merged: bool) {
    let state = if merged { "closed" } else { "open" };
    server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/repos/stacc-sandbox/example/pulls/{number}"));
        then.status(200).json_body(serde_json::json!({
            "number": number, "html_url": "u", "state": state, "merged": merged,
        }));
    });
}

fn write(dir: &std::path::Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn commit_file(dir: &std::path::Path, name: &str, contents: &str, message: &str) {
    write(dir, name, contents);
    run_git(dir, &["add", name]);
    run_git(dir, &["commit", "-q", "-m", message]);
}

fn stacc_env(dir: &std::path::Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stacc"));
    cmd.current_dir(dir).args(args);
    // Hermetic credentials: never let a test reach the developer's ambient
    // GitHub token, OS keychain entry, or `gh` login. Per-call `envs` below
    // re-add a token (or a mock API URL) for the tests that need one.
    cmd.env_remove("GITHUB_TOKEN");
    cmd.env_remove("GH_TOKEN");
    cmd.env("STACC_GH_BIN", ""); // disable the gh auth token fallback
    cmd.env("STACC_KEYCHAIN", ""); // disable the OS keychain source
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn stacc")
}

fn stacc(dir: &std::path::Path, args: &[&str]) -> Output {
    // Hermetic by default: a token via env (so `from_env` never falls back to
    // the OS keychain) and a dead API URL (so PR adoption and merge detection
    // fail fast locally instead of reaching the real GitHub). Tests that mock
    // the API use `stacc_env` with the mock server's URL instead.
    stacc_env(
        dir,
        args,
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", "http://127.0.0.1:1")],
    )
}

/// Like `stacc` but with no GitHub token at all (the harness already disables
/// the keychain and gh fallback), exercising the missing-credential paths.
fn stacc_no_token(dir: &std::path::Path, args: &[&str]) -> Output {
    stacc_env(dir, args, &[])
}

// A nonexistent GitHub URL: parses to an owner/repo, but `git fetch`/`push`
// fail fast (GIT_TERMINAL_PROMPT=0), exercising sync's best-effort network path.
const ORIGIN: &str = "https://github.com/stacc-sandbox/example.git";

fn repo() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(tmp.path(), &["remote", "add", "origin", ORIGIN]);
    tmp
}

/// A working repo whose `origin` is a GitHub-shaped URL (so `parse_remote`
/// works) but is rewritten via `insteadOf` to a local bare repo, so an online
/// sync's trunk fetch and state push actually succeed. Returns (work tree, bare
/// remote); keep the bare handle bound for the test's duration. Already
/// `stacc init`ed, like `repo()` callers expect after their own init.
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

fn show(dir: &std::path::Path, spec: &str) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["show", spec])
        .output()
        .expect("git show")
}

#[test]
fn sync_is_noop_without_prs() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let out = stacc(tmp.path(), &["sync", "--offline", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""merged":[]"#), "got: {s}");
    assert!(s.contains(r#""reparented":[]"#), "got: {s}");
    assert!(s.contains(r#""restacked":[]"#), "got: {s}");
}

#[test]
fn sync_prunes_a_branch_whose_git_ref_is_gone() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "ghost"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "g1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "child"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    assert!(stacc(p, &["track", "--base", "ghost"]).status.success());
    // Delete ghost's git ref: tracked, no ref, no PR.
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["branch", "-D", "ghost"]);

    let out = stacc(p, &["sync", "--offline", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains(r#""pruned":["ghost"]"#), "pruned reported: {s}");

    // ghost is gone from state; child reparented off it onto main.
    let j = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(!j.contains(r#""name":"ghost""#), "ghost dropped: {j}");
    assert!(j.contains(r#""name":"child""#), "child kept: {j}");
    assert!(!j.contains(r#""base":"ghost""#), "child no longer bases on ghost: {j}");
}

#[test]
fn sync_no_prune_keeps_a_branch_whose_git_ref_is_gone() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "ghost"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "g1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["branch", "-D", "ghost"]);

    let out = stacc(p, &["sync", "--offline", "--no-prune", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains(r#""pruned":[]"#), "nothing pruned under --no-prune: {s}");

    let j = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(j.contains(r#""name":"ghost""#), "ghost kept under --no-prune: {j}");
}

#[test]
fn sync_keeps_a_missing_ref_branch_with_an_open_pr() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    // Seed a PR, then delete the git ref: a missing ref but a recorded PR.
    let store = StateStore::new(Git::open(p));
    let mut state = store.load().unwrap();
    state.branches.insert(
        "feature".to_string(),
        BranchState {
            base: Base { name: "main".into(), hash: "h".into() },
            pr: Some(PullRequest { number: 9, url: None }),
        },
    );
    store.save(&state).unwrap();
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["branch", "-D", "feature"]);

    // GitHub: PR 9 is still open, so the branch must not be dropped.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/stacc-sandbox/example/pulls/9");
        then.status(200).json_body(serde_json::json!({
            "number": 9, "html_url": "u", "state": "open", "merged": false,
        }));
    });

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains(r#""pruned":[]"#), "open-PR branch must not be pruned: {s}");
    assert!(s.contains(r#""merged":[]"#), "open PR is not merged: {s}");

    let j = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(j.contains(r#""name":"feature""#), "open-PR branch kept: {j}");
}

#[test]
fn sync_prunes_multiple_missing_ref_branches() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    for b in ["ghost-a", "ghost-b"] {
        run_git(p, &["checkout", "-q", "main"]);
        run_git(p, &["checkout", "-q", "-b", b]);
        run_git(p, &["commit", "-q", "--allow-empty", "-m", b]);
        assert!(stacc(p, &["track"]).status.success());
        run_git(p, &["checkout", "-q", "main"]);
        run_git(p, &["branch", "-D", b]);
    }

    let out = stacc(p, &["sync", "--offline", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    // Both pruned, in sorted (BTreeSet) order.
    assert!(s.contains(r#""pruned":["ghost-a","ghost-b"]"#), "both pruned, sorted: {s}");

    let j = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(!j.contains("ghost-a") && !j.contains("ghost-b"), "both dropped: {j}");
}

#[test]
fn sync_reports_a_merged_and_gone_branch_as_merged_not_pruned() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    // Seed a PR, then delete the git ref: a merged PR whose branch ref is gone.
    let store = StateStore::new(Git::open(p));
    let mut state = store.load().unwrap();
    state.branches.insert(
        "feature".to_string(),
        BranchState {
            base: Base { name: "main".into(), hash: "h".into() },
            pr: Some(PullRequest { number: 3, url: None }),
        },
    );
    store.save(&state).unwrap();
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["branch", "-D", "feature"]);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/stacc-sandbox/example/pulls/3");
        then.status(200).json_body(serde_json::json!({
            "number": 3, "html_url": "u", "state": "closed", "merged": true,
        }));
    });

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    // Reported as merged (via PR detection), not double-counted as pruned.
    assert!(s.contains(r#""merged":["feature"]"#), "reported merged: {s}");
    assert!(s.contains(r#""pruned":[]"#), "not pruned: {s}");
}

#[test]
fn sync_pretty_reports_pruned() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "ghost"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "g1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["branch", "-D", "ghost"]);

    let out = stacc(p, &["sync", "--offline"]); // pretty output
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains("Pruned (no git ref): ghost"), "pretty prune line: {s}");
}

#[test]
fn sync_requires_init() {
    let tmp = repo();
    let out = stacc(tmp.path(), &["sync", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not initialized"), "got: {s}");
}

#[test]
fn sync_unreachable_forge_falls_back_to_forge_less_with_a_note() {
    // A reachable git remote (via insteadOf) but an unreachable GitHub API (the
    // dead API URL the hermetic `stacc` helper injects, a Transport error). Rather
    // than hard-erroring, sync drops to the forge-less floor: it fetches trunk and
    // restacks, emitting a loud note that merged-PR detection was skipped. KTD-6
    // preserves STA-94's loud-beats-silent contract via the note, not a refusal.
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc(p, &["sync", "--format", "json"]);
    assert!(
        out.status.success(),
        "an unreachable API falls back to forge-less, it does not error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no reachable forge"), "emits the skipped-detection note: {err}");
}

#[test]
fn sync_detects_merged_and_reparents_children() {
    let (tmp, _bare) = online_repo();

    // Real branches: feature-1 off main, feature-2 off feature-1.
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-1"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-2"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f2"]);

    // Seed both as submitted (PRs 1 and 2).
    let store = StateStore::new(Git::open(tmp.path()));
    let mut state = store.load().unwrap();
    state.branches.insert(
        "feature-1".to_string(),
        BranchState {
            base: Base { name: "main".into(), hash: "h1".into() },
            pr: Some(PullRequest { number: 1, url: None }),
        },
    );
    state.branches.insert(
        "feature-2".to_string(),
        BranchState {
            base: Base { name: "feature-1".into(), hash: "h2".into() },
            pr: Some(PullRequest { number: 2, url: None }),
        },
    );
    store.save(&state).unwrap();

    // GitHub: PR 1 merged, PR 2 still open.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/stacc-sandbox/example/pulls/1");
        then.status(200).json_body(serde_json::json!({
            "number": 1, "html_url": "u", "state": "closed", "merged": true,
        }));
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/stacc-sandbox/example/pulls/2");
        then.status(200).json_body(serde_json::json!({
            "number": 2, "html_url": "u", "state": "open", "merged": false,
        }));
    });

    let out = stacc_env(
        tmp.path(),
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""merged":["feature-1"]"#), "got: {s}");
    assert!(s.contains(r#""branch":"feature-2""#), "got: {s}");
    assert!(s.contains(r#""base":"main""#), "got: {s}");

    assert!(!show(tmp.path(), "refs/stacc/data:branches/feature-1").status.success());
    let f2 = show(tmp.path(), "refs/stacc/data:branches/feature-2");
    assert!(String::from_utf8_lossy(&f2.stdout).contains(r#""name": "main""#));
}

#[test]
fn sync_restacks_onto_advanced_trunk() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    commit_file(tmp.path(), "a.txt", "a\n", "feature work");
    assert!(stacc(tmp.path(), &["track"]).status.success());

    // Advance the trunk locally (a different file, so no conflict).
    run_git(tmp.path(), &["checkout", "-q", "main"]);
    commit_file(tmp.path(), "b.txt", "b\n", "trunk work");
    run_git(tmp.path(), &["checkout", "-q", "feature"]);

    let out = stacc(tmp.path(), &["sync", "--offline", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":["feature"]"#), "got: {s}");

    // feature now sits on top of the advanced trunk and carries both files.
    assert!(git_ok(tmp.path(), &["merge-base", "--is-ancestor", "main", "feature"]));
    assert!(git_ok(tmp.path(), &["cat-file", "-e", "feature:a.txt"]));
    assert!(git_ok(tmp.path(), &["cat-file", "-e", "feature:b.txt"]));
}

#[test]
fn sync_uses_fork_point_when_recorded_base_is_stale() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    commit_file(tmp.path(), "f.txt", "feature\n", "feature commit");

    // Advance the trunk so there's something to restack onto.
    run_git(tmp.path(), &["checkout", "-q", "main"]);
    commit_file(tmp.path(), "m.txt", "main\n", "trunk commit");
    run_git(tmp.path(), &["checkout", "-q", "feature"]);

    // Poison the recorded base hash with the zero-OID, not a valid commit at
    // all. Without fork-point recovery the rebase would bail with `fatal:
    // invalid upstream 0000…`; with it, sync should use the base's reflog to
    // find the real divergence point and complete.
    let store = StateStore::new(Git::open(tmp.path()));
    let mut state = store.load().unwrap();
    state.branches.insert(
        "feature".to_string(),
        BranchState {
            base: Base {
                name: "main".into(),
                hash: "0000000000000000000000000000000000000000".into(),
            },
            pr: None,
        },
    );
    store.save(&state).unwrap();

    let out = stacc(tmp.path(), &["sync", "--offline", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":["feature"]"#), "got: {s}");
    assert!(git_ok(tmp.path(), &["merge-base", "--is-ancestor", "main", "feature"]));
    assert!(git_ok(tmp.path(), &["cat-file", "-e", "feature:f.txt"]));
    assert!(git_ok(tmp.path(), &["cat-file", "-e", "feature:m.txt"]));
}

#[test]
fn sync_conflict_writes_context_then_continue_completes() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    commit_file(tmp.path(), "conflict.txt", "feature\n", "feature edit");
    assert!(stacc(tmp.path(), &["track"]).status.success());

    // Advance the trunk with a conflicting edit to the same file.
    run_git(tmp.path(), &["checkout", "-q", "main"]);
    commit_file(tmp.path(), "conflict.txt", "main\n", "trunk edit");
    run_git(tmp.path(), &["checkout", "-q", "feature"]);

    // First sync: conflict -> structured error + context + continuation files.
    let out = stacc(tmp.path(), &["sync", "--offline", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""error":"conflict""#), "got: {s}");
    assert!(s.contains(r#""branch":"feature""#), "got: {s}");

    let git_dir = tmp.path().join(".git");
    let ctx_path = git_dir.join("stacc-conflict-context.json");
    assert!(ctx_path.exists(), "context file missing");
    let ctx = std::fs::read_to_string(&ctx_path).unwrap();
    assert!(ctx.contains(r#""branch": "feature""#), "ctx: {ctx}");
    assert!(ctx.contains("conflict.txt"), "ctx: {ctx}");
    assert!(git_dir.join("stacc-continue.json").exists(), "continuation missing");

    // Resolve the conflict, then continue.
    write(tmp.path(), "conflict.txt", "resolved\n");
    run_git(tmp.path(), &["add", "conflict.txt"]);

    let out2 = stacc(tmp.path(), &["sync", "--continue", "--format", "json"]);
    assert!(out2.status.success(), "stderr: {}", String::from_utf8_lossy(&out2.stderr));
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(s2.contains(r#""restacked":["feature"]"#), "got: {s2}");

    // Artifacts cleared; feature now sits on the advanced trunk.
    assert!(!git_dir.join("stacc-continue.json").exists());
    assert!(!ctx_path.exists());
    assert!(git_ok(tmp.path(), &["merge-base", "--is-ancestor", "main", "feature"]));
}

#[test]
fn sync_deletes_the_merged_branchs_ref_and_keeps_unmerged_ones() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature-1"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_pr(p, "feature-1", "main", 1);
    run_git(p, &["checkout", "-q", "-b", "feature-2"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f2"]);
    track_pr(p, "feature-2", "feature-1", 2);
    run_git(p, &["checkout", "-q", "main"]);

    // PR 1 merged out of band; PR 2 still open.
    let server = MockServer::start();
    mock_pr_state(&server, 1, true);
    mock_pr_state(&server, 2, false);

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""merged":["feature-1"]"#), "got: {s}");
    assert!(s.contains(r#""cleaned":["feature-1"]"#), "got: {s}");
    assert!(s.contains(r#""cleanup_skipped":[]"#), "got: {s}");
    // The merged branch's local ref is gone; the open-PR branch's survives.
    assert!(!ref_exists(p, "feature-1"), "merged ref must be deleted");
    assert!(ref_exists(p, "feature-2"), "unmerged ref must survive");
}

#[test]
fn sync_keep_branches_keeps_the_merged_ref() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_pr(p, "feature", "main", 1);
    run_git(p, &["checkout", "-q", "main"]);

    let server = MockServer::start();
    mock_pr_state(&server, 1, true);

    let out = stacc_env(
        p,
        &["sync", "--keep-branches", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""merged":["feature"]"#), "still untracked from state: {s}");
    assert!(s.contains(r#""cleaned":[]"#), "nothing deleted: {s}");
    assert!(ref_exists(p, "feature"), "ref must survive --keep-branches");
}

#[test]
fn sync_keeps_a_merged_branch_checked_out_in_another_worktree() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_pr(p, "feature", "main", 1);
    run_git(p, &["checkout", "-q", "main"]);
    // Check the merged branch out in a second worktree: deleting its ref would
    // desync that worktree, so sync must keep it and say why.
    let wt = TempDir::new().expect("worktree dir");
    let wt_path = wt.path().join("wt");
    run_git(p, &["worktree", "add", "-q", wt_path.to_str().unwrap(), "feature"]);

    let server = MockServer::start();
    mock_pr_state(&server, 1, true);

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""cleaned":[]"#), "got: {s}");
    assert!(s.contains(r#""cleanup_skipped":[{"branch":"feature""#), "got: {s}");
    assert!(s.contains("checked out in"), "reason names the worktree: {s}");
    assert!(ref_exists(p, "feature"), "checked-out ref must survive");
}

#[test]
fn sync_on_the_merged_branch_ends_on_the_trunk_with_the_ref_gone() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    track_pr(p, "feature", "main", 1);
    // Stay on `feature`: sync must move to the trunk before deleting it.

    let server = MockServer::start();
    mock_pr_state(&server, 1, true);

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""cleaned":["feature"]"#), "got: {s}");
    assert_eq!(git_out(p, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert!(!ref_exists(p, "feature"), "merged ref must be gone");
}

#[test]
fn sync_offline_without_pr_detection_deletes_nothing() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f1"]);
    assert!(stacc(p, &["track"]).status.success());

    // No PRs recorded, so no merge detection: nothing to clean.
    let out = stacc(p, &["sync", "--offline", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""cleaned":[]"#), "got: {s}");
    assert!(s.contains(r#""cleanup_skipped":[]"#), "got: {s}");
    assert!(ref_exists(p, "feature"), "tracked ref must survive");
}

/// Mock `GET /pulls?head=stacc-sandbox:{branch}&state=all` (the adoption
/// lookup) returning `body`.
fn mock_pr_by_head(server: &MockServer, branch: &str, body: serde_json::Value) {
    let head = format!("stacc-sandbox:{branch}");
    server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/stacc-sandbox/example/pulls")
            .query_param("head", &head)
            .query_param("state", "all");
        then.status(200).json_body(body.clone());
    });
}

/// Track `branch` (no PR) with one committed file, ending back on main.
fn tracked_branch_without_pr(p: &std::path::Path, branch: &str) {
    run_git(p, &["checkout", "-q", "-b", branch]);
    commit_file(p, &format!("{branch}.txt"), "1", branch);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
}

#[test]
fn sync_adopts_a_merged_pr_created_outside_stacc_and_cleans_up() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    tracked_branch_without_pr(p, "feature");

    // The PR exists on GitHub (created via gh) and already merged; stacc state
    // has no record of it. The list endpoint reports the merge via `merged_at`.
    let server = MockServer::start();
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
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains(r#""merged":["feature"]"#), "adopted merged PR drops the branch: {s}");
    assert!(s.contains(r#""adopted":[]"#), "a merged PR is not reported as adopted: {s}");
    assert!(s.contains(r#""cleaned":["feature"]"#), "local ref cleaned up: {s}");
    assert!(!ref_exists(p, "feature"), "feature's git ref must be gone");

    let state = StateStore::new(Git::open(p)).load().unwrap();
    assert!(!state.branches.contains_key("feature"), "feature dropped from state");
}

#[test]
fn sync_adopts_an_open_pr_and_records_it() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    tracked_branch_without_pr(p, "feature");

    let server = MockServer::start();
    mock_pr_by_head(
        &server,
        "feature",
        serde_json::json!([{
            "number": 12, "html_url": "https://github.com/stacc-sandbox/example/pull/12",
            "state": "open", "merged_at": null,
        }]),
    );

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        s.contains(r#""adopted":[{"branch":"feature","number":12"#),
        "adoption reported: {s}"
    );
    assert!(s.contains(r#""merged":[]"#), "an open PR is not merged: {s}");
    assert!(ref_exists(p, "feature"), "open-PR branch kept");

    let state = StateStore::new(Git::open(p)).load().unwrap();
    let pr = state.branches["feature"].pr.as_ref().expect("PR recorded in state");
    assert_eq!(pr.number, 12);
}

#[test]
fn sync_pretty_reports_adoption() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    tracked_branch_without_pr(p, "feature");

    let server = MockServer::start();
    mock_pr_by_head(
        &server,
        "feature",
        serde_json::json!([{
            "number": 12, "html_url": "https://github.com/stacc-sandbox/example/pull/12",
            "state": "open", "merged_at": null,
        }]),
    );

    let out = stacc_env(
        p,
        &["sync"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        s.contains("Adopted PR #12 for feature: https://github.com/stacc-sandbox/example/pull/12"),
        "got: {s}"
    );
    assert!(!s.contains("Already up to date."), "adoption is not a no-op: {s}");
}

#[test]
fn sync_does_not_adopt_a_closed_unmerged_pr() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    tracked_branch_without_pr(p, "feature");

    // Closed without merging: submit should open a fresh PR for this head
    // later, so sync must not resurrect the closed one into state.
    let server = MockServer::start();
    mock_pr_by_head(
        &server,
        "feature",
        serde_json::json!([{
            "number": 9, "html_url": "u", "state": "closed", "merged_at": null,
        }]),
    );

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains(r#""adopted":[]"#), "closed PR not adopted: {s}");
    assert!(s.contains(r#""merged":[]"#), "closed PR not merged: {s}");
    assert!(ref_exists(p, "feature"), "branch kept");

    let state = StateStore::new(Git::open(p)).load().unwrap();
    assert!(state.branches["feature"].pr.is_none(), "no PR recorded for a closed PR");
}

#[test]
fn sync_adoption_leaves_a_branch_without_any_pr_alone() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    tracked_branch_without_pr(p, "feature");

    let server = MockServer::start();
    mock_pr_by_head(&server, "feature", serde_json::json!([]));

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains(r#""adopted":[]"#), "nothing adopted: {s}");
    assert!(ref_exists(p, "feature"), "branch kept");

    let state = StateStore::new(Git::open(p)).load().unwrap();
    assert!(state.branches["feature"].pr.is_none(), "state untouched");
}

#[test]
fn sync_without_credentials_falls_back_to_forge_less_with_a_note() {
    // A reachable git remote (via insteadOf) but no GitHub token, so `from_env`
    // returns MissingToken. Rather than hard-erroring, sync drops to the forge-less
    // floor: it fetches trunk and restacks, emitting a loud note that detection was
    // skipped (KTD-6). The missing token surfaces in the note, not as a refusal.
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    commit_file(p, "f.txt", "1", "feature");
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc_no_token(p, &["sync", "--format", "json"]);
    assert!(
        out.status.success(),
        "a missing token falls back to forge-less, it does not error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no reachable forge"), "names the skipped-detection note: {err}");
}

#[test]
fn sync_error_does_not_leak_a_credentialed_remote_url() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    // The standard CI remote shape carries a token in the URL. It fails the
    // strict remote parse and must route to the GitHub-only error WITHOUT the
    // URL appearing in the message.
    run_git(
        p,
        &["remote", "set-url", "origin", "https://x-access-token:SECRET@github.com/owner/repo.git"],
    );
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc(p, &["sync", "--format", "json"]);
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!combined.contains("SECRET"), "must not echo the credentialed URL: {combined}");
    assert!(combined.contains("origin"), "names the remote: {combined}");
}

#[test]
fn sync_offline_marks_detection_skipped() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    commit_file(p, "f.txt", "1", "feature");
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc_no_token(p, &["sync", "--offline", "--format", "json"]);
    assert!(
        out.status.success(),
        "offline succeeds without credentials: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""detection_skipped":true"#), "JSON marks the skip: {s}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("skipped merged-PR detection"), "stderr notes the skip: {err}");
}

#[test]
fn sync_empty_stack_succeeds_without_credentials() {
    let (tmp, _bare) = online_repo();
    let p = tmp.path();
    // No tracked branches: nothing needs the API, even without a token.
    let out = stacc_no_token(p, &["sync", "--format", "json"]);
    assert!(
        out.status.success(),
        "an empty sync needs no credentials: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains(r#""merged":[]"#) && s.contains(r#""detection_skipped":false"#),
        "got: {s}"
    );
}

#[test]
fn sync_aborts_when_an_adoption_lookup_fails_and_persists_partial() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    tracked_branch_without_pr(p, "feature-a");
    tracked_branch_without_pr(p, "feature-b");

    let server = MockServer::start();
    // feature-a (queried first, sorted) adopts cleanly...
    mock_pr_by_head(
        &server,
        "feature-a",
        serde_json::json!([{
            "number": 5, "html_url": "https://github.com/stacc-sandbox/example/pull/5",
            "state": "open", "merged_at": null,
        }]),
    );
    // ...feature-b's lookup 500s, which must abort the whole sync.
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/stacc-sandbox/example/pulls")
            .query_param("head", "stacc-sandbox:feature-b");
        then.status(500);
    });

    let out = stacc_env(
        p,
        &["sync", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(!out.status.success(), "a failed lookup must abort the sync");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""error":"github""#), "github error: {s}");

    // feature-a's PR, adopted before the failing lookup, is persisted.
    let state = StateStore::new(Git::open(p)).load().unwrap();
    assert_eq!(
        state.branches["feature-a"].pr.as_ref().map(|pr| pr.number),
        Some(5),
        "partial adoption persisted despite the later failure"
    );
}

#[test]
fn sync_continue_needs_no_credentials() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    commit_file(p, "conflict.txt", "feature\n", "feature edit");
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    commit_file(p, "conflict.txt", "main\n", "trunk edit");
    run_git(p, &["checkout", "-q", "feature"]);

    // Offline sync conflicts (no credentials needed to reach the restack).
    let out = stacc_no_token(p, &["sync", "--offline", "--format", "json"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#""error":"conflict""#));

    // Resolve, then continue with no token: the resume must not need credentials.
    write(p, "conflict.txt", "resolved\n");
    run_git(p, &["add", "conflict.txt"]);
    let out2 = stacc_no_token(p, &["sync", "--continue", "--format", "json"]);
    assert!(
        out2.status.success(),
        "continue resumes without credentials: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(String::from_utf8_lossy(&out2.stdout).contains(r#""restacked":["feature"]"#));
    // The offline detection-skipped note is not printed on the continue path.
    assert!(
        !String::from_utf8_lossy(&out2.stderr).contains("skipped merged-PR detection"),
        "no offline note on continue"
    );
}

/// Give `feature` a tip whose tree matches `main`'s tip but is not an ancestor
/// of it: the squash-merge shape. Leaves the checkout on `feature`.
fn squash_feature_onto_main(p: &std::path::Path) {
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["checkout", "feature", "--", "f.txt"]);
    run_git(p, &["commit", "-q", "-m", "squash of feature"]);
    run_git(p, &["checkout", "-q", "feature"]);
}

#[test]
fn sync_skips_a_tree_identical_branch_instead_of_rebasing() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    commit_file(p, "f.txt", "content\n", "feature work");
    assert!(stacc(p, &["track"]).status.success());
    let feature_tip = git_out(p, &["rev-parse", "feature"]);

    squash_feature_onto_main(p);

    let out = stacc_no_token(p, &["sync", "--offline", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("look squash-merged"), "notice surfaced: {err}");
    // The branch was skipped, not rebased: its tip is unchanged...
    assert_eq!(git_out(p, &["rev-parse", "feature"]), feature_tip, "feature not rebased");
    // ...and it is still tracked (the guard skips, it does not drop state).
    let state = StateStore::new(Git::open(p)).load().unwrap();
    assert!(state.branches.contains_key("feature"), "feature still tracked");
}

#[test]
fn sync_does_not_flag_a_branch_already_on_its_base() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    // A fresh branch on the trunk tip is tree-identical AND an ancestor of its
    // base, so the existing "already on base" skip handles it, not the guard.
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc_no_token(p, &["sync", "--offline", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("squash-merged"), "an ancestor branch must not be flagged: {err}");
}

#[test]
fn explicit_restack_does_not_apply_the_squash_merge_guard() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    commit_file(p, "f.txt", "content\n", "feature work");
    assert!(stacc(p, &["track"]).status.success());
    squash_feature_onto_main(p);

    // `stacc restack` is an explicit op: the sync-only guard must stay off, so
    // the branch is restacked normally rather than skipped as squash-merged.
    let out = stacc(p, &["restack", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("squash-merged"), "restack must not apply the sync-only guard: {err}");
}
