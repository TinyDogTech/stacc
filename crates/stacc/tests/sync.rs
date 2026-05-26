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

fn repo() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://github.com/TinyDogTech/stacc.git"],
    );
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

    let out = stacc(tmp.path(), &["sync", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""merged":[]"#), "got: {s}");
    assert!(s.contains(r#""reparented":[]"#), "got: {s}");
}

#[test]
fn sync_detects_merged_and_reparents_children() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    // Seed a two-deep stack, both submitted: feature-1 (PR 1) <- feature-2 (PR 2).
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
            .path("/repos/TinyDogTech/stacc/pulls/1");
        then.status(200).json_body(serde_json::json!({
            "number": 1, "html_url": "u", "state": "closed", "merged": true,
        }));
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls/2");
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

    // feature-1 is gone from state; feature-2 is now based on main.
    assert!(!show(tmp.path(), "refs/stacc/data:branches/feature-1").status.success());
    let f2 = show(tmp.path(), "refs/stacc/data:branches/feature-2");
    assert!(String::from_utf8_lossy(&f2.stdout).contains(r#""name": "main""#));
}

#[test]
fn sync_requires_init() {
    let tmp = repo();
    let out = stacc(tmp.path(), &["sync", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not initialized"), "got: {s}");
}
