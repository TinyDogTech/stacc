use std::path::Path;
use std::process::{Command, Output};

use stacc_git::Git;
use stacc_state::{Base, BranchState, PullRequest, StateStore};
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

fn stacc(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
}

fn repo() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    assert!(stacc(p, &["init"]).status.success());
    tmp
}

/// Inject a recorded PR for `branch` (base main) with an optional URL.
fn track_pr(p: &Path, branch: &str, number: u64, url: Option<&str>) {
    let store = StateStore::new(Git::open(p));
    let mut state = store.load().unwrap();
    state.branches.insert(
        branch.to_string(),
        BranchState {
            base: Base {
                name: "main".into(),
                hash: git_out(p, &["rev-parse", "main"]),
            },
            pr: Some(PullRequest {
                number,
                url: url.map(String::from),
            }),
        },
    );
    store.save(&state).unwrap();
}

#[test]
fn pr_emits_the_recorded_url() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", 7, Some("https://github.com/owner/repo/pull/7"));

    let out = stacc(p, &["pr", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":7"#), "got: {s}");
    assert!(
        s.contains(r#""url":"https://github.com/owner/repo/pull/7""#),
        "got: {s}"
    );
}

#[test]
fn pr_constructs_the_url_when_none_recorded() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", 9, None); // no recorded URL

    let out = stacc(p, &["pr", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Built from the remote owner/repo + number.
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains(r#""url":"https://github.com/owner/repo/pull/9""#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn pr_pretty_prints_just_the_url() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    track_pr(p, "feat", 7, Some("https://github.com/owner/repo/pull/7"));

    // Pretty mode: stdout is piped here (not a TTY), so no browser is spawned.
    let out = stacc(p, &["pr"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(s.trim(), "https://github.com/owner/repo/pull/7");
    assert!(!s.contains('{'), "pretty output should not be JSON: {s}");
}

#[test]
fn pr_on_a_detached_head_errors() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "--detach"]);
    let out = stacc(p, &["pr", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("detached"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn pr_without_a_recorded_pr_errors() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    assert!(stacc(p, &["track"]).status.success()); // tracked, but no PR

    let out = stacc(p, &["pr", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no PR recorded"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn co_alias_checks_out() {
    let tmp = repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "feat"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "f"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);

    // `co` is the shipped alias for `checkout`.
    let out = stacc(p, &["co", "feat", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""op":"checkout""#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(git_out(p, &["symbolic-ref", "--short", "HEAD"]), "feat");
}
