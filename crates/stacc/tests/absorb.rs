//! `stacc absorb`: distribute staged hunks into the downstack commits that
//! introduced their lines (by blame), applied as in-memory tree rewrites, then
//! restack the upstack. The mapping-correctness and no-strand tests are
//! load-bearing: a wrong mapping silently rewrites the wrong commit, and a
//! stranded rebase is unrecoverable via `stacc abort`.

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

fn stage(dir: &Path, file: &str, contents: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run_git(dir, &["add", file]);
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

fn blob_at(dir: &Path, rev: &str, path: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "-p", &format!("{rev}:{path}")])
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

fn rebase_in_progress(dir: &Path) -> bool {
    let git_dir = dir.join(".git");
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

fn repo_init() -> TempDir {
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

/// Track the current branch on `base`.
fn track(p: &Path, base: &str) {
    assert!(
        stacc(p, &["track", "--base", base]).status.success(),
        "track on {base} failed"
    );
}

#[test]
fn absorb_lands_two_hunks_in_their_blame_commits_and_restacks_upstack() {
    let tmp = repo_init();
    let p = tmp.path();

    // Branch `a` with two commits, each introducing a distinct file.
    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "aaa\nbbb\nccc\n", "c1: add f");
    let c1 = rev(p, "HEAD");
    write_commit(p, "g.txt", "xxx\nyyy\nzzz\n", "c2: add g");
    let c2 = rev(p, "HEAD");
    track(p, "main");

    // An upstack child so we can confirm it restacks onto the absorbed tip.
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "h.txt", "h\n", "b1");
    track(p, "a");
    let b_before = rev(p, "b");

    // Back on `a`, stage one edit per file: bbb->BBB blames c1, yyy->YYY blames c2.
    run_git(p, &["checkout", "-q", "a"]);
    stage(p, "f.txt", "aaa\nBBB\nccc\n");
    stage(p, "g.txt", "xxx\nYYY\nzzz\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(
        out.status.success(),
        "absorb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "absorb");
    assert_eq!(v["absorbed"], 2, "both hunks absorbed: {v}");

    // The mapping routed f.txt's hunk to c1's rewrite and g.txt's to c2's. The
    // commit ids changed (rewritten), so assert by tree content instead: c1's
    // rewrite carries BBB, c2's carries YYY, and the tip tree has both.
    let new_c1 = rev(p, "a~1");
    let new_tip = rev(p, "a");
    assert_eq!(blob_at(p, &new_c1, "f.txt"), "aaa\nBBB\nccc\n", "BBB landed in c1");
    // c1's rewrite must NOT carry g.txt's edit (that belongs to c2).
    assert_eq!(blob_at(p, &new_tip, "f.txt"), "aaa\nBBB\nccc\n");
    assert_eq!(blob_at(p, &new_tip, "g.txt"), "xxx\nYYY\nzzz\n", "YYY landed in c2");

    // The rewrite genuinely moved the commits (c1/c2 are stale now).
    assert_ne!(new_tip, c2);
    assert_ne!(new_c1, c1);

    // The working tree is clean now: both hunks read as committed.
    let status = stacc(p, &["status"]);
    assert!(status.status.success());
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--quiet"])
            .status()
            .unwrap()
            .success(),
        "no unstaged changes remain after absorbing everything"
    );

    // The upstack child `b` restacked onto the new tip.
    let restacked = v["restacked"].as_array().expect("restacked array");
    assert!(restacked.iter().any(|x| x == "b"), "b restacked: {v}");
    assert_ne!(rev(p, "b"), b_before, "b moved onto the absorbed tip");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["merge-base", "--is-ancestor", &new_tip, "b"])
            .status()
            .unwrap()
            .success(),
        "b descends the absorbed tip"
    );

    assert!(!rebase_in_progress(p), "no rebase left in progress");
}

