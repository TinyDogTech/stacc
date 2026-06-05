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

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git")
        .success()
}

fn stacc(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
}

fn current_branch(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", "https://example.com/r.git"]);
    assert!(stacc(p, &["init"]).status.success());
    tmp
}

/// Create branch `name` off the current branch, committing `file=contents`.
fn create(p: &Path, name: &str, file: &str, contents: &str) {
    std::fs::write(p.join(file), contents).expect("write");
    run_git(p, &["add", "."]);
    assert!(stacc(p, &["create", name, "-m", name]).status.success());
}

#[test]
fn move_reparents_onto_a_new_base() {
    let tmp = init_repo();
    let p = tmp.path();
    // main -> a, and main -> b (siblings, different files so no conflict).
    create(p, "a", "a.txt", "a\n");
    run_git(p, &["checkout", "-q", "main"]);
    create(p, "b", "b.txt", "b\n"); // left checked out on b

    let out = stacc(p, &["move", "--onto", "a", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""op":"move""#), "got: {s}");
    assert!(s.contains(r#""branch":"b""#), "got: {s}");
    assert!(s.contains(r#""base":"a""#), "got: {s}");
    assert!(s.contains(r#""restacked":["b"]"#), "got: {s}");
    // b now descends a, and we are back on b.
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
    assert_eq!(current_branch(p), "b");
}

#[test]
fn move_moves_the_whole_subtree() {
    let tmp = init_repo();
    let p = tmp.path();
    // main -> a -> b
    create(p, "a", "a.txt", "a\n");
    create(p, "b", "b.txt", "b\n");
    // main -> c (sibling of a)
    run_git(p, &["checkout", "-q", "main"]);
    create(p, "c", "c.txt", "c\n");

    // Move a (with its child b) onto c.
    run_git(p, &["checkout", "-q", "a"]);
    let out = stacc(p, &["move", "--onto", "c", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""restacked":["a","b"]"#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // a descends c, and b still descends a (subtree preserved).
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "c", "a"]));
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
}

#[test]
fn move_onto_own_upstack_errors() {
    let tmp = init_repo();
    let p = tmp.path();
    // main -> a -> b; moving a onto its own child b is a cycle.
    create(p, "a", "a.txt", "a\n");
    create(p, "b", "b.txt", "b\n");
    run_git(p, &["checkout", "-q", "a"]);

    let out = stacc(p, &["move", "--onto", "b", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cycle"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn move_conflict_records_continuation_and_continue_finishes() {
    let tmp = init_repo();
    let p = tmp.path();
    // a and c are siblings on main that both add shared.txt differently.
    create(p, "a", "shared.txt", "a-version\n");
    run_git(p, &["checkout", "-q", "main"]);
    create(p, "c", "shared.txt", "c-version\n");
    run_git(p, &["checkout", "-q", "a"]);

    // Move a onto c: both touch shared.txt, so the rebase conflicts.
    assert!(
        !stacc(p, &["move", "--onto", "c"]).status.success(),
        "expected a conflict"
    );
    let cont = std::fs::read_to_string(p.join(".git/stacc-continue.json")).expect("continuation");
    assert!(cont.contains(r#""op":"move""#), "got: {cont}");
    assert!(cont.contains(r#""branch":"a""#), "got: {cont}");

    std::fs::write(p.join("shared.txt"), "resolved\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    let out = stacc(p, &["continue", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""op":"move""#), "got: {s}");
    assert!(s.contains(r#""branch":"a""#), "got: {s}");
    assert!(s.contains(r#""base":"c""#), "got: {s}");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "c", "a"]));
}

#[test]
fn abort_of_a_conflicted_move_restores_the_base() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a", "shared.txt", "a-version\n");
    run_git(p, &["checkout", "-q", "main"]);
    create(p, "c", "shared.txt", "c-version\n");
    run_git(p, &["checkout", "-q", "a"]);
    assert!(
        !stacc(p, &["move", "--onto", "c"]).status.success(),
        "expected a conflict"
    );

    let out = stacc(p, &["abort", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // a's recorded base rolled back to main, and a does not descend c.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(
        log.contains(r#""name":"a""#) && log.contains(r#""base":"main""#),
        "got: {log}"
    );
    assert!(!git_ok(p, &["merge-base", "--is-ancestor", "c", "a"]));
}
