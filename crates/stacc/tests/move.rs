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

/// The commit a ref points at.
fn sha(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The recorded state blob for `branch` (from the state ref).
fn state_blob(dir: &Path, branch: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["show", &format!("refs/stacc/data:branches/{branch}")])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn move_only_moves_the_branch_alone_and_children_stay() {
    let tmp = init_repo();
    let p = tmp.path();
    // main -> a -> b, and main -> c (sibling of a).
    create(p, "a", "a.txt", "a\n");
    create(p, "b", "b.txt", "b\n");
    run_git(p, &["checkout", "-q", "main"]);
    create(p, "c", "c.txt", "c\n");

    // Move ONLY a onto c; its child b must stay put on a's old position.
    run_git(p, &["checkout", "-q", "a"]);
    let b_before = sha(p, "b");
    let out = stacc(p, &["move", "--onto", "c", "--only", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":["a"]"#), "got: {s}");
    assert!(s.contains(r#""reparented":["b"]"#), "got: {s}");
    // a moved: it descends c now.
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "c", "a"]));
    // b's ref is untouched and it does not descend the moved a.
    assert_eq!(sha(p, "b"), b_before);
    assert!(!git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
    // b's recorded base is re-pointed onto a's OLD parent (main), so it does
    // not follow a on the next restack.
    let blob = state_blob(p, "b");
    assert!(blob.contains(r#""name": "main""#), "got: {blob}");
    assert_eq!(current_branch(p), "a");
}

#[test]
fn move_only_without_children_just_moves_the_branch() {
    let tmp = init_repo();
    let p = tmp.path();
    // main -> a, and main -> b (siblings).
    create(p, "a", "a.txt", "a\n");
    run_git(p, &["checkout", "-q", "main"]);
    create(p, "b", "b.txt", "b\n"); // left on b

    let out = stacc(p, &["move", "--onto", "a", "--only", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":["b"]"#), "got: {s}");
    assert!(s.contains(r#""reparented":[]"#), "got: {s}");
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
    assert!(log.contains(r#""name":"a""#), "got: {log}");
    // The rollback put a back on main, so nothing is based on c any more (a
    // staying on c would leave `"base":"c"` in the tree).
    assert!(!log.contains(r#""base":"c""#), "a was not rolled back: {log}");
    assert!(!git_ok(p, &["merge-base", "--is-ancestor", "c", "a"]));
}

#[test]
fn move_onto_a_downstack_ancestor_is_rejected() {
    let tmp = init_repo();
    let p = tmp.path();
    // main -> a -> b; b already descends a, so moving b onto main would only
    // flatten a out, which move does not do.
    create(p, "a", "a.txt", "a\n");
    create(p, "b", "b.txt", "b\n"); // on b
    let out = stacc(p, &["move", "--onto", "main", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("already descends"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn move_onto_the_current_base_is_rejected() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a", "a.txt", "a\n"); // a on main
    let out = stacc(p, &["move", "--onto", "main", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("already based on"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn move_onto_an_untracked_base_is_rejected() {
    let tmp = init_repo();
    let p = tmp.path();
    create(p, "a", "a.txt", "a\n");
    let out = stacc(p, &["move", "--onto", "ghost", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not the trunk or a tracked branch"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn abort_after_a_partial_move_keeps_the_move_and_warns() {
    let tmp = init_repo();
    let p = tmp.path();
    // main -> x -> c -> g, where only g collides with n on shared.txt.
    create(p, "x", "x.txt", "x\n");
    create(p, "c", "c.txt", "c\n");
    create(p, "g", "shared.txt", "g-version\n");
    // main -> n (sibling) also touching shared.txt.
    run_git(p, &["checkout", "-q", "main"]);
    create(p, "n", "shared.txt", "n-version\n");

    // Move x's subtree onto n: x and c restack cleanly, g conflicts on shared.txt.
    run_git(p, &["checkout", "-q", "x"]);
    assert!(
        !stacc(p, &["move", "--onto", "n"]).status.success(),
        "expected a conflict on g"
    );
    assert!(
        std::fs::read_to_string(p.join(".git/stacc-continue.json"))
            .unwrap()
            .contains(r#""remaining":["g"]"#),
        "expected the conflict to land on g"
    );

    // Abort: x (and c) already restacked onto n, so the move is KEPT + warned.
    let out = stacc(p, &["abort", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("stays moved"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The move stuck: x is on n.
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "n", "x"]));
}
