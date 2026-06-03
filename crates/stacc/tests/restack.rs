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

/// `main -> a -> b`, then advance `main` so `a` (and `b` above it) drift off it.
fn drifted_stack() -> TempDir {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a\n", "a1");
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b\n", "b1");
    assert!(stacc(p, &["track", "--base", "a"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    write_commit(p, "m.txt", "m\n", "main moves");
    tmp
}

#[test]
fn restack_default_scope_leaves_downstack_untouched() {
    let tmp = drifted_stack();
    let p = tmp.path();
    // From `b`, the default scope is `b` + its upstack (empty). `a` is downstack
    // and drifted, but out of scope — nothing is restacked.
    run_git(p, &["checkout", "-q", "b"]);
    let out = stacc(p, &["restack", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":[]"#), "got: {s}");
}

#[test]
fn restack_stack_scope_repairs_whole_stack() {
    let tmp = drifted_stack();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "b"]);
    let out = stacc(p, &["restack", "--stack", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":["a","b"]"#), "got: {s}");
    // The drift is repaired: both branches now descend from main's new tip.
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "main", "a"]));
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "main", "b"]));
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
    // Idempotent: a second --stack restack is a no-op.
    let again = stacc(p, &["restack", "--stack", "--format", "json"]);
    assert!(String::from_utf8_lossy(&again.stdout).contains(r#""restacked":[]"#));
}

#[test]
fn restack_requires_init() {
    let tmp = repo();
    let out = stacc(tmp.path(), &["restack", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not initialized"), "got: {s}");
}

#[test]
fn restack_conflict_writes_restack_continuation() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "shared.txt", "a-version\n", "a edits shared");
    assert!(stacc(p, &["track"]).status.success());
    // main edits the same file so restacking `a` onto it conflicts.
    run_git(p, &["checkout", "-q", "main"]);
    write_commit(p, "shared.txt", "main-version\n", "main edits shared");
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["restack", "--stack"]);
    assert!(!out.status.success());
    // The continuation records this as a `restack` operation (for `stacc continue`).
    let git_dir = p.join(".git");
    let cont = std::fs::read_to_string(git_dir.join("stacc-continue.json"))
        .expect("continuation written");
    assert!(cont.contains(r#""op":"restack""#), "got: {cont}");
    assert!(cont.contains(r#""remaining":["a"]"#), "got: {cont}");
    assert!(
        git_dir.join("stacc-conflict-context.json").exists(),
        "context missing"
    );
}

#[test]
fn restack_default_scope_restacks_current_upstack() {
    let tmp = drifted_stack();
    let p = tmp.path();
    // From `a` (drifted off main), the default scope is `a` + its upstack `b`.
    run_git(p, &["checkout", "-q", "a"]);
    let out = stacc(p, &["restack", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":["a","b"]"#), "got: {s}");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "main", "a"]));
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
}

#[test]
fn restack_pretty_output() {
    let tmp = drifted_stack();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "a"]);
    // First run restacks the upstack -> "Restacked" lines.
    let out = stacc(p, &["restack"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Restacked a"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // Second run is a no-op -> "Already up to date."
    let out = stacc(p, &["restack"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Already up to date."),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn restack_on_trunk_errors() {
    let tmp = drifted_stack();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "main"]);
    let out = stacc(p, &["restack", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("trunk"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // --stack still restacks from the trunk.
    assert!(stacc(p, &["restack", "--stack"]).status.success());
}
