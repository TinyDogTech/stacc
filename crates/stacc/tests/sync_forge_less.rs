//! `stacc sync` forge-less mode: a non-GitHub remote (or the local-mode key)
//! fetches trunk and restacks without any forge API call, proposing likely-merged
//! branches via the local heuristic without dropping them (KTD-4/6). The remote
//! here is a local bare repo, whose path is not a github.com URL, so `parse_remote`
//! returns None and forge-less engages.

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

fn branch_ref_exists(dir: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("spawn git")
        .status
        .success()
}

fn track(p: &Path, base: &str) {
    assert!(
        stacc(p, &["track", "--base", base]).status.success(),
        "track on {base} failed"
    );
}

/// An initialized stacc repo whose `origin` is a local bare repo (a non-github.com
/// URL), so sync runs forge-less. The remote `TempDir` must outlive the repo.
fn repo_with_local_remote() -> (TempDir, TempDir) {
    let remote = TempDir::new().expect("remote dir");
    Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(remote.path())
        .status()
        .expect("init bare");

    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", remote.path().to_str().unwrap()]);
    run_git(p, &["push", "-q", "-u", "origin", "main"]);
    assert!(stacc(p, &["init"]).status.success(), "init failed");
    (tmp, remote)
}

#[test]
fn sync_on_a_non_github_remote_is_forge_less_with_a_note() {
    let (tmp, _remote) = repo_with_local_remote();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");

    let out = stacc(p, &["sync"]);
    assert!(
        out.status.success(),
        "forge-less sync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no reachable forge"),
        "auto-engaged forge-less emits the skipped-detection note: {stderr}"
    );
    assert!(
        !stderr.contains("not a GitHub URL"),
        "a non-GitHub remote is no longer a hard error: {stderr}"
    );
}

#[test]
fn local_mode_runs_forge_less_silently() {
    let (tmp, _remote) = repo_with_local_remote();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    assert!(stacc(p, &["config", "set", "local", "true"]).status.success());

    let out = stacc(p, &["sync"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("no reachable forge"),
        "the local-mode key opts in, so the note is suppressed: {stderr}"
    );
}

#[test]
fn forge_less_sync_proposes_likely_merged_without_dropping() {
    let (tmp, _remote) = repo_with_local_remote();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");
    // Land a's change on main as a squash, advancing trunk on an unrelated file:
    // a now looks merged by the local heuristic, but no forge confirms it.
    run_git(p, &["checkout", "-q", "main"]);
    write_commit(p, "other.txt", "x\n", "trunk advance");
    write_commit(p, "a.txt", "a\n", "squash a");
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["sync", "--format", "json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");

    let likely = v["likely_merged"].as_array().expect("likely_merged array");
    assert!(
        likely.iter().any(|l| l["branch"] == "a"),
        "a is proposed as likely-merged: {v}"
    );
    // Propose-only: nothing is auto-dropped, and a survives.
    assert!(
        v["merged"].as_array().expect("merged array").is_empty(),
        "the heuristic never drops on a forge-less run: {v}"
    );
    assert!(branch_ref_exists(p, "a"), "a is not dropped, only proposed");
}

#[test]
fn offline_sync_skips_the_fetch_and_notes_it() {
    let (tmp, _remote) = repo_with_local_remote();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    track(p, "main");

    let out = stacc(p, &["sync", "--offline"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--offline skipped merged-PR detection"),
        "offline keeps its own note: {stderr}"
    );
}
