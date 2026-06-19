use std::process::{Command, Output};

use httpmock::MockServer;
use stacc_git::Git;
use stacc_state::{Base, BranchState, PullRequest, StateStore};
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

fn stacc_env(dir: &std::path::Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stacc"));
    cmd.current_dir(dir).args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn stacc")
}

fn stacc(dir: &std::path::Path, args: &[&str]) -> Output {
    stacc_env(dir, args, &[])
}

/// A repo whose `origin` fetch URL is a GitHub URL (so owner/repo parse), but
/// whose *push* URL points at a local bare repo (so `git push` works offline).
fn setup() -> (TempDir, TempDir) {
    let bare = TempDir::new().expect("bare dir");
    let status = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(bare.path())
        .status()
        .expect("init bare");
    assert!(status.success());

    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://github.com/TinyDogTech/stacc.git"],
    );
    run_git(
        tmp.path(),
        &["remote", "set-url", "--push", "origin", bare.path().to_str().unwrap()],
    );
    (tmp, bare)
}

fn pr_body(number: u64) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "html_url": format!("https://github.com/TinyDogTech/stacc/pull/{number}"),
        "state": "open",
        "merged": false,
    })
}

#[test]
fn submit_creates_pr_and_records_it() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(7));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""status":"created""#), "got: {s}");
    assert!(s.contains(r#""number":7"#), "got: {s}");
    mock.assert();

    // The PR number is now recorded in state.
    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&show.stdout).contains(r#""number": 7"#));
}

#[test]
fn submit_sends_description_as_body() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains("Custom description");
        then.status(201).json_body(pr_body(8));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--description", "Custom description", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_updates_existing_pr() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature"]);

    // Seed state as if feature was already submitted as PR #3.
    let store = StateStore::new(Git::open(tmp.path()));
    let mut state = store.load().unwrap();
    state.branches.insert(
        "feature".to_string(),
        BranchState {
            base: Base {
                name: "main".into(),
                hash: "deadbeef".into(),
            },
            pr: Some(PullRequest {
                number: 3,
                url: None,
            }),
            pr_title: None,
            pr_description: None,
        },
    );
    store.save(&state).unwrap();

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/3");
        then.status(200).json_body(pr_body(3));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""status":"updated""#), "got: {s}");
    mock.assert();
}

#[test]
fn submit_adopts_an_existing_pr_by_head() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());
    // No PR recorded in state, but an open PR with this head exists on GitHub
    // (created by gh/graphite before the stack migrated to stacc).

    let server = MockServer::start();
    let lookup = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls")
            .query_param("head", "TinyDogTech:feature")
            .query_param("state", "open");
        then.status(200).json_body(serde_json::json!([pr_body(55)]));
    });
    // The adopted PR takes the update path.
    let update = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/55");
        then.status(200).json_body(pr_body(55));
    });
    // Creating would duplicate the PR; this mock must never fire.
    let create = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(422)
            .json_body(serde_json::json!({ "message": "A pull request already exists" }));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""status":"updated""#), "adoption takes the update path: {s}");
    assert!(s.contains(r#""adopted":true"#), "got: {s}");
    assert!(s.contains(r#""number":55"#), "got: {s}");
    lookup.assert();
    update.assert();
    create.assert_hits(0);

    // The adopted number is recorded in state for the next submit/log.
    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&show.stdout).contains(r#""number": 55"#));

    // A second submit reads the recorded number: update again, no new lookup.
    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains(r#""adopted""#), "a recorded PR is a plain update: {s}");
    lookup.assert_hits(1);
}

#[test]
fn submit_reports_adoption_in_pretty_output() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/repos/TinyDogTech/stacc/pulls")
            .query_param("head", "TinyDogTech:feature");
        then.status(200).json_body(serde_json::json!([pr_body(56)]));
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/56");
        then.status(200).json_body(pr_body(56));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Adopted PR #56 for feature"), "got: {s}");
}

#[test]
fn submit_requires_tracked_branch() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);

    // Not tracked, no GitHub env needed, it should fail before any network.
    let out = stacc(tmp.path(), &["submit", "--format", "json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not tracked"), "got: {s}");
}

#[test]
fn submit_rejects_trunk() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    let out = stacc(tmp.path(), &["submit", "--format", "json"]); // on main
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("trunk"), "got: {s}");
}

