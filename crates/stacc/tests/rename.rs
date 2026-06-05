use std::path::Path;
use std::process::{Command, Output};

use httpmock::MockServer;
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

fn current_branch(dir: &Path) -> String {
    git_out(dir, &["symbolic-ref", "--short", "HEAD"])
}

/// A repo whose `origin` fetch URL is a GitHub URL (so owner/repo parse), but
/// whose *push* URL points at a local bare repo (so `git push` works offline).
fn setup() -> (TempDir, TempDir) {
    let bare = TempDir::new().expect("bare dir");
    let status = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(bare.path())
        .status()
        .expect("init bare");
    assert!(status.success());

    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://github.com/TinyDogTech/stacc.git"],
    );
    run_git(
        tmp.path(),
        &["remote", "set-url", "--push", "origin", bare.path().to_str().unwrap()],
    );
    assert!(stacc(tmp.path(), &["init"]).status.success());
    (tmp, bare)
}

fn pr_body(number: u64) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "html_url": format!("https://github.com/TinyDogTech/stacc/pull/{number}"),
        "state": "open",
        "merged": false,
    })
}

#[test]
fn rename_moves_the_state_key_and_repoints_children() {
    let (tmp, _bare) = setup();
    let p = tmp.path();
    // main -> a -> b
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["rename", "x", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""op":"rename""#), "got: {s}");
    assert!(s.contains(r#""from":"a""#) && s.contains(r#""to":"x""#), "got: {s}");
    assert!(s.contains(r#""remote_renamed":false"#), "got: {s}");

    // HEAD followed the rename; a is gone, x is tracked, and b re-parented onto x.
    assert_eq!(current_branch(p), "x");
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(log.contains(r#""name":"x""#), "got: {log}");
    assert!(log.contains(r#""base":"x""#), "b not re-pointed: {log}");
    assert!(!log.contains(r#""name":"a""#), "a still present: {log}");
}

#[test]
fn rename_to_an_existing_name_errors() {
    let (tmp, _bare) = setup();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["rename", "b", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("already tracked"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn rename_the_trunk_errors() {
    let (tmp, _bare) = setup();
    let p = tmp.path();
    let out = stacc(p, &["rename", "x", "--format", "json"]); // on main
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cannot rename the trunk"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn rename_with_an_open_pr_requires_force() {
    let (tmp, _bare) = setup();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());

    let server = MockServer::start();
    let _pulls = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(7));
    });
    assert!(stacc_env(
        p,
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    )
    .status
    .success());

    // a now has an open PR; renaming without --force is refused, naming the PR.
    let out = stacc(p, &["rename", "x", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("will close its open PR #7"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn rename_with_force_drops_the_pr_and_renames_the_remote() {
    let (tmp, _bare) = setup();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());

    let server = MockServer::start();
    let _pulls = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(7));
    });
    let base = server.base_url();
    let env = [("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", base.as_str())];
    assert!(stacc_env(p, &["submit", "--format", "json"], &env).status.success());

    // The remote branch-rename API is called with the new name.
    let rename_mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/branches/a/rename");
        then.status(201).json_body(serde_json::json!({ "name": "x" }));
    });

    let out = stacc_env(p, &["rename", "x", "--force", "--format", "json"], &env);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""op":"rename""#), "got: {s}");
    assert!(s.contains(r#""remote_renamed":true"#), "got: {s}");
    assert!(s.contains(r#""number":7"#), "closing PR not surfaced: {s}");
    rename_mock.assert();

    // x is tracked with no recorded PR (the field is omitted when None), so the
    // next submit recreates it.
    let show = git_out(p, &["show", "refs/stacc/data:branches/x"]);
    assert!(!show.contains(r#""pr""#), "pr not dropped: {show}");
}
