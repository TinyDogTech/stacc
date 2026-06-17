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

/// A git repo with `main` + an initial commit and an `origin` remote, with stacc
/// initialized.
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

#[test]
fn create_with_staged_changes_commits_and_tracks() {
    let tmp = init_repo();
    let p = tmp.path();
    std::fs::write(p.join("f.txt"), "hi\n").expect("write");
    run_git(p, &["add", "f.txt"]);

    let out = stacc(p, &["create", "feat-x", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""branch":"feat-x""#), "got: {s}");
    assert!(s.contains(r#""base":"main""#), "got: {s}");
    assert!(s.contains(r#""committed":true"#), "got: {s}");

    // Switched to the new branch, the staged file is committed, index is clean,
    // and the default commit message is the branch name.
    assert_eq!(current_branch(p), "feat-x");
    assert!(git_ok(p, &["cat-file", "-e", "HEAD:f.txt"]));
    assert!(git_ok(p, &["diff", "--cached", "--quiet"]));
    assert_eq!(git_out(p, &["log", "-1", "--format=%s"]), "feat-x");
    let head = git_out(p, &["rev-parse", "HEAD"]);
    assert!(s.contains(&format!(r#""sha":"{head}""#)), "got: {s}");

    // Tracked with base main.
    let log = stacc(p, &["log", "--format", "json"]);
    let ls = String::from_utf8_lossy(&log.stdout);
    assert!(ls.contains(r#""name":"feat-x""#), "got: {ls}");
    assert!(ls.contains(r#""base":"main""#), "got: {ls}");
}

#[test]
fn create_with_nothing_staged_makes_an_empty_tracked_branch() {
    let tmp = init_repo();
    let p = tmp.path();

    let out = stacc(p, &["create", "feat-y", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""committed":false"#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(current_branch(p), "feat-y");
    // No new commit: feat-y is exactly main's tip.
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "feat-y", "main"]));
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "main", "feat-y"]));
    let log = stacc(p, &["log", "--format", "json"]);
    assert!(String::from_utf8_lossy(&log.stdout).contains(r#""name":"feat-y""#));
}

#[test]
fn create_uninitialized_errors() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);

    let out = stacc(p, &["create", "feat-z", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not initialized"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn create_stacks_on_the_current_branch() {
    let tmp = init_repo();
    let p = tmp.path();
    // First branch off main.
    assert!(stacc(p, &["create", "a"]).status.success());
    assert_eq!(current_branch(p), "a");
    // Second branch off a, with staged work and a custom message.
    std::fs::write(p.join("a.txt"), "a\n").expect("write");
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["create", "b", "-m", "b work"]).status.success());
    assert_eq!(current_branch(p), "b");
    assert_eq!(git_out(p, &["log", "-1", "--format=%s"]), "b work");

    let log = stacc(p, &["log", "--format", "json"]);
    let s = String::from_utf8_lossy(&log.stdout);
    assert!(s.contains(r#""name":"b""#), "got: {s}");
    assert!(s.contains(r#""base":"a""#), "got: {s}");
}

#[test]
fn create_from_detached_head_errors() {
    let tmp = init_repo();
    let p = tmp.path();
    let head = git_out(p, &["rev-parse", "HEAD"]);
    run_git(p, &["checkout", "-q", &head]); // detach HEAD

    let out = stacc(p, &["create", "feat-d", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("detached HEAD"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn create_refuses_an_already_tracked_name() {
    let tmp = init_repo();
    let p = tmp.path();
    assert!(stacc(p, &["create", "dup"]).status.success());
    let out = stacc(p, &["create", "dup", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("already tracked"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn create_refuses_the_trunk_name() {
    let tmp = init_repo();
    let p = tmp.path();
    let out = stacc(p, &["create", "main", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("trunk"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn create_on_an_existing_git_branch_fails_without_partial_state() {
    let tmp = init_repo();
    let p = tmp.path();
    // A git branch stacc does not track.
    run_git(p, &["branch", "taken"]);
    let out = stacc(p, &["create", "taken", "--format", "json"]);
    assert!(!out.status.success());
    // No checkout happened and nothing was tracked.
    assert_eq!(current_branch(p), "main");
    let log = stacc(p, &["log", "--format", "json"]);
    assert!(!String::from_utf8_lossy(&log.stdout).contains(r#""name":"taken""#));
}

#[test]
fn create_all_stages_tracked_and_untracked_changes() {
    let tmp = init_repo();
    let p = tmp.path();
    // A tracked file on main to modify, plus a brand-new untracked file.
    std::fs::write(p.join("tracked.txt"), "v1\n").expect("write");
    run_git(p, &["add", "tracked.txt"]);
    run_git(p, &["commit", "-q", "-m", "tracked"]);
    std::fs::write(p.join("tracked.txt"), "v2\n").expect("write");
    std::fs::write(p.join("new.txt"), "new\n").expect("write");

    let out = stacc(p, &["create", "feat-all", "--all", "-m", "all work", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""committed":true"#), "got: {s}");
    assert_eq!(current_branch(p), "feat-all");
    // Both the modification and the untracked file are in the commit, and the
    // working tree is clean afterwards.
    assert_eq!(git_out(p, &["show", "HEAD:tracked.txt"]), "v2");
    assert!(git_ok(p, &["cat-file", "-e", "HEAD:new.txt"]));
    assert_eq!(git_out(p, &["status", "--porcelain"]), "");
}

// STA-117: a path the same `-a` commit adds to `.gitignore` must stop being
// tracked, not get swept in. `git add -A` keeps an already-tracked path even
// after a rule for it is added, so the dir would otherwise be committed once
// more before the ignore "takes effect".
#[test]
fn create_all_drops_a_path_the_commit_adds_to_gitignore() {
    let tmp = init_repo();
    let p = tmp.path();
    // An index dir tracked before it gets ignored (the dogfooding trigger).
    std::fs::create_dir(p.join("cache")).expect("mkdir");
    std::fs::write(p.join("cache/idx"), "v1\n").expect("write");
    run_git(p, &["add", "cache/idx"]);
    run_git(p, &["commit", "-q", "-m", "track cache"]);

    // Ignore it and touch it, all part of the change `create -a` commits.
    std::fs::write(p.join(".gitignore"), "cache/\n").expect("write");
    std::fs::write(p.join("cache/idx"), "v2\n").expect("write");

    let out = stacc(p, &["create", "feat-ig", "-a", "-m", "ignore cache", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // .gitignore is committed; cache/idx is not in the new commit (it stops
    // being tracked), and the working-tree file is untouched.
    assert!(git_ok(p, &["cat-file", "-e", "HEAD:.gitignore"]));
    assert!(
        !git_ok(p, &["cat-file", "-e", "HEAD:cache/idx"]),
        "cache/idx must not be committed once it is ignored"
    );
    assert_eq!(std::fs::read_to_string(p.join("cache/idx")).unwrap(), "v2\n");
    // The index matches the commit (nothing left staged).
    assert!(git_ok(p, &["diff", "--cached", "--quiet"]));
}

// STA-117: the ignore-drop is gated on a staged `.gitignore` change. A
// deliberately force-added file (`git add -f`) under a pre-existing ignore rule
// must survive a later `-a` commit that does not touch `.gitignore`.
#[test]
fn create_all_keeps_a_force_added_file_when_gitignore_is_unchanged() {
    let tmp = init_repo();
    let p = tmp.path();
    // A pre-existing ignore rule plus a force-added tracked file under it.
    std::fs::write(p.join(".gitignore"), "*.log\n").expect("write");
    run_git(p, &["add", ".gitignore"]);
    std::fs::write(p.join("important.log"), "keep\n").expect("write");
    run_git(p, &["add", "-f", "important.log"]);
    run_git(p, &["commit", "-q", "-m", "base"]);

    // A later -a commit that does not touch .gitignore must not un-track it.
    std::fs::write(p.join("important.log"), "v2\n").expect("write");
    std::fs::write(p.join("other.txt"), "x\n").expect("write");
    let out = stacc(p, &["create", "feat-x", "-a", "-m", "more", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // important.log stays tracked and updated; other.txt is added.
    assert!(git_ok(p, &["cat-file", "-e", "HEAD:important.log"]));
    assert_eq!(git_out(p, &["show", "HEAD:important.log"]), "v2");
    assert!(git_ok(p, &["cat-file", "-e", "HEAD:other.txt"]));
}

#[test]
fn create_onto_bases_on_the_named_branch() {
    let tmp = init_repo();
    let p = tmp.path();
    std::fs::write(p.join("a.txt"), "a\n").expect("write");
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["create", "a", "-m", "a1"]).status.success());

    // On `a`; create `b` based on main instead of the current branch.
    let out = stacc(p, &["create", "b", "--onto", "main", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""base":"main""#), "got: {s}");
    assert_eq!(current_branch(p), "b");
    // Parentage: b sits on main's tip, not on a (a's commit is not in b).
    assert_eq!(git_out(p, &["rev-parse", "b"]), git_out(p, &["rev-parse", "main"]));
    assert!(!git_ok(p, &["merge-base", "--is-ancestor", "a", "b"]));
    // Recorded base: `stacc parent` on b names main.
    let parent = stacc(p, &["parent", "--format", "json"]);
    assert!(
        String::from_utf8_lossy(&parent.stdout).contains(r#""parent":"main""#),
        "got: {}",
        String::from_utf8_lossy(&parent.stdout)
    );
}

#[test]
fn create_insert_reparents_children_onto_the_new_branch() {
    let tmp = init_repo();
    let p = tmp.path();
    // main -> a -> b, then back on a.
    std::fs::write(p.join("a.txt"), "a\n").expect("write");
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["create", "a", "-m", "a1"]).status.success());
    std::fs::write(p.join("b.txt"), "b\n").expect("write");
    run_git(p, &["add", "b.txt"]);
    assert!(stacc(p, &["create", "b", "-m", "b1"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);

    // Insert n between a and b.
    std::fs::write(p.join("n.txt"), "n\n").expect("write");
    run_git(p, &["add", "n.txt"]);
    let out = stacc(p, &["create", "n", "--insert", "-m", "n1", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""base":"a""#), "got: {s}");
    assert!(s.contains(r#""reparented":["b"]"#), "got: {s}");
    assert!(s.contains(r#""restacked":["b"]"#), "got: {s}");
    assert_eq!(current_branch(p), "n");

    // b's recorded base is now n, and b's history descends n's commit.
    run_git(p, &["checkout", "-q", "b"]);
    let parent = stacc(p, &["parent", "--format", "json"]);
    assert!(
        String::from_utf8_lossy(&parent.stdout).contains(r#""parent":"n""#),
        "got: {}",
        String::from_utf8_lossy(&parent.stdout)
    );
    assert!(git_ok(p, &["merge-base", "--is-ancestor", "n", "b"]));
    assert!(git_ok(p, &["cat-file", "-e", "b:n.txt"]));
}

#[test]
fn create_without_insert_leaves_children_on_the_old_parent() {
    let tmp = init_repo();
    let p = tmp.path();
    std::fs::write(p.join("a.txt"), "a\n").expect("write");
    run_git(p, &["add", "a.txt"]);
    assert!(stacc(p, &["create", "a", "-m", "a1"]).status.success());
    std::fs::write(p.join("b.txt"), "b\n").expect("write");
    run_git(p, &["add", "b.txt"]);
    assert!(stacc(p, &["create", "b", "-m", "b1"]).status.success());
    run_git(p, &["checkout", "-q", "a"]);

    assert!(stacc(p, &["create", "sib"]).status.success());
    // b still has a as its recorded base.
    run_git(p, &["checkout", "-q", "b"]);
    let parent = stacc(p, &["parent", "--format", "json"]);
    assert!(
        String::from_utf8_lossy(&parent.stdout).contains(r#""parent":"a""#),
        "got: {}",
        String::from_utf8_lossy(&parent.stdout)
    );
}

#[test]
fn create_insert_with_onto_errors() {
    let tmp = init_repo();
    let p = tmp.path();
    let out = stacc(p, &["create", "x", "--insert", "--onto", "main", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("mutually exclusive"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn create_handles_slashed_branch_names() {
    let tmp = init_repo();
    let p = tmp.path();
    std::fs::write(p.join("f.txt"), "hi\n").expect("write");
    run_git(p, &["add", "f.txt"]);
    let out = stacc(p, &["create", "jillian/feat", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""branch":"jillian/feat""#),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(current_branch(p), "jillian/feat");
    let log = stacc(p, &["log", "--format", "json"]);
    assert!(
        String::from_utf8_lossy(&log.stdout).contains(r#""name":"jillian/feat""#),
        "got: {}",
        String::from_utf8_lossy(&log.stdout)
    );
}