#[test]
fn submit_walks_the_downstack() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    // 2-deep stack: main -> feature-1 -> feature-2 (the branch we submit from).
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-1"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-2"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f2"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature-1"]).status.success());

    let server = MockServer::start();
    // feature-1's PR, base must be the trunk.
    let mock_f1 = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""head":"feature-1""#)
            .body_contains(r#""base":"main""#);
        then.status(201).json_body(pr_body(11));
    });
    // feature-2's PR, base must be its parent, not the trunk.
    let mock_f2 = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""head":"feature-2""#)
            .body_contains(r#""base":"feature-1""#);
        then.status(201).json_body(pr_body(12));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""number":11"#), "got: {s}");
    assert!(s.contains(r#""number":12"#), "got: {s}");
    mock_f1.assert();
    mock_f2.assert();

    // Both PR numbers landed back in state.
    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature-1"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&show.stdout).contains(r#""number": 11"#));
    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature-2"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&show.stdout).contains(r#""number": 12"#));
}

#[test]
fn submit_stack_submits_the_whole_stack_downstack_first() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    // main -> feature-1 -> feature-2; submit from feature-1, so feature-2 is
    // only reachable through --stack.
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-1"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-2"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f2"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature-1"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "feature-1"]);

    let server = MockServer::start();
    let mock_f1 = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""head":"feature-1""#)
            .body_contains(r#""base":"main""#);
        then.status(201).json_body(pr_body(41));
    });
    let mock_f2 = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""head":"feature-2""#)
            .body_contains(r#""base":"feature-1""#);
        then.status(201).json_body(pr_body(42));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--stack", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    // Downstack-first: feature-1's PR is reported before feature-2's, so its
    // base ref existed on the remote when feature-2's PR opened.
    let f1 = s.find(r#""branch":"feature-1""#).expect("feature-1 in output");
    let f2 = s.find(r#""branch":"feature-2""#).expect("feature-2 in output");
    assert!(f1 < f2, "expected feature-1 before feature-2: {s}");
    mock_f1.assert();
    mock_f2.assert();
}

#[test]
fn submit_update_only_skips_branches_without_prs() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    // main -> feature-1 (has PR #3) -> feature-2 (no PR); submit from feature-2.
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-1"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-2"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f2"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature-1"]).status.success());

    let store = StateStore::new(Git::open(tmp.path()));
    let mut state = store.load().unwrap();
    state.branches.get_mut("feature-1").unwrap().pr = Some(PullRequest {
        number: 3,
        url: None,
    });
    store.save(&state).unwrap();

    let server = MockServer::start();
    // Only feature-1's existing PR is touched; no POST mock exists, so any
    // attempt to create a PR would 404 and fail the command.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/3");
        then.status(200).json_body(pr_body(3));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--update-only", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""status":"updated""#), "got: {s}");
    assert!(s.contains(r#""skipped":["feature-2"]"#), "got: {s}");
    mock.assert();
}

#[test]
fn submit_draft_creates_a_draft_pr() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""draft":true"#);
        then.status(201).json_body(pr_body(9));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--draft", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_without_draft_sends_draft_false() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""draft":false"#);
        then.status(201).json_body(pr_body(10));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_re_pushes_a_rebased_branch_via_lease() {
    // Plain push would refuse a non-fast-forward after a rebase. The lease
    // push accepts it because the local remote-tracking ref still matches the
    // remote's tip, we're the ones who put it there.
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(31));
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/31");
        then.status(200).json_body(pr_body(31));
    });

    // First submit, creates the PR and lands `feature` on the bare remote.
    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    // Amend the commit, rewrites history, so a plain push would now be
    // rejected as non-fast-forward.
    run_git(
        tmp.path(),
        &["commit", "-q", "--allow-empty", "--amend", "-m", "Add feature, revised"],
    );

    // Re-submit, lease push lets the rewritten ref overwrite the old tip.
    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""status":"updated""#), "got: {s}");
}

