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
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn stacc")
}

fn stacc(dir: &std::path::Path, args: &[&str]) -> Output {
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
fn sync_requires_init() {
    let tmp = repo();
    let out = stacc(tmp.path(), &["sync", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not initialized"), "got: {s}");
}

#[test]
fn sync_errors_when_remote_is_unreachable_without_offline() {
    // The repo()'s `origin` points at a sandbox URL that 404s — without
    // --offline, sync's fetch must surface that as a hard error (and the
    // stderr hint tells the user to retry with --offline).
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let out = stacc(tmp.path(), &["sync", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""error":"git""#), "got: {s}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--offline"), "stderr: {err}");
}

#[test]
fn sync_detects_merged_and_reparents_children() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());

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
        &["sync", "--offline", "--format", "json"],
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

    // Poison the recorded base hash with the zero-OID — not a valid commit at
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