#[test]
fn absorb_maps_by_blame_not_newest_clean_apply() {
    // The load-bearing test. A one-line edit whose surrounding context is
    // unchanged in a LATER commit must land in the commit that INTRODUCED the
    // edited line (blame), not the newest commit it merely applies cleanly to.
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    // c1 introduces TARGET (line 2). The whole file context is created here.
    write_commit(p, "f.txt", "L1\nTARGET\nL3\n", "c1: introduce target");
    // c2 only appends an unrelated line; TARGET still blames to c1, but the
    // edit `TARGET -> TARGET-edited` applies cleanly to c2's file too.
    write_commit(p, "f.txt", "L1\nTARGET\nL3\nL4\n", "c2: append unrelated");
    track(p, "main");

    // Edit only line 2 (TARGET). Keep L4 so the only hunk is the TARGET change.
    run_git(p, &["checkout", "-q", "a"]);
    stage(p, "f.txt", "L1\nTARGET-edited\nL3\nL4\n");

    // Dry-run first: the mapping must name c1, the introducing commit.
    let c1 = rev(p, "a~1");
    let dry = stacc(p, &["absorb", "--dry-run", "--json"]);
    assert!(dry.status.success(), "{}", String::from_utf8_lossy(&dry.stderr));
    let dv = json(&dry);
    let mapping = dv["mapping"].as_array().expect("mapping array");
    assert_eq!(mapping.len(), 1, "one mapped hunk: {dv}");
    assert_eq!(
        mapping[0]["commit"].as_str(),
        Some(c1.as_str()),
        "edit blames to the INTRODUCING commit c1, not the newest c2: {dv}"
    );

    // Now apply for real and confirm c1's rewrite (a~1) carries the edit and c2
    // (the tip) did NOT swallow it at its own boundary.
    let out = stacc(p, &["absorb", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let new_c1 = rev(p, "a~1");
    assert_eq!(
        blob_at(p, &new_c1, "f.txt"),
        "L1\nTARGET-edited\nL3\n",
        "the edit landed in c1's rewrite (introducing commit)"
    );
    // The tip naturally carries it too (inherited), plus L4.
    assert_eq!(blob_at(p, "a", "f.txt"), "L1\nTARGET-edited\nL3\nL4\n");
    assert!(!rebase_in_progress(p));
}

#[test]
fn absorb_dry_run_emits_mapping_and_mutates_nothing() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "aaa\nbbb\nccc\n", "c1: add f");
    track(p, "main");
    let tip_before = rev(p, "a");

    // Stage a mappable edit AND a new file (unsupported) so the dry-run shows
    // both a mapping and an unabsorbed-with-reason entry.
    stage(p, "f.txt", "aaa\nBBB\nccc\n");
    stage(p, "new.txt", "brand new\n");

    let out = stacc(p, &["absorb", "--dry-run", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["dry_run"], true);

    let mapping = v["mapping"].as_array().expect("mapping");
    assert_eq!(mapping.len(), 1, "the f.txt edit maps: {v}");
    assert_eq!(mapping[0]["path"], "f.txt");

    let targets = v["targets"].as_array().expect("targets");
    assert_eq!(targets.len(), 1, "one target commit: {v}");
    assert_eq!(targets[0]["hunks"], 1);
    assert!(targets[0]["subject"].as_str().unwrap().contains("c1"));

    let unabsorbed = v["unabsorbed"].as_array().expect("unabsorbed");
    assert!(
        unabsorbed.iter().any(|u| u["path"] == "new.txt" && u["reason"] == "added_file"),
        "the new file is reported unabsorbed with a reason: {v}"
    );

    // Nothing changed: the tip ref and the still-staged index are untouched.
    assert_eq!(rev(p, "a"), tip_before, "dry-run did not move the ref");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap()
            .code()
            == Some(1),
        "the staged changes are still staged after a dry-run"
    );
}

#[test]
fn absorb_leaves_ambiguous_hunks_staged_and_reported() {
    // A hunk whose lines blame to TWO commits is ambiguous: leave it staged.
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "from-c1\n", "c1: line one");
    write_commit(p, "f.txt", "from-c1\nfrom-c2\n", "c2: line two");
    track(p, "main");

    // Edit BOTH lines in a single contiguous hunk: line 1 blames c1, line 2
    // blames c2, so the hunk's old range spans two commits -> ambiguous.
    stage(p, "f.txt", "edited-1\nedited-2\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["absorbed"], 0, "nothing absorbed: {v}");
    let unabsorbed = v["unabsorbed"].as_array().expect("unabsorbed");
    assert!(
        unabsorbed.iter().any(|u| u["reason"] == "ambiguous"),
        "the cross-commit hunk is reported ambiguous: {v}"
    );

    // Still staged (untouched), ref unchanged.
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap()
            .code()
            == Some(1),
        "the ambiguous change is left staged"
    );
    assert!(!rebase_in_progress(p));
}

