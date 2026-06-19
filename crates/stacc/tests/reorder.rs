//! `stacc reorder`: reorder the downstack via `--order` and restack. The
//! conflict tests are load-bearing: reorder re-points N bases and any of the N
//! rebases can conflict after earlier ones already landed, so `continue` must
//! drain the remaining queue and `abort` must restore EVERY chain branch (ref
//! and recorded base), not just the first one, the single-anchor Move guard
//! deliberately does not apply here.

use std::path::Path;
use std::process::{Command, Output};

use stacc_git::Git;
use stacc_state::StateStore;
use tempfile::TempDir;

// A nonexistent GitHub URL: nothing here fetches or pushes it.
const ORIGIN: &str = "https://github.com/stacc-sandbox/example.git";

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
        .output()
        .expect("spawn git")
        .status
        .success()
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

fn rev(dir: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", r])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

fn rebase_in_progress(dir: &Path) -> bool {
    let git_dir = dir.join(".git");
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

/// The recorded `(base.name, base.hash)` of `branch`.
fn base_of(dir: &Path, branch: &str) -> (String, String) {
    let state = StateStore::new(Git::open(dir)).load().unwrap();
    let b = state.branches.get(branch).unwrap_or_else(|| panic!("`{branch}` is tracked"));
    (b.base.name.clone(), b.base.hash.clone())
}

fn repo_init() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    let p = tmp.path();
    run_git(p, &["init", "-q", "-b", "main"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(p, &["remote", "add", "origin", ORIGIN]);
    assert!(stacc(p, &["init"]).status.success());
    tmp
}

fn track(p: &Path, base: &str) {
    assert!(
        stacc(p, &["track", "--base", base]).status.success(),
        "track on {base} failed"
    );
}

/// main <- a <- b <- c, each branch adding its own file (no overlap, so any
/// reorder of it rebases cleanly). Leaves the repo checked out on `c`.
fn linear_stack(p: &Path) {
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "a.txt", "a-content\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "b.txt", "b-content\n", "b1");
    track(p, "a");
    run_git(p, &["checkout", "-q", "-b", "c"]);
    write_commit(p, "c.txt", "c-content\n", "c1");
    track(p, "b");
}

/// main <- a <- b where both edit `shared.txt` (b modifies a's version), so
/// reordering to [b, a] conflicts on every chain rebase. Leaves HEAD on `b`.
fn overlapping_stack(p: &Path) {
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "shared.txt", "a-version\n", "a1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "shared.txt", "b-version\n", "b1");
    track(p, "a");
}

#[test]
fn reorder_repoints_bases_and_restacks_into_the_new_order() {
    let tmp = repo_init();
    let p = tmp.path();
    linear_stack(p);

    let out = stacc(p, &["reorder", "--order", "b,a,c", "--json"]);
    assert!(
        out.status.success(),
        "reorder failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "reorder");
    assert_eq!(v["order"], serde_json::json!(["b", "a", "c"]));
    assert_eq!(v["restacked"], serde_json::json!(["b", "a", "c"]));

    // Recorded bases follow the new order, hashes at the new bases' tips.
    assert_eq!(base_of(p, "b"), ("main".into(), rev(p, "main")));
    assert_eq!(base_of(p, "a"), ("b".into(), rev(p, "b")));
    assert_eq!(base_of(p, "c"), ("a".into(), rev(p, "a")));

    // Git history matches: each one-commit branch sits directly on its new base.
    assert_eq!(rev(p, "b~1"), rev(p, "main"), "b rebased onto main");
    assert_eq!(rev(p, "a~1"), rev(p, "b"), "a rebased onto b");
    assert_eq!(rev(p, "c~1"), rev(p, "a"), "c rebased onto a");

    // b dropped a's commit (no a.txt); a carries b's content underneath.
    assert!(!git_ok(p, &["cat-file", "-e", "b:a.txt"]), "b no longer contains a.txt");
    assert!(git_ok(p, &["cat-file", "-e", "a:b.txt"]), "a now contains b.txt");
    assert!(git_ok(p, &["cat-file", "-e", "c:a.txt"]), "c still sees the whole chain");

    // The full chain is intact in the log, and the finish is clean.
    let log = String::from_utf8_lossy(&stacc(p, &["log", "--json"]).stdout).into_owned();
    for name in ["a", "b", "c"] {
        assert!(log.contains(&format!(r#""name":"{name}""#)), "{name} tracked: {log}");
    }
    assert_eq!(current_branch(p), "c", "back on the starting branch");
    assert!(!rebase_in_progress(p));
    assert!(git_ok(p, &["diff", "--quiet", "HEAD"]), "clean tree after reorder");
}

#[test]
fn reorder_rejects_unknown_duplicate_missing_and_absent_order_specs() {
    let tmp = repo_init();
    let p = tmp.path();
    linear_stack(p);
    let tips = [rev(p, "a"), rev(p, "b"), rev(p, "c")];

    let cases: &[(&[&str], &str)] = &[
        (&["reorder", "--order", "b,a,zz", "--json"], "zz"),
        (&["reorder", "--order", "b,b,c", "--json"], "more than once"),
        (&["reorder", "--order", "b,a", "--json"], "missing c"),
        (&["reorder", "--json"], "missing --order"),
    ];
    for (args, needle) in cases {
        let out = stacc(p, args);
        assert!(!out.status.success(), "{args:?} must be rejected");
        let v = json(&out);
        assert_eq!(v["type"], "usage", "structured error for {args:?}: {v}");
        let msg = v["message"].as_str().expect("message");
        assert!(msg.contains(needle), "error names the offender ({needle}): {msg}");
    }

    // Nothing mutated by any rejected spec.
    assert_eq!([rev(p, "a"), rev(p, "b"), rev(p, "c")], tips);
    assert_eq!(base_of(p, "a").0, "main");
    assert_eq!(base_of(p, "b").0, "a");
    assert_eq!(base_of(p, "c").0, "b");
    assert!(!rebase_in_progress(p));
}

#[test]
fn reorder_refuses_the_trunk_and_an_untracked_branch() {
    let tmp = repo_init();
    let p = tmp.path();
    linear_stack(p);

    run_git(p, &["checkout", "-q", "main"]);
    let out = stacc(p, &["reorder", "--order", "b,a,c", "--json"]);
    assert!(!out.status.success());
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    assert!(v["message"].as_str().unwrap().contains("trunk"), "{v}");

    run_git(p, &["checkout", "-q", "-b", "feral", "main"]);
    let out = stacc(p, &["reorder", "--order", "feral", "--json"]);
    assert!(!out.status.success());
    let v = json(&out);
    assert_eq!(v["type"], "usage");
    assert!(v["message"].as_str().unwrap().contains("not tracked"), "{v}");
}

#[test]
fn reorder_with_the_current_order_is_a_noop() {
    let tmp = repo_init();
    let p = tmp.path();
    linear_stack(p);
    let tips = [rev(p, "a"), rev(p, "b"), rev(p, "c")];

    let out = stacc(p, &["reorder", "--order", "a,b,c", "--json"]);
    assert!(
        out.status.success(),
        "noop reorder failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "reorder");
    assert_eq!(v["unchanged"], true, "reported as a no-op: {v}");

    assert_eq!([rev(p, "a"), rev(p, "b"), rev(p, "c")], tips, "nothing moved");
    assert_eq!(base_of(p, "a").0, "main");
    assert_eq!(base_of(p, "b").0, "a");
    assert_eq!(base_of(p, "c").0, "b");
}

#[test]
fn continue_of_a_conflicted_reorder_finishes_the_reorder() {
    let tmp = repo_init();
    let p = tmp.path();
    overlapping_stack(p);

    // Swapping two branches that edit the same file conflicts on b's rebase
    // first (its shared.txt edit replays onto a trunk without the file).
    let out = stacc(p, &["reorder", "--order", "b,a", "--json"]);
    assert!(!out.status.success(), "expected a conflict on b");
    let v = json(&out);
    assert_eq!(v["type"], "conflict", "structured conflict: {v}");
    assert_eq!(v["branch"], "b");
    let cont = std::fs::read_to_string(p.join(".git/stacc-continue.json")).expect("continuation");
    assert!(cont.contains(r#""op":"reorder""#), "got: {cont}");
    assert!(cont.contains(r#""pre_state""#), "carries the pre-state: {cont}");

    // Resolve b's conflict; a's rebase then conflicts in turn (it re-adds
    // shared.txt over b's resolved version), and the rewritten continuation
    // must keep its reorder identity.
    std::fs::write(p.join("shared.txt"), "b-resolved\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    let out = stacc(p, &["continue", "--json"]);
    assert!(!out.status.success(), "expected a follow-up conflict on a");
    let v = json(&out);
    assert_eq!(v["type"], "conflict", "{v}");
    assert_eq!(v["branch"], "a");
    let cont = std::fs::read_to_string(p.join(".git/stacc-continue.json")).expect("continuation");
    assert!(cont.contains(r#""op":"reorder""#), "identity preserved: {cont}");

    std::fs::write(p.join("shared.txt"), "a-resolved\n").expect("write");
    run_git(p, &["add", "shared.txt"]);
    let out = stacc(p, &["continue", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "reorder");
    assert_eq!(v["order"], serde_json::json!(["b", "a"]));
    let restacked = v["restacked"].as_array().expect("restacked array");
    assert!(restacked.iter().any(|x| x == "a"), "a drained from the queue: {v}");

    // The final order is b on main, a on b, in state and in git.
    assert_eq!(base_of(p, "b"), ("main".into(), rev(p, "main")));
    assert_eq!(base_of(p, "a"), ("b".into(), rev(p, "b")));
    assert_eq!(rev(p, "b~1"), rev(p, "main"));
    assert_eq!(rev(p, "a~1"), rev(p, "b"));
    assert!(!rebase_in_progress(p));
    assert!(!p.join(".git/stacc-continue.json").exists());
}

#[test]
fn abort_of_a_conflicted_reorder_restores_every_branch_and_base() {
    let tmp = repo_init();
    let p = tmp.path();
    // a and b overlap on shared.txt; c adds its own file, so reordering to
    // [c, b, a] rebases c cleanly FIRST and then conflicts on b, leaving a
    // chain member already rewritten when the abort runs.
    overlapping_stack(p);
    run_git(p, &["checkout", "-q", "-b", "c"]);
    write_commit(p, "c.txt", "c-content\n", "c1");
    track(p, "b");
    let pre = [
        ("a", rev(p, "a"), base_of(p, "a")),
        ("b", rev(p, "b"), base_of(p, "b")),
        ("c", rev(p, "c"), base_of(p, "c")),
    ];

    let out = stacc(p, &["reorder", "--order", "c,b,a", "--json"]);
    assert!(!out.status.success(), "expected a conflict on b");
    let v = json(&out);
    assert_eq!(v["type"], "conflict", "{v}");
    assert_eq!(v["branch"], "b");
    assert_ne!(rev(p, "c"), pre[2].1, "c already rebased before the conflict");

    let out = stacc(p, &["abort", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // TOTAL restore: every ref back at its pre-reorder tip (including the
    // already-rebased c) and every recorded base (name AND hash) restored.
    for (name, tip, base) in &pre {
        assert_eq!(&rev(p, name), tip, "{name}'s ref restored");
        assert_eq!(&base_of(p, name), base, "{name}'s recorded base restored");
    }
    assert!(!rebase_in_progress(p));
    assert!(!p.join(".git/stacc-continue.json").exists());
    assert!(git_ok(p, &["diff", "--quiet", "HEAD"]), "clean tree after abort");
}

#[test]
fn reorder_refuses_when_a_branch_is_checked_out_in_another_worktree() {
    let tmp = repo_init();
    let p = tmp.path();
    linear_stack(p);
    let tips = [rev(p, "a"), rev(p, "b"), rev(p, "c")];

    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-a").to_str().unwrap(), "a"],
    );

    let out = stacc(p, &["reorder", "--order", "b,a,c", "--json"]);
    assert!(!out.status.success(), "reorder must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "worktree_conflict", "{v}");
    assert_eq!(v["branch"], "a", "names the borrowed branch: {v}");

    // Nothing mutated.
    assert_eq!([rev(p, "a"), rev(p, "b"), rev(p, "c")], tips);
    assert_eq!(base_of(p, "b").0, "a");
    assert!(!rebase_in_progress(p));
}

#[test]
fn reorder_refuses_a_dirty_working_tree() {
    let tmp = repo_init();
    let p = tmp.path();
    linear_stack(p);
    let tips = [rev(p, "a"), rev(p, "b"), rev(p, "c")];

    std::fs::write(p.join("c.txt"), "uncommitted edit\n").expect("write");

    let out = stacc(p, &["reorder", "--order", "b,a,c", "--json"]);
    assert!(!out.status.success(), "reorder must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(v["type"], "usage", "{v}");
    assert!(v["message"].as_str().unwrap().contains("uncommitted"), "{v}");

    // Nothing mutated, and the user's edit survives.
    assert_eq!([rev(p, "a"), rev(p, "b"), rev(p, "c")], tips);
    assert_eq!(base_of(p, "b").0, "a");
    assert_eq!(
        std::fs::read_to_string(p.join("c.txt")).unwrap(),
        "uncommitted edit\n"
    );
    assert!(!rebase_in_progress(p));
}
