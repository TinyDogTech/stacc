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

fn stacc(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn stacc")
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

#[test]
fn log_renders_nested_stack_json() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-1"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-2"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature-1"])
        .status
        .success());

    let out = stacc(tmp.path(), &["log", "--format", "json"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""trunk":"main""#), "got: {s}");
    assert!(s.contains(r#""name":"feature-1""#), "got: {s}");
    assert!(s.contains(r#""name":"feature-2""#), "got: {s}");
    // feature-2 is nested under feature-1
    assert!(s.contains(r#""base":"feature-1""#), "got: {s}");
}

#[test]
fn log_pretty_lists_branches() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let out = stacc(tmp.path(), &["log"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("main"), "got: {s}");
    assert!(s.contains("feature"), "got: {s}");
}

#[test]
fn log_requires_init() {
    let tmp = repo();
    let out = stacc(tmp.path(), &["log", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not initialized"), "got: {s}");
}

#[test]
fn log_marks_current_branch_and_needs_restack() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());

    // On `a`, up to date: `a` is marked current (◉) and shows no restack marker.
    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    assert!(s.contains("◉ a (current)"), "current branch not marked: {s}");
    assert!(!s.contains("needs restack"), "unexpected restack marker: {s}");

    // Advance main so `a` drifts off its base.
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "main moves"]);
    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    assert!(s.contains("◉ main (current)"), "current trunk not marked: {s}");
    assert!(
        s.contains("○ a") && s.contains("needs restack"),
        "expected restack marker on a: {s}"
    );
}

#[test]
fn log_short_emits_one_line_per_branch() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success());

    let s = String::from_utf8_lossy(&stacc(p, &["log", "short"]).stdout).into_owned();
    let lines: Vec<&str> = s.lines().collect();
    // One row per branch, trunk included, no metadata block.
    assert_eq!(lines.len(), 3, "one row per branch incl trunk: {s}");
    assert!(
        s.contains("◉ b") && s.contains("○ a") && s.contains("○ main"),
        "got: {s}"
    );
    assert!(!s.contains(" ago") && !s.contains(" - "), "short omits metadata: {s}");
    assert!(!s.contains("(current)"), "short omits the (current) suffix: {s}");
    assert!(!s.contains("needs restack"), "clean stack should have no marker: {s}");

    // The `s` value-enum alias is equivalent to `short`.
    let alias = String::from_utf8_lossy(&stacc(p, &["log", "s"]).stdout).into_owned();
    assert_eq!(alias, s, "`log s` should match `log short`");
}

#[test]
fn log_renders_a_forked_stack() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    // main -> a and main -> b: two children of the trunk.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track"]).status.success());

    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    // Two columns merging at the trunk via a fork connector; b is current.
    assert!(s.contains("○ a"), "got: {s}");
    assert!(s.contains("◉ b (current)"), "got: {s}");
    assert!(s.contains("├─┘"), "fork join expected: {s}");
    assert!(s.contains("○ main"), "got: {s}");
}

#[test]
fn log_surfaces_unreachable_branches() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success()); // a.base = main
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success()); // b.base = a
    // Re-track a onto b: now a.base=b and b.base=a, a cycle neither reachable
    // from the trunk.
    run_git(p, &["checkout", "-q", "a"]);
    assert!(stacc(p, &["track", "--base", "b"]).status.success());

    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    assert!(s.contains("unreachable:"), "got: {s}");
    assert!(s.contains("a (base: b)"), "got: {s}");
    assert!(s.contains("b (base: a)"), "got: {s}");

    // R15: the JSON path still hides them (no `unreachable` leak).
    let j = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(!j.contains("unreachable"), "got: {j}");
}

#[test]
fn log_json_is_not_changed_by_drift() {
    // R15: the JSON contract must never gain pretty-only fields like needs-restack.
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "main moves"]); // a drifts

    let s = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(s.contains(r#""name":"a""#), "got: {s}");
    assert!(!s.contains("restack"), "JSON leaked a pretty marker: {s}");
    assert!(!s.contains("needs"), "JSON leaked a pretty marker: {s}");
}

#[test]
fn log_full_shows_commit_metadata() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "feat: do the thing"]);
    assert!(stacc(p, &["track"]).status.success());

    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    assert!(s.contains("◉ a (current)"), "current marker: {s}");
    assert!(s.contains("feat: do the thing"), "subject in metadata: {s}");
    assert!(s.contains(" ago"), "relative age in metadata: {s}");
}

#[test]
fn log_long_passes_through_to_git() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "the-a-commit"]);
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc(p, &["log", "long"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    // git's own --oneline history, not stacc's graph glyphs.
    assert!(s.contains("the-a-commit"), "git history expected: {s}");
    assert!(!s.contains('◉'), "long is a git pass-through, not the stacc graph: {s}");
}

#[test]
fn log_json_includes_commit_object_and_null_pr() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());

    let s = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(s.contains(r#""name":"a""#), "got: {s}");
    assert!(s.contains(r#""subject":"a1""#), "commit object expected: {s}");
    // pr is an object-or-null; with no recorded PR it is null.
    assert!(s.contains(r#""pr":null"#), "pr should be null without a PR: {s}");
}

#[test]
fn log_show_untracked_lists_untracked_branches() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["branch", "loose"]); // an untracked local branch

    let plain = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    assert!(!plain.contains("loose"), "untracked hidden by default: {plain}");

    let s = String::from_utf8_lossy(&stacc(p, &["log", "--show-untracked"]).stdout).into_owned();
    assert!(s.contains("untracked:"), "got: {s}");
    assert!(s.contains("loose"), "got: {s}");
}