#[test]
fn absorb_reports_unsupported_kinds_with_distinct_reasons() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "seed\n", "c1");
    track(p, "main");

    // A new file (added_file) and a binary file (binary): both unsupported.
    stage(p, "new.txt", "fresh\n");
    std::fs::write(p.join("b.dat"), [0u8, 1, 2, 0, 3]).unwrap();
    run_git(p, &["add", "b.dat"]);

    let out = stacc(p, &["absorb", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["absorbed"], 0, "nothing absorbable: {v}");
    let unabsorbed = v["unabsorbed"].as_array().expect("unabsorbed");
    // Distinct reasons, not a single bucket, and no panic / silent drop.
    assert!(
        unabsorbed.iter().any(|u| u["path"] == "new.txt" && u["reason"] == "added_file"),
        "new file -> added_file: {v}"
    );
    assert!(
        unabsorbed.iter().any(|u| u["path"] == "b.dat" && u["reason"] == "binary"),
        "binary file -> binary: {v}"
    );
    assert!(!rebase_in_progress(p));
}

#[test]
fn absorb_that_maps_nothing_leaves_the_repo_untouched_and_no_rebase() {
    // The no-strand test: an absorb that maps nothing must leave NO
    // rebase-in-progress and an untouched branch (fully absorbed or untouched).
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "seed\n", "c1");
    track(p, "main");
    let tip_before = rev(p, "a");

    // Only an unsupported change is staged (a new file): nothing maps.
    stage(p, "new.txt", "nope\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["absorbed"], 0);

    // The branch is untouched and no rebase is in progress.
    assert_eq!(rev(p, "a"), tip_before, "the branch did not move");
    assert!(!rebase_in_progress(p), "no rebase left in progress");
    // The staged change is still staged, never dropped.
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap()
            .code()
            == Some(1),
        "the unmapped change is left staged"
    );
}

#[test]
fn absorb_leaves_only_the_unabsorbed_hunks_as_unstaged_changes() {
    // The reset --mixed mechanic: absorbed hunks read as committed, and only the
    // unabsorbed ones remain as unstaged working-tree modifications.
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "aaa\nbbb\nccc\n", "c1: add f");
    track(p, "main");

    // One absorbable edit (f.txt bbb->BBB) and one unabsorbable change (a brand
    // new file), staged together.
    stage(p, "f.txt", "aaa\nBBB\nccc\n");
    stage(p, "new.txt", "leftover\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["absorbed"], 1, "only the f.txt hunk absorbed: {v}");

    // f.txt's edit is now committed (no longer a pending change).
    assert_eq!(blob_at(p, "a", "f.txt"), "aaa\nBBB\nccc\n");

    // new.txt is NOT committed: it remains as an untracked/unstaged leftover.
    assert!(
        blob_at(p, "a", "new.txt").is_empty(),
        "the unabsorbed new file is not committed onto a"
    );
    assert!(
        p.join("new.txt").exists(),
        "the leftover file is still in the working tree"
    );
    // Its content survives intact.
    assert_eq!(
        std::fs::read_to_string(p.join("new.txt")).unwrap(),
        "leftover\n"
    );
    assert!(!rebase_in_progress(p));
}

#[test]
fn absorb_upstack_conflict_is_resumable_via_continue() {
    // An absorb whose upstack restack conflicts must surface a structured,
    // resumable conflict (never strand a raw rebase), and `continue` finishes it.
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "shared.txt", "original\n", "c1: shared");
    track(p, "main");
    // A child that edits the same file in a way that will collide once `a`'s
    // shared.txt is rewritten by the absorb.
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "shared.txt", "b-version\n", "b edits shared");
    track(p, "a");

    // On `a`, stage an edit to shared.txt (blames c1). Absorbing it rewrites
    // `a`'s tip, and restacking `b` onto that rewrite conflicts on shared.txt.
    run_git(p, &["checkout", "-q", "a"]);
    stage(p, "shared.txt", "a-version\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(!out.status.success(), "the upstack restack should conflict");
    let v = json(&out);
    assert_eq!(v["type"], "conflict", "structured conflict, not a strand: {v}");
    assert_eq!(v["branch"], "b");
    // The absorb itself landed on `a` (its tip was rewritten before the restack).
    assert_eq!(blob_at(p, "a", "shared.txt"), "a-version\n");
    assert!(rebase_in_progress(p), "a stacc rebase is in progress to resume");

    // Resolve and continue: the conflict drains and `b` lands on the absorbed `a`.
    std::fs::write(p.join("shared.txt"), "resolved\n").unwrap();
    run_git(p, &["add", "shared.txt"]);
    let cont = stacc(p, &["continue", "--json"]);
    assert!(cont.status.success(), "{}", String::from_utf8_lossy(&cont.stderr));
    assert!(!rebase_in_progress(p), "no rebase left after continue");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["merge-base", "--is-ancestor", "a", "b"])
            .status()
            .unwrap()
            .success(),
        "b descends the absorbed a after continue"
    );
}

