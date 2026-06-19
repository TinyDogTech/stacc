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

fn write_commit(dir: &std::path::Path, file: &str, contents: &str, msg: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run_git(dir, &["add", file]);
    run_git(dir, &["commit", "-q", "-m", msg]);
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

/// Run `stacc <args> --format json` expecting success, parsed.
fn info_json(dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
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

/// An initialized repo with a tracked `feature` branch carrying one commit.
fn tracked_feature() -> TempDir {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());
    write_commit(tmp.path(), "f.txt", "one\ntwo\n", "feat: add f");
    tmp
}

/// Record a PR on an already-tracked branch (submit needs the network).
fn seed_pr(dir: &std::path::Path, branch: &str, number: u64, url: Option<&str>) {
    let store = StateStore::new(Git::open(dir));
    let mut state = store.load().unwrap();
    state.branches.get_mut(branch).expect("branch is tracked").pr = Some(PullRequest {
        number,
        url: url.map(ToString::to_string),
    });
    store.save(&state).unwrap();
}

#[test]
fn info_reports_the_tracked_branch_shape() {
    let tmp = tracked_feature();
    let head = Git::open(tmp.path()).rev_parse("feature").unwrap();

    let v = info_json(tmp.path(), &["info"]);
    assert_eq!(v["branch"], "feature");
    assert_eq!(v["tracked"], true);
    assert_eq!(v["base"]["name"], "main");
    assert_eq!(v["base"]["hash"].as_str().unwrap().len(), 40, "recorded base hash");
    assert_eq!(v["head"].as_str().unwrap(), head);
    assert_eq!(v["parent"], "main");
    // No children: the empty array is omitted (compacted), not emitted as `[]`.
    assert!(v.get("children").is_none(), "empty children omitted: {v}");
    assert_eq!(v["needs_restack"], false);
    assert_eq!(v["commits"], 1);
    assert_eq!(v["commit"]["subject"], "feat: add f");
    assert_eq!(v["diffstat"]["files"], 1);
    assert_eq!(v["diffstat"]["insertions"], 2);
    assert_eq!(v["diffstat"]["deletions"], 0);
    assert!(v["change"].is_null());
    // Heavy fields are absent without their flags.
    assert!(v.get("diff").is_none(), "got: {v}");
    assert!(v.get("patch").is_none(), "got: {v}");
    assert!(v.get("change_fetch").is_none(), "got: {v}");
}

#[test]
fn info_flags_needs_restack_after_the_base_is_amended() {
    let tmp = repo();
    // Give main a second commit so amending it keeps a shared root with feature.
    write_commit(tmp.path(), "m.txt", "m1\n", "main second");
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());
    write_commit(tmp.path(), "f.txt", "f\n", "feat: add f");

    let v = info_json(tmp.path(), &["info"]);
    assert_eq!(v["needs_restack"], false);

    // Amend the base under the branch: feature no longer descends main's tip.
    run_git(tmp.path(), &["checkout", "-q", "main"]);
    run_git(tmp.path(), &["commit", "-q", "--amend", "-m", "main amended"]);

    let v = info_json(tmp.path(), &["info", "feature"]);
    assert_eq!(v["needs_restack"], true);
}

#[test]
fn info_accepts_an_explicit_branch_from_elsewhere() {
    let tmp = tracked_feature();
    run_git(tmp.path(), &["checkout", "-q", "main"]);

    let v = info_json(tmp.path(), &["info", "feature"]);
    assert_eq!(v["branch"], "feature");
    assert_eq!(v["base"]["name"], "main");
    assert_eq!(v["needs_restack"], false);
}

#[test]
fn info_on_the_trunk_is_a_structured_result() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    let v = info_json(tmp.path(), &["info"]);
    assert_eq!(v["branch"], "main");
    assert_eq!(v["trunk"], true);
}

#[test]
fn info_on_an_untracked_branch_is_a_structured_result() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "loose"]);

    let v = info_json(tmp.path(), &["info"]);
    assert_eq!(v["branch"], "loose");
    assert_eq!(v["tracked"], false);
}