#[test]
fn submit_description_applies_only_to_current_branch() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-1"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-2"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f2"]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature-1"]).status.success());

    let server = MockServer::start();
    // feature-1 (ancestor), body must stay empty. The matcher requires the
    // literal JSON fragment `"body":""`, so if --description ever leaked to
    // feature-1 the matcher would miss, the POST would 404, and submit would
    // fail. That's the negative assertion.
    let mock_f1 = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""head":"feature-1""#)
            .body_contains(r#""body":"""#);
        then.status(201).json_body(pr_body(21));
    });
    // feature-2 (the current branch), body MUST carry the description.
    let mock_f2 = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""head":"feature-2""#)
            .body_contains("Top branch description");
        then.status(201).json_body(pr_body(22));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--description", "Top branch description", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock_f1.assert();
    mock_f2.assert();
}

#[test]
fn submit_title_flag_sends_custom_title() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Commit subject"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    // The mock requires the custom title, not the commit subject.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""title":"Custom PR Title""#);
        then.status(201).json_body(pr_body(70));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--title", "Custom PR Title", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_title_persists_to_state() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Commit subject"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(71));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--title", "Stored Title", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature"])
        .output()
        .unwrap();
    let blob = String::from_utf8_lossy(&show.stdout);
    assert!(blob.contains(r#""pr_title": "Stored Title""#), "pr_title in state blob: {blob}");
}

#[test]
fn submit_persisted_title_survives_resubmit() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Commit subject"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(72));
    });
    // The re-submit update must carry the stored title, not the commit subject.
    let resubmit_mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/72")
            .body_contains(r#""title":"Stored Title""#);
        then.status(200).json_body(pr_body(72));
    });

    // First submit: sets --title and persists it.
    let out = stacc_env(
        tmp.path(),
        &["submit", "--title", "Stored Title", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "first submit stderr: {}", String::from_utf8_lossy(&out.stderr));

    // Second submit: no --title flag; stored title must be used.
    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "resubmit stderr: {}", String::from_utf8_lossy(&out.stderr));
    resubmit_mock.assert();
}

#[test]
fn submit_description_persists_to_state() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Commit subject"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(73));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--description", "Stored body", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature"])
        .output()
        .unwrap();
    let blob = String::from_utf8_lossy(&show.stdout);
    assert!(blob.contains(r#""pr_description": "Stored body""#), "pr_description in state blob: {blob}");
}

#[test]
fn submit_persisted_description_survives_resubmit() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Commit subject"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(74));
    });
    let resubmit_mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/74")
            .body_contains("Stored body");
        then.status(200).json_body(pr_body(74));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--description", "Stored body", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "first submit stderr: {}", String::from_utf8_lossy(&out.stderr));

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "resubmit stderr: {}", String::from_utf8_lossy(&out.stderr));
    resubmit_mock.assert();
}

// F3: the GitHub-only boundary. In forge-less or local mode, `submit` is
// unavailable with a forge-generic message, a non-zero exit, no crash, and never
// a raw remote URL (R9/R10). Asserted on `--format json` so the single-line,
// unwrapped message is matched verbatim.