#[test]
fn absorb_refuses_when_an_upstack_branch_is_in_another_worktree() {
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "aaa\nbbb\nccc\n", "c1");
    track(p, "main");
    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "h.txt", "h\n", "b1");
    track(p, "a");
    let a_before = rev(p, "a");

    // Move off `b` first, then check it out in another worktree (a branch can't
    // be in two worktrees at once).
    run_git(p, &["checkout", "-q", "a"]);
    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-b").to_str().unwrap(), "b"],
    );

    // Stage a mappable edit on `a`, then absorb: it must refuse before mutating.
    stage(p, "f.txt", "aaa\nBBB\nccc\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(!out.status.success(), "absorb must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(
        v["type"], "worktree_conflict",
        "refused with the worktree_conflict discriminator: {v}"
    );

    // Nothing mutated: `a` is unchanged and the edit is still staged.
    assert_eq!(rev(p, "a"), a_before, "a did not move");
    assert!(!rebase_in_progress(p));
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap()
            .code()
            == Some(1),
        "the edit is still staged after the refusal"
    );
}

#[test]
fn absorb_cross_branch_routes_hunk_into_downstack_commit() {
    // A hunk staged on branch `b` that blames a commit on the lower branch `a`
    // must be absorbed into `a`'s commit chain and `b` restacked onto the new `a`.
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "aaa\nbbb\nccc\n", "a1: add f");
    track(p, "main");

    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "g.txt", "xxx\n", "b1: add g");
    track(p, "a");

    let a_tip_before = rev(p, "a");
    let b_tip_before = rev(p, "b");

    // Stage an edit to f.txt (introduced by a1 on branch `a`) while on `b`.
    stage(p, "f.txt", "aaa\nBBB\nccc\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(
        out.status.success(),
        "absorb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["op"], "absorb");
    assert_eq!(v["absorbed"], 1, "one hunk absorbed: {v}");

    // `a`'s ref must have moved to incorporate the edit.
    let a_tip_after = rev(p, "a");
    assert_ne!(a_tip_after, a_tip_before, "a moved to the rewritten tip");

    // The edit landed in `a`'s commit tree.
    assert_eq!(blob_at(p, &a_tip_after, "f.txt"), "aaa\nBBB\nccc\n");

    // `b` was restacked onto the new `a`.
    let b_tip_after = rev(p, "b");
    assert_ne!(b_tip_after, b_tip_before, "b moved onto the new a");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["merge-base", "--is-ancestor", &a_tip_after, "b"])
            .status()
            .unwrap()
            .success(),
        "b descends the rewritten a"
    );

    // No staged changes remain.
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap()
            .success(),
        "no staged changes after cross-branch absorb"
    );
    assert!(!rebase_in_progress(p));
}

#[test]
fn absorb_cross_branch_and_own_branch_simultaneously() {
    // Two hunks staged on `b`: one blames `a`'s commit, one blames `b`'s own
    // commit. Both must be absorbed in a single call.
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "aaa\nbbb\nccc\n", "a1: add f");
    track(p, "main");

    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "g.txt", "xxx\nyyy\nzzz\n", "b1: add g");
    track(p, "a");

    let a_before = rev(p, "a");

    // Stage: f.txt edit blames a1 (downstack), g.txt edit blames b1 (own).
    stage(p, "f.txt", "aaa\nBBB\nccc\n");
    stage(p, "g.txt", "xxx\nYYY\nzzz\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(
        out.status.success(),
        "absorb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["absorbed"], 2, "both hunks absorbed: {v}");

    // `a`'s ref moved (carries f.txt edit).
    assert_ne!(rev(p, "a"), a_before, "a moved");
    assert_eq!(blob_at(p, "a", "f.txt"), "aaa\nBBB\nccc\n");

    // `b`'s tip carries both edits.
    assert_eq!(blob_at(p, "b", "f.txt"), "aaa\nBBB\nccc\n");
    assert_eq!(blob_at(p, "b", "g.txt"), "xxx\nYYY\nzzz\n");

    // Clean index.
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap()
            .success(),
        "nothing left staged"
    );
    assert!(!rebase_in_progress(p));
}