#[test]
fn info_diff_flag_includes_the_diff_text() {
    let tmp = tracked_feature();

    let with = info_json(tmp.path(), &["info", "--diff"]);
    let diff = with["diff"].as_str().expect("diff is a string");
    assert!(diff.contains("+one") && diff.contains("+two"), "got: {diff}");

    let without = info_json(tmp.path(), &["info"]);
    assert!(without.get("diff").is_none(), "got: {without}");
}

#[test]
fn info_patch_flag_includes_per_commit_patches() {
    let tmp = tracked_feature();
    write_commit(tmp.path(), "g.txt", "g\n", "feat: add g");

    let v = info_json(tmp.path(), &["info", "--patch"]);
    let patch = v["patch"].as_str().expect("patch is a string");
    // Both commits appear, message and body.
    assert!(patch.contains("feat: add f"), "got: {patch}");
    assert!(patch.contains("feat: add g"), "got: {patch}");
    assert!(patch.contains("+two"), "got: {patch}");
    assert_eq!(v["commits"], 2);
}

#[test]
fn info_lists_children_in_name_order() {
    let tmp = tracked_feature();
    // Create the fork in reverse name order; the listing is still sorted.
    run_git(tmp.path(), &["checkout", "-q", "-b", "kid-b"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "feature"]);
    run_git(tmp.path(), &["checkout", "-q", "-b", "kid-a"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature"]).status.success());

    let v = info_json(tmp.path(), &["info", "feature"]);
    let children: Vec<&str> = v["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(children, ["kid-a", "kid-b"]);
}

#[test]
fn info_reports_the_recorded_pr() {
    let tmp = tracked_feature();
    let url = "https://github.com/TinyDogTech/stacc/pull/5";
    seed_pr(tmp.path(), "feature", 5, Some(url));

    let v = info_json(tmp.path(), &["info"]);
    assert_eq!(v["change"]["number"], 5);
    assert_eq!(v["change"]["url"], url);
    // Without --body there is no fetch and no body fields.
    assert!(v["change"].get("body").is_none(), "got: {v}");
    assert!(v.get("change_fetch").is_none(), "got: {v}");
}

#[test]
fn info_body_fetches_the_pr_title_state_and_body() {
    let tmp = tracked_feature();
    seed_pr(tmp.path(), "feature", 5, None);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls/5");
        then.status(200).json_body(serde_json::json!({
            "number": 5, "html_url": "u", "state": "open", "merged": false,
            "title": "feat: add f", "body": "the body",
            "draft": true, "mergeable_state": "behind",
        }));
    });

    let out = stacc_env(
        tmp.path(),
        &["info", "--body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["change"]["number"], 5);
    assert_eq!(v["change"]["title"], "feat: add f");
    assert_eq!(v["change"]["state"], "open");
    assert_eq!(v["change"]["body"], "the body");
    assert_eq!(v["change"]["draft"], true);
    assert_eq!(v["change"]["readiness"], "behind");
    assert_eq!(v["change_fetch"], "ok");

    // The pretty form tags the same live facts on the PR line.
    let out = stacc_env(
        tmp.path(),
        &["info", "--body"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.contains("(open, draft, behind)"), "got: {s}");
}

#[test]
fn info_body_fetch_failure_is_not_fatal() {
    let tmp = tracked_feature();
    seed_pr(tmp.path(), "feature", 5, None);

    // No mock server: the API URL points at a dead port, so the fetch fails.
    let out = stacc_env(
        tmp.path(),
        &["info", "--body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", "http://127.0.0.1:1")],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // The recorded fields survive; only the fetch is marked failed.
    assert_eq!(v["change"]["number"], 5);
    assert!(v["change"].get("body").is_none(), "got: {v}");
    assert_eq!(v["change_fetch"], "failed");
}

#[test]
fn info_pretty_renders_the_compact_layout() {
    let tmp = tracked_feature();
    run_git(tmp.path(), &["checkout", "-q", "-b", "kid-a"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "feature"]);

    let out = stacc(tmp.path(), &["info"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("feature (base: main)"), "got: {s}");
    assert!(s.contains("Parent:   main"), "got: {s}");
    assert!(s.contains("Children: kid-a"), "got: {s}");
    assert!(s.contains("feat: add f"), "got: {s}");
    assert!(s.contains("Diffstat: 1 file changed, +2 -0"), "got: {s}");
    assert!(!s.contains("needs restack"), "got: {s}");
}
