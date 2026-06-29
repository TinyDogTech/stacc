//! `stacc parent` / `stacc children`: read-only neighbor probes over state.

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

/// Run `stacc <args> --format json` expecting success, parsed.
fn json(dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let mut full: Vec<&str> = args.to_vec();
    full.extend(["--json"]);
    let out = stacc(dir, &full);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON")
}

fn repo() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://github.com/TinyDogTech/stacc.git"],
    );
    tmp
}

/// Create and track `name` on top of the current branch, leaving HEAD on it.
fn stack_branch(dir: &std::path::Path, name: &str, base: &str) {
    run_git(dir, &["checkout", "-q", "-b", name]);
    assert!(stacc(dir, &["track", "--base", base]).status.success());
}

/// An initialized repo with a tracked `main <- a <- b` stack, HEAD on `b`.
fn stack() -> TempDir {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    stack_branch(tmp.path(), "a", "main");
    stack_branch(tmp.path(), "b", "a");
    tmp
}

#[test]
fn parent_of_a_stacked_branch_is_its_recorded_base() {
    let tmp = stack();
    // HEAD is on b.
    let v = json(tmp.path(), &["parent"]);
    assert_eq!(v, serde_json::json!({ "op": "parent", "parent": "a", "schema_version": 3 }));

    // Pretty is just the name.
    let out = stacc(tmp.path(), &["parent"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a\n");
}

#[test]
fn parent_of_a_trunk_based_branch_is_the_trunk() {
    let tmp = stack();
    run_git(tmp.path(), &["checkout", "-q", "a"]);
    let v = json(tmp.path(), &["parent"]);
    assert_eq!(v["op"], "parent");
    assert_eq!(v["parent"], "main");
}

#[test]
fn parent_on_the_trunk_is_null_and_exits_zero() {
    let tmp = stack();
    run_git(tmp.path(), &["checkout", "-q", "main"]);
    let v = json(tmp.path(), &["parent"]);
    assert_eq!(v, serde_json::json!({ "op": "parent", "parent": null, "schema_version": 3 }));

    // Pretty prints nothing, still exit 0.
    let out = stacc(tmp.path(), &["parent"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn parent_on_an_untracked_branch_is_null_and_exits_zero() {
    let tmp = stack();
    run_git(tmp.path(), &["checkout", "-q", "-b", "loose", "main"]);
    let v = json(tmp.path(), &["parent"]);
    assert_eq!(v, serde_json::json!({ "op": "parent", "parent": null, "schema_version": 3 }));
}

#[test]
fn children_of_a_branch_lists_its_direct_child() {
    let tmp = stack();
    run_git(tmp.path(), &["checkout", "-q", "a"]);
    let v = json(tmp.path(), &["children"]);
    assert_eq!(v, serde_json::json!({ "op": "children", "children": ["b"], "schema_version": 3 }));
}

#[test]
fn children_of_a_leaf_is_an_empty_array_and_exits_zero() {
    let tmp = stack();
    // HEAD is on b, the leaf.
    let v = json(tmp.path(), &["children"]);
    assert_eq!(v, serde_json::json!({ "op": "children", "children": [], "schema_version": 3 }));

    // Pretty prints nothing, still exit 0.
    let out = stacc(tmp.path(), &["children"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn children_at_a_fork_are_name_ordered() {
    let tmp = stack();
    // Create the fork on b in reverse name order; the listing is still sorted.
    stack_branch(tmp.path(), "kid-z", "b");
    run_git(tmp.path(), &["checkout", "-q", "b"]);
    stack_branch(tmp.path(), "kid-y", "b");
    run_git(tmp.path(), &["checkout", "-q", "b"]);

    let v = json(tmp.path(), &["children"]);
    assert_eq!(
        v,
        serde_json::json!({ "op": "children", "children": ["kid-y", "kid-z"], "schema_version": 3 })
    );

    // Pretty is one name per line, same order.
    let out = stacc(tmp.path(), &["children"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "kid-y\nkid-z\n");
}

#[test]
fn children_on_the_trunk_lists_trunk_based_branches() {
    let tmp = stack();
    run_git(tmp.path(), &["checkout", "-q", "main"]);
    stack_branch(tmp.path(), "c", "main");
    run_git(tmp.path(), &["checkout", "-q", "main"]);

    let v = json(tmp.path(), &["children"]);
    assert_eq!(
        v,
        serde_json::json!({ "op": "children", "children": ["a", "c"], "schema_version": 3 })
    );
}