#[test]
fn absorb_cross_branch_dry_run_includes_branch_field() {
    // --dry-run JSON must include "branch" in both mapping entries and targets
    // when the blame commit belongs to a downstack branch.
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "aaa\nbbb\nccc\n", "a1: add f");
    track(p, "main");

    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "g.txt", "xxx\n", "b1: add g");
    track(p, "a");

    let a_tip = rev(p, "a");
    let b_tip = rev(p, "b");

    // Stage an edit blaming a's commit (downstack).
    stage(p, "f.txt", "aaa\nBBB\nccc\n");

    let out = stacc(p, &["absorb", "--dry-run", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["dry_run"], true);

    let mapping = v["mapping"].as_array().expect("mapping array");
    assert_eq!(mapping.len(), 1, "one mapping entry: {v}");
    assert_eq!(mapping[0]["branch"], "a", "mapping entry names the downstack branch: {v}");

    let targets = v["targets"].as_array().expect("targets array");
    assert_eq!(targets.len(), 1, "one target: {v}");
    assert_eq!(targets[0]["branch"], "a", "target names the downstack branch: {v}");

    // Dry-run mutated nothing.
    assert_eq!(rev(p, "a"), a_tip, "a did not move");
    assert_eq!(rev(p, "b"), b_tip, "b did not move");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap()
            .code()
            == Some(1),
        "staged change is still staged after dry-run"
    );
}

#[test]
fn absorb_outside_branch_unchanged_when_not_in_any_chain_branch() {
    // A hunk whose blame commit is on trunk (before the stack) must stay
    // outside_branch regardless of cross-branch routing.
    let tmp = repo_init();
    let p = tmp.path();

    // A trunk commit that introduces the file we will edit.
    write_commit(p, "base.txt", "line1\nline2\nline3\n", "trunk: add base");
    let trunk_commit = rev(p, "main");

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "own.txt", "own\n", "a1: own file");
    track(p, "main");

    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "other.txt", "other\n", "b1");
    track(p, "a");

    let a_before = rev(p, "a");
    let b_before = rev(p, "b");

    // Edit a line introduced by the trunk commit (not in any stack branch).
    stage(p, "base.txt", "line1\nLINE2\nline3\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["absorbed"], 0, "trunk-origin hunk not absorbed: {v}");

    let unabsorbed = v["unabsorbed"].as_array().expect("unabsorbed array");
    assert!(
        unabsorbed.iter().any(|u| u["reason"] == "outside_branch"),
        "trunk hunk is outside_branch: {v}"
    );

    // Nothing mutated.
    assert_eq!(rev(p, "a"), a_before);
    assert_eq!(rev(p, "b"), b_before);
    assert_eq!(rev(p, "main"), trunk_commit, "trunk commit unchanged");
    assert!(!rebase_in_progress(p));
}

#[test]
fn absorb_cross_branch_worktree_guard_fires_for_downstack_branch() {
    // If a downstack branch is checked out in another worktree, absorb must
    // refuse with worktree_conflict before mutating anything.
    let tmp = repo_init();
    let p = tmp.path();

    run_git(p, &["checkout", "-q", "-b", "a"]);
    write_commit(p, "f.txt", "aaa\nbbb\nccc\n", "a1: add f");
    track(p, "main");

    run_git(p, &["checkout", "-q", "-b", "b"]);
    write_commit(p, "g.txt", "xxx\n", "b1");
    track(p, "a");

    let a_before = rev(p, "a");
    let b_before = rev(p, "b");

    // Check out `a` (the downstack branch) in a second worktree while on `b`.
    let holder = TempDir::new().unwrap();
    run_git(
        p,
        &["worktree", "add", "-q", holder.path().join("wt-a").to_str().unwrap(), "a"],
    );

    // Stage an edit that would route to `a` (the worktree-occupied branch).
    stage(p, "f.txt", "aaa\nBBB\nccc\n");

    let out = stacc(p, &["absorb", "--json"]);
    assert!(!out.status.success(), "absorb must refuse: {:?}", json(&out));
    let v = json(&out);
    assert_eq!(
        v["type"], "worktree_conflict",
        "refused with worktree_conflict: {v}"
    );

    // Nothing mutated.
    assert_eq!(rev(p, "a"), a_before, "a did not move");
    assert_eq!(rev(p, "b"), b_before, "b did not move");
    assert!(!rebase_in_progress(p));
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap()
            .code()
            == Some(1),
        "edit is still staged after refusal"
    );
}
