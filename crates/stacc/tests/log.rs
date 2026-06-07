use std::process::{Command, Output};

use httpmock::MockServer;
use stacc_git::Git;
use stacc_state::{PullRequest, StateStore};
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
    stacc_env(dir, args, &[])
}

fn stacc_env(dir: &std::path::Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stacc"));
    cmd.current_dir(dir).args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn stacc")
}

/// A repo whose remote is a real GitHub URL, so the live PR-status path runs
/// (the default `repo()` uses a non-GitHub remote and short-circuits it).
fn github_repo() -> TempDir {
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

/// Record a PR number on an already-tracked branch (submit needs the network).
fn seed_pr(dir: &std::path::Path, branch: &str, number: u64) {
    let store = StateStore::new(Git::open(dir));
    let mut state = store.load().unwrap();
    state
        .branches
        .get_mut(branch)
        .expect("branch is tracked")
        .pr = Some(PullRequest { number, url: None });
    store.save(&state).unwrap();
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
fn log_color_flag_controls_ansi() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());

    let never = String::from_utf8_lossy(&stacc(p, &["log", "--color", "never"]).stdout).into_owned();
    assert!(!never.contains('\u{1b}'), "no ANSI with --color never: {never:?}");

    let always =
        String::from_utf8_lossy(&stacc(p, &["log", "--color", "always"]).stdout).into_owned();
    assert!(always.contains('\u{1b}'), "ANSI expected with --color always: {always:?}");
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

#[test]
fn log_reverse_and_stack_flags_apply() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success());
    // An unrelated sibling stack off the trunk.
    run_git(p, &["checkout", "-q", "main"]);
    run_git(p, &["checkout", "-q", "-b", "sib"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "s1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "b"]);

    // --reverse puts the trunk first.
    let rev = String::from_utf8_lossy(&stacc(p, &["log", "--reverse", "short"]).stdout).into_owned();
    let first = rev.lines().next().unwrap_or_default();
    assert!(first.contains("main"), "trunk should be first under --reverse: {rev}");

    // --stack from b scopes to b's line (a, b, trunk); the sibling is excluded.
    let scoped = String::from_utf8_lossy(&stacc(p, &["log", "--stack", "short"]).stdout).into_owned();
    assert!(scoped.contains("◉ b") && scoped.contains("○ a"), "got: {scoped}");
    assert!(!scoped.contains("sib"), "sibling stack should be excluded: {scoped}");

    // --steps with out-of-range values must never error.
    assert!(stacc(p, &["log", "--steps", "0"]).status.success());
    assert!(stacc(p, &["log", "--steps", "99"]).status.success());
}

#[test]
fn log_marks_a_tracked_branch_whose_git_ref_is_gone() {
    let tmp = repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "keep"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "keep1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "gone"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "gone1"]);
    assert!(stacc(p, &["track", "--base", "keep"]).status.success());
    // Delete the git branch but leave it tracked in stacc state.
    run_git(p, &["checkout", "-q", "keep"]);
    run_git(p, &["branch", "-D", "gone"]);

    let s = String::from_utf8_lossy(&stacc(p, &["log"]).stdout).into_owned();
    assert!(s.contains("gone (deleted)"), "deleted marker expected: {s}");
    assert!(!s.contains("gone1"), "a deleted branch shows no commit metadata: {s}");
    assert!(s.contains("keep1"), "live branches still render metadata: {s}");

    // The marker shows in the short form too.
    let short = String::from_utf8_lossy(&stacc(p, &["log", "short"]).stdout).into_owned();
    assert!(short.contains("gone (deleted)"), "short marks deleted too: {short}");

    // JSON flags it.
    let j = String::from_utf8_lossy(&stacc(p, &["log", "--format", "json"]).stdout).into_owned();
    assert!(j.contains(r#""deleted":true"#), "JSON deleted flag expected: {j}");
}

#[test]
fn log_full_shows_live_pr_status() {
    let tmp = github_repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "feat: x"]);
    assert!(stacc(p, &["track"]).status.success());
    seed_pr(p, "feature", 7);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls/7");
        then.status(200).json_body(serde_json::json!({
            "number": 7, "html_url": "u", "state": "open", "merged": false,
        }));
    });
    let base = server.base_url();
    let envs: &[(&str, &str)] = &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", base.as_str())];

    let out = stacc_env(p, &["log"], envs);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains("#7 Open"), "live PR status expected: {s}");

    let j = String::from_utf8_lossy(&stacc_env(p, &["log", "--format", "json"], envs).stdout)
        .into_owned();
    assert!(j.contains(r#""status":"open""#), "JSON status expected: {j}");
}

#[test]
fn log_pr_status_falls_back_on_error() {
    let tmp = github_repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "feat: x"]);
    assert!(stacc(p, &["track"]).status.success());
    seed_pr(p, "feature", 7);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls/7");
        then.status(500).json_body(serde_json::json!({ "message": "boom" }));
    });
    let base = server.base_url();
    let envs: &[(&str, &str)] = &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", base.as_str())];

    let out = stacc_env(p, &["log"], envs);
    assert!(out.status.success(), "must not be fatal: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains("#7"), "PR number still shown: {s}");
    assert!(!s.contains("#7 Open") && !s.contains("Merged"), "no status on error: {s}");
}

#[test]
fn log_no_status_makes_no_api_call() {
    let tmp = github_repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "feat: x"]);
    assert!(stacc(p, &["track"]).status.success());
    seed_pr(p, "feature", 7);

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls/7");
        then.status(200).json_body(serde_json::json!({
            "number": 7, "html_url": "u", "state": "open", "merged": false,
        }));
    });
    let base = server.base_url();
    let envs: &[(&str, &str)] = &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", base.as_str())];

    let s = String::from_utf8_lossy(&stacc_env(p, &["log", "--no-status"], envs).stdout).into_owned();
    assert!(s.contains("#7"), "PR number still shown: {s}");
    assert!(!s.contains("#7 Open"), "no live status under --no-status: {s}");
    mock.assert_hits(0); // --no-status must not hit the API at all
}

#[test]
fn log_pr_status_partial_failure_does_not_abort() {
    let tmp = github_repo();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "a"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "a1"]);
    assert!(stacc(p, &["track"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "b"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "b1"]);
    assert!(stacc(p, &["track", "--base", "a"]).status.success());
    seed_pr(p, "a", 7);
    seed_pr(p, "b", 8);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls/7");
        then.status(200).json_body(serde_json::json!({
            "number": 7, "html_url": "u", "state": "open", "merged": false,
        }));
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls/8");
        then.status(500).json_body(serde_json::json!({ "message": "boom" }));
    });
    let base = server.base_url();
    let envs: &[(&str, &str)] = &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", base.as_str())];

    let out = stacc_env(p, &["log"], envs);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains("#7 Open"), "the succeeding PR keeps its status: {s}");
    assert!(s.contains("#8") && !s.contains("#8 "), "the failing PR shows just its number: {s}");
}
