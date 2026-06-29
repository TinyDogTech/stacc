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
        &["submit", "--json"],
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
        &["submit", "--description", "Custom description", "--json"],
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
        &["submit", "--json"],
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
        &["submit", "--json"],
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
        &["submit", "--json"],
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
    let out = stacc(tmp.path(), &["submit", "--json"]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("not tracked"), "got: {s}");
}

#[test]
fn submit_rejects_trunk() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    let out = stacc(tmp.path(), &["submit", "--json"]); // on main
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
        &["submit", "--json"],
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
        &["submit", "--stack", "--json"],
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
        &["submit", "--update-only", "--json"],
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
        &["submit", "--draft", "--json"],
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
        &["submit", "--json"],
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
        &["submit", "--json"],
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
        &["submit", "--json"],
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
        &["submit", "--description", "Top branch description", "--json"],
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
        &["submit", "--title", "Custom PR Title", "--json"],
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
        &["submit", "--title", "Stored Title", "--json"],
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
        &["submit", "--title", "Stored Title", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "first submit stderr: {}", String::from_utf8_lossy(&out.stderr));

    // Second submit: no --title flag; stored title must be used.
    let out = stacc_env(
        tmp.path(),
        &["submit", "--json"],
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
        &["submit", "--description", "Stored body", "--json"],
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
        &["submit", "--description", "Stored body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "first submit stderr: {}", String::from_utf8_lossy(&out.stderr));

    let out = stacc_env(
        tmp.path(),
        &["submit", "--json"],
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

    let out = stacc(p, &["submit", "--json"]);
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

    let out = stacc(p, &["submit", "--json"]);
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
        &["submit", "--json"],
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
        &["submit", "--json"],
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
        &["submit", "--description", "My explicit description", "--json"],
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
        &["submit", "--json"],
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
        &["submit", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "resubmit stderr: {}", String::from_utf8_lossy(&out.stderr));
    resubmit_mock.assert();
}

// STA-131: `--update-body` refreshes the current branch's PR body from the commit
// body on update, overriding a stale stored description, then clears the stored
// description so the no-clobber default resumes on the next plain submit.

/// Seed a tracked, already-submitted `feature` branch (PR `number`) with the given
/// stored description and a commit whose body is `commit_body`.
fn seed_submitted_feature(
    tmp: &std::path::Path,
    number: u64,
    stored_desc: Option<&str>,
    commit_message: &str,
) {
    assert!(stacc(tmp, &["init"]).status.success());
    run_git(tmp, &["checkout", "-q", "-b", "feature"]);
    run_git(tmp, &["commit", "-q", "--allow-empty", "-m", commit_message]);

    let store = StateStore::new(Git::open(tmp));
    let mut state = store.load().unwrap();
    state.branches.insert(
        "feature".to_string(),
        BranchState {
            base: Base {
                name: "main".into(),
                hash: "deadbeef".into(),
            },
            pr: Some(PullRequest { number, url: None }),
            pr_title: None,
            pr_description: stored_desc.map(str::to_string),
        },
    );
    store.save(&state).unwrap();
}

#[test]
fn submit_update_body_refreshes_from_commit_body_when_no_stored_desc() {
    let (tmp, _bare) = setup();
    seed_submitted_feature(
        tmp.path(),
        40,
        None,
        "Add feature\n\nFresh body line one\nwrapped line two.",
    );

    let server = MockServer::start();
    // The PATCH must carry the reflowed commit body; a missing/omitted body would
    // not match, failing the request and the success assertion below.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/40")
            .body_contains("Fresh body line one wrapped line two.");
        then.status(200).json_body(pr_body(40));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--update-body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_update_body_overrides_a_stale_stored_description() {
    let (tmp, _bare) = setup();
    seed_submitted_feature(
        tmp.path(),
        41,
        Some("Stale stored body"),
        "Add feature\n\nCurrent commit body.",
    );

    let server = MockServer::start();
    // Only a request carrying the commit body matches; if submit sent the stale
    // stored description instead, no mock matches and the request fails.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/41")
            .body_contains("Current commit body.");
        then.status(200).json_body(pr_body(41));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--update-body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_update_body_is_one_shot_and_clears_stored_description() {
    let (tmp, _bare) = setup();
    seed_submitted_feature(
        tmp.path(),
        42,
        Some("Old stored body"),
        "Refresh me\n\nCurrent commit body.",
    );

    let server = MockServer::start();
    // First submit with --update-body: refresh from the commit body.
    let refresh_mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/42")
            .body_contains("Current commit body.");
        then.status(200).json_body(pr_body(42));
    });
    // Second, plain submit: the stored description was cleared, so the body field
    // is omitted entirely and GitHub keeps whatever the refresh wrote.
    let plain_mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/42")
            .body(r#"{"title":"Refresh me","base":"main"}"#);
        then.status(200).json_body(pr_body(42));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--update-body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "first submit stderr: {}", String::from_utf8_lossy(&out.stderr));
    refresh_mock.assert();

    // The refresh cleared the stored description in state (one-shot, not a stored mode).
    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature"])
        .output()
        .unwrap();
    let blob = String::from_utf8_lossy(&show.stdout);
    assert!(
        !blob.contains("Old stored body"),
        "stored description cleared after --update-body refresh: {blob}"
    );

    let out = stacc_env(
        tmp.path(),
        &["submit", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "second submit stderr: {}", String::from_utf8_lossy(&out.stderr));
    plain_mock.assert();
}

#[test]
fn submit_description_takes_precedence_over_update_body() {
    let (tmp, _bare) = setup();
    seed_submitted_feature(
        tmp.path(),
        43,
        None,
        "Add feature\n\nCommit body that must not win.",
    );

    let server = MockServer::start();
    // --description wins; the explicit text is sent, not the commit body.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/43")
            .body(r#"{"title":"Add feature","body":"Explicit description","base":"main"}"#);
        then.status(200).json_body(pr_body(43));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--description", "Explicit description", "--update-body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();

    // The explicit description persists (it was not cleared by --update-body).
    let show = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["show", "refs/stacc/data:branches/feature"])
        .output()
        .unwrap();
    let blob = String::from_utf8_lossy(&show.stdout);
    assert!(
        blob.contains(r#""pr_description": "Explicit description""#),
        "explicit description persists: {blob}"
    );
}

#[test]
fn submit_update_body_is_a_noop_on_create() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(
        tmp.path(),
        &["commit", "-q", "--allow-empty", "-m", "Create subject\n\nCreate body text."],
    );
    assert!(stacc(tmp.path(), &["track"]).status.success());

    let server = MockServer::start();
    // On first submit there is no PR yet, so --update-body changes nothing: the new
    // PR still takes the commit body through the normal create cascade.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains("Create body text.");
        then.status(201).json_body(pr_body(44));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--update-body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_update_body_skips_an_empty_commit_body() {
    let (tmp, _bare) = setup();
    // Subject-only commit: there is no body to sync from.
    seed_submitted_feature(tmp.path(), 45, None, "Subject only");

    let server = MockServer::start();
    // With nothing to refresh, --update-body falls through to the default (here, omit)
    // rather than blanking an existing PR body to "". The PATCH carries no body field.
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/45")
            .body(r#"{"title":"Subject only","base":"main"}"#);
        then.status(200).json_body(pr_body(45));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--update-body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    mock.assert();
}

#[test]
fn submit_update_body_on_create_preserves_a_stored_description() {
    // Reachable state (e.g. after `stacc rename` nulls the PR but keeps the stored
    // description): a branch with a stored description but no PR yet. `--update-body`
    // must be a true no-op on create -- it must not clear the stored description.
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "Add feature\n\nCommit body."]);

    let store = StateStore::new(Git::open(tmp.path()));
    let mut state = store.load().unwrap();
    state.branches.insert(
        "feature".to_string(),
        BranchState {
            base: Base {
                name: "main".into(),
                hash: "deadbeef".into(),
            },
            pr: None,
            pr_title: None,
            pr_description: Some("Retained description".to_string()),
        },
    );
    store.save(&state).unwrap();

    let server = MockServer::start();
    // No PR exists, so submit creates one (POST). --update-body has no update path to
    // act on and must leave the stored description untouched.
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls");
        then.status(201).json_body(pr_body(46));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--update-body", "--json"],
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
    assert!(
        blob.contains(r#""pr_description": "Retained description""#),
        "stored description must survive --update-body on create: {blob}"
    );
}

#[test]
fn submit_update_body_applies_to_current_branch_only() {
    let (tmp, _bare) = setup();
    assert!(stacc(tmp.path(), &["init"]).status.success());

    // main -> feature-1 -> feature-2 (current). Give each a distinct commit body.
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-1"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f1\n\nF1 commit body."]);
    assert!(stacc(tmp.path(), &["track"]).status.success());
    run_git(tmp.path(), &["checkout", "-q", "-b", "feature-2"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "f2\n\nF2 commit body."]);
    assert!(stacc(tmp.path(), &["track", "--base", "feature-1"]).status.success());

    let server = MockServer::start();
    // First submit: create both PRs.
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""head":"feature-1""#);
        then.status(201).json_body(pr_body(51));
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/repos/TinyDogTech/stacc/pulls")
            .body_contains(r#""head":"feature-2""#);
        then.status(201).json_body(pr_body(52));
    });
    let out = stacc_env(
        tmp.path(),
        &["submit", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "create stderr: {}", String::from_utf8_lossy(&out.stderr));

    // Re-submit from feature-2 with --update-body. Only feature-2 (current) refreshes
    // its body; feature-1 (downstack) omits the body field.
    let f2_refresh = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/52")
            .body_contains("F2 commit body.");
        then.status(200).json_body(pr_body(52));
    });
    let f1_untouched = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path("/repos/TinyDogTech/stacc/pulls/51")
            .body(r#"{"title":"f1","base":"main"}"#);
        then.status(200).json_body(pr_body(51));
    });

    let out = stacc_env(
        tmp.path(),
        &["submit", "--update-body", "--json"],
        &[("GITHUB_TOKEN", "x"), ("GITHUB_API_URL", &server.base_url())],
    );
    assert!(out.status.success(), "update stderr: {}", String::from_utf8_lossy(&out.stderr));
    f2_refresh.assert();
    f1_untouched.assert();
}