#[test]
fn submit_on_a_non_github_remote_is_unavailable_with_a_forge_generic_message() {
    let (tmp, _bare) = setup();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    // Repoint origin at a non-GitHub forge. The boundary refuses before any
    // network, so the dead URL is never contacted.
    run_git(p, &["remote", "set-url", "origin", "https://gitlab.com/acme/widgets.git"]);
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(p, &["track"]).status.success());

    let out = stacc(p, &["submit", "--format", "json"]);
    assert!(!out.status.success(), "submit on a non-GitHub remote is unavailable");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""type":"usage""#), "usage error, not a crash: {s}");
    assert!(s.contains("open a change through your forge"), "forge-generic guidance: {s}");
    assert!(s.contains("origin"), "names the remote: {s}");
    // No forge detection and no raw remote URL: neither the host nor the path leak.
    assert!(!s.contains("gitlab.com"), "no raw remote URL: {s}");
    assert!(!s.contains("widgets"), "no raw remote URL: {s}");
}

#[test]
fn submit_in_local_mode_is_unavailable_even_on_a_github_remote() {
    let (tmp, _bare) = setup();
    let p = tmp.path();
    assert!(stacc(p, &["init"]).status.success());
    run_git(p, &["checkout", "-q", "-b", "feature"]);
    run_git(p, &["commit", "-q", "--allow-empty", "-m", "Add feature"]);
    assert!(stacc(p, &["track"]).status.success());
    // Opting into local mode makes submit unavailable even though origin is a
    // github.com URL with a working push target.
    assert!(stacc(p, &["config", "set", "local", "true"]).status.success());

    let out = stacc(p, &["submit", "--format", "json"]);
    assert!(!out.status.success(), "local mode makes submit unavailable");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("local mode is on"), "names local mode: {s}");
    assert!(s.contains("open a change through your forge"), "forge-generic guidance: {s}");
}

// STA-121: reflow hard-wrapped commit bodies so they render as paragraphs on GitHub.

#[test]
fn submit_reflowed_wrapped_commit_body_on_create() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    // Commit with a conventionally-wrapped body.
    run_git(
        tmp.path(),
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Add feature\n\nThis is the first sentence of the body\nwrapped before seventy-two columns.",
        ],
    );
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    // The POST body must contain the reflowed paragraph as a single line.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(
                "This is the first sentence of the body wrapped before seventy-two columns.",
            );
        then.status(201).json_body(pr_body(80));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_reflowed_multi_paragraph_commit_body() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(
        tmp.path(),
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Add feature\n\nFirst paragraph line one\nfirst paragraph line two.\n\nSecond paragraph line one\nsecond paragraph line two.",
        ],
    );
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains("First paragraph line one first paragraph line two.")
            .body_contains("Second paragraph line one second paragraph line two.");
        then.status(201).json_body(pr_body(81));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_description_flag_bypasses_reflow() {
    // Text passed via --description is sent as-is, not reflowed.
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(
        tmp.path(),
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Add feature\n\nWrapped body that\nwould be reflowed.",
        ],
    );
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    // The POST body must contain the literal --description text, not the reflowed commit body.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains("My explicit description");
        then.status(201).json_body(pr_body(82));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--description", "My explicit description", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

// STA-121: re-submit without --description and no stored_desc must omit the body
// field in the PATCH so GitHub preserves any manual PR body edits.

#[test]
fn submit_resubmit_without_description_omits_body_in_patch() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Preserve body test"]);
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    // First submit: CREATE. No body constraint; just let it succeed.
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(90));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "first submit stderr: {}", String::from_utf8_lossy(&out.stderr));

    // Second submit: UPDATE. No --description, no stored_desc.
    // The PATCH body must be exactly {"title":"Preserve body test","base":"main"}
    // -- the "body" key must be absent so the manual PR body is not overwritten.
    let resubmit_mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/90")
            .body(r#"{"title":"Preserve body test","base":"main"}"#);
        then.status(200).json_body(pr_body(90));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--format", "json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "resubmit stderr: {}", String::from_utf8_lossy(&out.stderr));
    resubmit_mock.assert();
}
