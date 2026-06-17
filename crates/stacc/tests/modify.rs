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

fn current_branch(dir: &Path) -> String {
    git_out(dir, &["symbolic-ref", "--short", "HEAD"])
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

/// `main -> a (a.txt) -> b (b.txt)`, all tracked, left checked out on `a`.
fn stack_main_a_b() -> TempDir {
    let tmp = init_repo();
    let p = tmp.path();
    std::fs::write(p.join("a.txt"), "a\n").expect("write");
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["create", "a", "-m", "a1"]).status.success());
    std::fs::write(p.join("b.txt"), "b\n").expect("write");
    run_git(p, &["add", "b.txt"]);
    assert!(stacc(p, &["create", "b", "-m", "b1"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);
    tmp
}

/// `main -> a -> b` where both `a` and `b` touch `shared.txt`, so amending `a`
/// and restacking `b` conflicts. Left checked out on `a`.
fn stack_with_modify_conflict() -> TempDir {
    let tmp = init_repo();
    let p = tmp.path();
    std::fs::write(p.join("shared.txt"), "a-version\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    assert!(stacc(p, &["create", "a", "-m", "a1"]).status.success());
    std::fs::write(p.join("shared.txt"), "b-version\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    assert!(stacc(p, &["create", "b", "-m", "b1"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);
    tmp
}

#[test]
fn modify_amends_and_restacks_the_upstack() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    let a_before = git_out(p, &["rev-parse", "a"]);
    let b_before = git_out(p, &["rev-parse", "b"]);
    std::fs::write(p.join("a.txt"), "a-modified\n").expect("write");
    run_git(p, &["add", "a.txt"]);

    let out = stacc(p, &["modify", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""op":"modify""#), "got: {s}");
    assert!(s.contains(r#""amended":true"#), "got: {s}");
    assert!(s.contains(r#""restacked":["b"]"#), "got: {s}");
    // a's tip moved, b was restacked onto it, and we are back on a.
    assert_ne!(git_out(p, &["rev-parse", "a"]), a_before);
    assert_ne!(git_out(p, &["rev-parse", "b"]), b_before);
    assert_eq!(current_branch(p), "a");
    assert!(
        s.contains(&format!(r#""sha":"{}""#, git_out(p, &["rev-parse", "a"]))),
        "got: {s}"
    );
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
}

#[test]
fn modify_commit_appends_and_restacks() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    let count_before: i32 = git_out(p, &["rev-list", "--count", "a"]).parse().unwrap();
    std::fs::write(p.join("a2.txt"), "a2\n").expect("write");
    run_git(p, &["add", "a2.txt"]);

    let out = stacc(p, &["modify", "--commit", "-m", "a2", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""amended":false"#), "got: {s}");
    assert!(s.contains(r#""restacked":["b"]"#), "got: {s}");
    let count_after: i32 = git_out(p, &["rev-list", "--count", "a"]).parse().unwrap();
    assert_eq!(count_after, count_before + 1);
    assert_eq!(git_out(p, &["log", "-1", "--format=%s", "a"]), "a2");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
}

#[test]
fn modify_on_trunk_errors() {
    let tmp = init_repo();
    let p = tmp.path();
    let out = stacc(p, &["modify", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("trunk"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn modify_without_own_commit_suggests_commit() {
    let tmp = init_repo();
    let p = tmp.path();
    assert!(stacc(p, &["create", "empty"]).status.success()); // empty == main tip
    let out = stacc(p, &["modify", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("--commit"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn modify_conflict_records_modify_continuation_and_continue_finishes() {
    let tmp = stack_with_modify_conflict();
    let p = tmp.path();
    std::fs::write(p.join("shared.txt"), "a-modified\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    assert!(!stacc(p, &["modify"]).status.success(), "expected a conflict");

    let cont = std::fs::read_to_string(p.join(".git/stacc-continue.json")).expect("continuation");
    assert!(cont.contains(r#""op":"modify""#), "got: {cont}");
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
    assert!(s.contains(r#""op":"modify""#), "got: {s}");
    assert!(s.contains(r#""restacked":["b"]"#), "got: {s}");
    // The resumed modify carries the same branch/sha shape as the direct command,
    // minus `amended` (the continuation does not record the amend/append choice).
    assert!(s.contains(r#""branch":"a""#), "got: {s}");
    assert!(
        s.contains(&format!(r#""sha":"{}""#, git_out(p, &["rev-parse", "a"]))),
        "got: {s}"
    );
    assert!(!s.contains("amended"), "amended is direct-only: {s}");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
}

#[test]
fn abort_of_a_conflicted_modify_restores_the_amend() {
    let tmp = stack_with_modify_conflict();
    let p = tmp.path();
    let a_before = git_out(p, &["rev-parse", "a"]);
    let b_before = git_out(p, &["rev-parse", "b"]);
    std::fs::write(p.join("shared.txt"), "a-modified\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    assert!(!stacc(p, &["modify"]).status.success(), "expected a conflict");
    // a was amended mid-operation.
    assert_ne!(git_out(p, &["rev-parse", "a"]), a_before);

    let out = stacc(p, &["abort", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // abort reset a back to its pre-amend tip and undid b's in-progress rebase.
    assert_eq!(git_out(p, &["rev-parse", "a"]), a_before, "a not restored");
    assert_eq!(git_out(p, &["rev-parse", "b"]), b_before, "b not restored");
    assert!(!p.join(".git/stacc-continue.json").exists());
}

/// `main -> a (shared) -> b (b.txt) -> c (shared)`. Amending `a`'s shared file
/// lets `b` restack clean but makes `c` conflict: a later-child conflict.
fn stack_with_multichild_conflict() -> TempDir {
    let tmp = init_repo();
    let p = tmp.path();
    std::fs::write(p.join("shared.txt"), "a-orig\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    assert!(stacc(p, &["create", "a", "-m", "a1"]).status.success());
    std::fs::write(p.join("b.txt"), "b\n").expect("write");
    run_git(p, &["add", "b.txt"]);
    assert!(stacc(p, &["create", "b", "-m", "b1"]).status.success());
    std::fs::write(p.join("shared.txt"), "c-version\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    assert!(stacc(p, &["create", "c", "-m", "c1"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);
    tmp
}

#[test]
fn abort_keeps_the_amend_when_a_child_already_restacked() {
    let tmp = stack_with_multichild_conflict();
    let p = tmp.path();
    let a_before = git_out(p, &["rev-parse", "a"]);
    std::fs::write(p.join("shared.txt"), "a-modified\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    // b restacks clean, c conflicts.
    assert!(
        !stacc(p, &["modify"]).status.success(),
        "expected a conflict on c"
    );
    let a_amended = git_out(p, &["rev-parse", "a"]);
    assert_ne!(a_amended, a_before);
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));

    let out = stacc(p, &["abort", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // a stays amended (resetting it would orphan the already-restacked b), and b
    // still descends a: consistent, no orphan.
    assert_eq!(
        git_out(p, &["rev-parse", "a"]),
        a_amended,
        "a should stay amended"
    );
    assert!(
        git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]),
        "b orphaned"
    );
    assert!(!p.join(".git/stacc-continue.json").exists());
}

#[test]
fn modify_on_untracked_branch_errors() {
    let tmp = init_repo();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "-b", "loose"]);
    let out = stacc(p, &["modify", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not tracked"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn modify_amend_with_nothing_staged_errors() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    // On `a` (which has its own commit), nothing staged and no -m: pure no-op.
    let out = stacc(p, &["modify", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("nothing staged"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn modify_all_stages_everything_then_amends() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    std::fs::write(p.join("a.txt"), "a-edited\n").expect("write"); // tracked, unstaged
    std::fs::write(p.join("x.txt"), "x\n").expect("write"); // untracked

    let out = stacc(p, &["modify", "--all", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""amended":true"#), "got: {s}");
    assert!(s.contains(r#""restacked":["b"]"#), "got: {s}");
    // Both the tracked edit and the untracked file landed in a's tip, and the
    // working tree is clean.
    assert_eq!(git_out(p, &["show", "a:a.txt"]), "a-edited");
    assert!(git_ok(p, &["cat-file", "-e", "a:x.txt"]));
    assert_eq!(git_out(p, &["status", "--porcelain"]), "");
    assert_eq!(current_branch(p), "a");
}

// STA-117: the `create -a` fix applies equally to `modify -a`. A path the same
// `--all` amend adds to `.gitignore` stops being tracked rather than getting
// folded back into the tip.
#[test]
fn modify_all_drops_a_path_the_change_adds_to_gitignore() {
    let tmp = init_repo();
    let p = tmp.path();
    // A branch whose own commit tracks an index dir.
    assert!(stacc(p, &["create", "wip", "-m", "wip"]).status.success());
    std::fs::create_dir(p.join("cache")).expect("mkdir");
    std::fs::write(p.join("cache/idx"), "v1\n").expect("write");
    assert!(
        stacc(p, &["modify", "--commit", "-a", "-m", "track cache"])
            .status
            .success()
    );
    assert!(git_ok(p, &["cat-file", "-e", "wip:cache/idx"]));

    // Ignore it and touch it, then fold with --all.
    std::fs::write(p.join(".gitignore"), "cache/\n").expect("write");
    std::fs::write(p.join("cache/idx"), "v2\n").expect("write");
    let out = stacc(p, &["modify", "--all", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The amended tip carries .gitignore and no longer tracks cache/idx; the
    // working-tree file is untouched.
    assert!(git_ok(p, &["cat-file", "-e", "wip:.gitignore"]));
    assert!(
        !git_ok(p, &["cat-file", "-e", "wip:cache/idx"]),
        "cache/idx must be untracked after the ignore"
    );
    assert_eq!(std::fs::read_to_string(p.join("cache/idx")).unwrap(), "v2\n");
}

#[test]
fn modify_into_lands_staged_changes_in_a_downstack_tip() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    run_git(p, &["checkout", "-q", "b"]);
    let a_before = git_out(p, &["rev-parse", "a"]);
    std::fs::write(p.join("a.txt"), "a-into\n").expect("write");
    run_git(p, &["add", "a.txt"]);

    let out = stacc(p, &["modify", "--into", "a", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""op":"modify""#), "got: {s}");
    assert!(s.contains(r#""into":"a""#), "got: {s}");
    assert!(s.contains(r#""applied":1"#), "got: {s}");
    // The change landed in a's tip and is carried through b's replayed history.
    assert_ne!(git_out(p, &["rev-parse", "a"]), a_before);
    assert_eq!(git_out(p, &["show", "a:a.txt"]), "a-into");
    assert_eq!(git_out(p, &["show", "b:a.txt"]), "a-into");
    // a's commit message survived the rewrite, b still descends a, and the
    // working tree is clean (the staged change now reads as committed).
    assert_eq!(git_out(p, &["log", "-1", "--format=%s", "a"]), "a1");
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
    assert_eq!(current_branch(p), "b");
    assert_eq!(git_out(p, &["status", "--porcelain"]), "");
}

#[test]
fn modify_into_a_non_downstack_branch_errors() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    // On a; b is in a's UPstack, not its downstack.
    let out = stacc(p, &["modify", "--into", "b", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("downstack"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn modify_edit_rewords_without_changing_the_tree() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    let tree_before = git_out(p, &["rev-parse", "a^{tree}"]);

    let out = stacc(p, &["modify", "--edit", "-m", "a1 reworded", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""restacked":["b"]"#), "got: {s}");
    assert_eq!(git_out(p, &["log", "-1", "--format=%s", "a"]), "a1 reworded");
    assert_eq!(git_out(p, &["rev-parse", "a^{tree}"]), tree_before);
}

#[test]
fn modify_edit_without_message_errors() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    let out = stacc(p, &["modify", "--edit", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("--message"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn modify_patch_includes_only_matching_paths_and_keeps_the_rest_staged() {
    let tmp = stack_main_a_b();
    let p = tmp.path();
    std::fs::write(p.join("a.txt"), "a-patched\n").expect("write");
    std::fs::write(p.join("x.txt"), "x\n").expect("write");
    run_git(p, &["add", "a.txt", "x.txt"]);

    let out = stacc(p, &["modify", "--patch", "a.txt", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // a.txt's change was amended in; x.txt is NOT in the commit and is still
    // staged afterwards.
    assert_eq!(git_out(p, &["show", "a:a.txt"]), "a-patched");
    assert!(!git_ok(p, &["cat-file", "-e", "a:x.txt"]));
    assert_eq!(git_out(p, &["diff", "--cached", "--name-only"]), "x.txt");
    assert_eq!(current_branch(p), "a");
}
