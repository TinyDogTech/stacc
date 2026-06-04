use std::process::{Command, Output};

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

/// Like `run_git` but returns whether git succeeded (for `--is-ancestor` checks).
fn git_ok(dir: &std::path::Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git")
        .success()
}

fn stacc(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
}

fn write_commit(dir: &std::path::Path, file: &str, contents: &str, msg: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run_git(dir, &["add", file]);
    run_git(dir, &["commit", "-q", "-m", msg]);
}

fn repo() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://example.com/r.git"],
    );
    tmp
}

/// `main -> a` where both touch `shared.txt`, then restack `a` so it conflicts.
/// Returns the repo mid-conflict: a rebase is in progress, the continuation and
/// context artifacts are written.
fn conflicted_restack() -> TempDir {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "shared.txt", "a-version\n", "a edits shared");
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    write_commit(p, "shared.txt", "main-version\n", "main edits shared");
    run_git(p, &["checkout", "-q", "a"]);
    let out = stacc(p, &["restack", "--stack"]);
    assert!(!out.status.success(), "expected a conflict");
    tmp
}

fn rebase_in_progress(p: &std::path::Path) -> bool {
    p.join(".git/rebase-merge").exists() || p.join(".git/rebase-apply").exists()
}

#[test]
fn continue_resumes_a_conflicted_restack() {
    let tmp = conflicted_restack();
    let p = tmp.path();
    // Resolve the conflict and stage it, then continue.
    std::fs::write(p.join("shared.txt"), "resolved\n").expect("write");
    run_git(p, &["add", "shared.txt"]);

    let out = stacc(p, &["continue", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":["a"]"#), "got: {s}");
    // a now descends from the new main, and all recovery state is gone.
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "main", "a"]));
    assert!(!rebase_in_progress(p));
    assert!(!p.join(".git/stacc-continue.json").exists());
    assert!(!p.join(".git/stacc-conflict-context.json").exists());
}

#[test]
fn abort_undoes_a_conflicted_restack() {
    let tmp = conflicted_restack();
    let p = tmp.path();

    let out = stacc(p, &["abort", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#""aborted":true"#));
    // The rebase is undone (a does NOT descend from new main) and artifacts gone.
    assert!(!rebase_in_progress(p));
    assert!(!git_ok(p, &["merge-base", "--is-ancestor", "main", "a"]));
    assert!(!p.join(".git/stacc-continue.json").exists());
    assert!(!p.join(".git/stacc-conflict-context.json").exists());
}

#[test]
fn continue_with_nothing_in_progress_errors() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    let out = stacc(p, &["continue", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no operation in progress"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn abort_with_nothing_in_progress_errors() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    let out = stacc(p, &["abort", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("nothing to abort"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn conflict_error_names_continue_and_abort() {
    let tmp = conflicted_restack();
    let p = tmp.path();
    // Re-trigger the conflict under --format json to inspect the error payload.
    // (conflicted_restack left a rebase in progress; abort first to reset.)
    assert!(stacc(p, &["abort"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);
    let out = stacc(p, &["restack", "--stack", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""continue":"stacc continue""#), "got: {s}");
    assert!(s.contains(r#""abort":"stacc abort""#), "got: {s}");
}

/// `main -> a -> b` where `a` conflicts with main on `shared.txt` but `b` only
/// touches `b.txt`, so a `--stack` restack conflicts on `a` and, once resolved,
/// drains cleanly through `b`.
fn conflicted_multi_stack() -> TempDir {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "shared.txt", "a-version\n", "a edits shared");
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    assert!(stacc(p, &["track", "--base", "a"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    write_commit(p, "shared.txt", "main-version\n", "main edits shared");
    run_git(p, &["checkout", "-q", "a"]);
    let out = stacc(p, &["restack", "--stack"]);
    assert!(!out.status.success(), "expected a conflict on a");
    tmp
}

#[test]
fn continue_drains_the_rest_of_the_stack() {
    let tmp = conflicted_multi_stack();
    let p = tmp.path();
    std::fs::write(p.join("shared.txt"), "resolved\n").expect("write");
    run_git(p, &["add", "shared.txt"]);

    let out = stacc(p, &["continue", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""op":"restack""#), "got: {s}");
    assert!(s.contains(r#""restacked":["a","b"]"#), "got: {s}");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "main", "b"]));
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
}

#[test]
fn abort_refuses_a_foreign_rebase() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "x"]);
    write_commit(p, "shared.txt", "x\n", "x1");
    run_git(p, &["checkout", "-q", "main"]);
    write_commit(p, "shared.txt", "main\n", "m1");
    run_git(p, &["checkout", "-q", "x"]);
    // A hand-run rebase that conflicts: stacc has no record of it.
    let _ = Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["rebase", "main"])
        .env("GIT_EDITOR", "true")
        .status();
    assert!(rebase_in_progress(p), "expected a foreign rebase in progress");

    let out = stacc(p, &["abort", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("non-stacc rebase"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // stacc left the user's rebase untouched.
    assert!(rebase_in_progress(p));
}

#[test]
fn continue_clears_a_stale_continuation() {
    let tmp = conflicted_restack();
    let p = tmp.path();
    // User aborts the rebase by hand, leaving the stacc continuation stale.
    run_git(p, &["rebase", "--abort"]);
    assert!(p.join(".git/stacc-continue.json").exists());

    let out = stacc(p, &["continue", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stale"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!p.join(".git/stacc-continue.json").exists());
}

#[test]
fn abort_clears_a_stale_continuation_without_a_rebase() {
    let tmp = conflicted_restack();
    let p = tmp.path();
    run_git(p, &["rebase", "--abort"]); // rebase gone, continuation stale
    let out = stacc(p, &["abort", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#""aborted":true"#));
    assert!(!p.join(".git/stacc-continue.json").exists());
}
