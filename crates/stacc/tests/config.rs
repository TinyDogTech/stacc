//! Integration tests for `stacc config`: get/set/unset/list over the
//! repo-local `.stacc.toml` and the user-global config file. File-level only,
//! so nothing here runs `stacc init`.

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

/// Run stacc in `dir` with HOME pointed at `home`, so the user-global config
/// (`$HOME/.config/stacc/config.toml`) is test-owned and can't leak.
fn stacc_with_home(dir: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .current_dir(dir)
        .env("HOME", home)
        .args(args)
        .output()
        .expect("spawn stacc")
}

fn stacc(dir: &std::path::Path, args: &[&str]) -> Output {
    stacc_with_home(dir, dir, args)
}

/// A git repo with a `main` trunk and no remote. NOT stacc-initialized.
fn repo() -> TempDir {
    let tmp = TempDir::new().expect("temp dir");
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
    run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
    tmp
}

fn json(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("bad json ({e}): {stdout}"))
}

#[test]
fn set_then_get_round_trips_through_the_repo_file() {
    let tmp = repo();

    let out = stacc(tmp.path(), &["config", "set", "trunk", "develop", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["status"], "set");
    assert_eq!(v["key"], "trunk");
    assert_eq!(v["value"], "develop");
    assert_eq!(v["file"], ".stacc.toml");

    // The value landed in the repo-local file...
    let text = std::fs::read_to_string(tmp.path().join(".stacc.toml")).unwrap();
    assert!(text.contains("trunk = \"develop\""), "got: {text}");

    // ...and get resolves it with source repo.
    let out = stacc(tmp.path(), &["config", "get", "trunk", "--format", "json"]);
    assert!(out.status.success());
    let v = json(&out);
    assert_eq!(v["value"], "develop");
    assert_eq!(v["source"], "repo");
}

#[test]
fn repo_value_overrides_global_and_list_reports_both_sources() {
    let tmp = repo();

    // trunk in both files (repo must win); remote only globally.
    assert!(stacc(tmp.path(), &["config", "set", "trunk", "global-trunk", "--global"]).status.success());
    assert!(stacc(tmp.path(), &["config", "set", "trunk", "repo-trunk"]).status.success());
    assert!(stacc(tmp.path(), &["config", "set", "remote", "upstream", "--global"]).status.success());

    let out = stacc(tmp.path(), &["config", "get", "trunk", "--format", "json"]);
    let v = json(&out);
    assert_eq!(v["value"], "repo-trunk");
    assert_eq!(v["source"], "repo");

    let out = stacc(tmp.path(), &["config", "list", "--format", "json"]);
    assert!(out.status.success());
    let v = json(&out);
    assert_eq!(v["op"], "config");
    assert_eq!(v["values"]["trunk"]["value"], "repo-trunk");
    assert_eq!(v["values"]["trunk"]["source"], "repo");
    assert_eq!(v["values"]["remote"]["value"], "upstream");
    assert_eq!(v["values"]["remote"]["source"], "global");
    // Shipped default aliases are listed with source default.
    assert_eq!(v["aliases"]["co"]["value"], "checkout");
    assert_eq!(v["aliases"]["co"]["source"], "default");
}

#[test]
fn detected_values_resolve_when_no_file_sets_them() {
    let tmp = repo();
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://example.com/r.git"],
    );

    let out = stacc(tmp.path(), &["config", "get", "trunk", "--format", "json"]);
    assert!(out.status.success());
    let v = json(&out);
    assert_eq!(v["value"], "main");
    assert_eq!(v["source"], "detected");

    let out = stacc(tmp.path(), &["config", "get", "remote", "--format", "json"]);
    let v = json(&out);
    assert_eq!(v["value"], "origin");
    assert_eq!(v["source"], "detected");
}

#[test]
fn unknown_key_is_a_structured_error_naming_the_valid_keys() {
    let tmp = repo();
    for sub in [
        vec!["config", "get", "bogus"],
        vec!["config", "set", "bogus", "x"],
        vec!["config", "unset", "bogus"],
    ] {
        let mut args = sub.clone();
        args.extend(["--format", "json"]);
        let out = stacc(tmp.path(), &args);
        assert!(!out.status.success(), "{sub:?} should fail");
        let v = json(&out);
        assert_eq!(v["error"], "usage", "{sub:?}");
        let msg = v["message"].as_str().unwrap();
        assert!(msg.contains("unknown config key `bogus`"), "got: {msg}");
        assert!(msg.contains("trunk"), "got: {msg}");
        assert!(msg.contains("remote"), "got: {msg}");
        assert!(msg.contains("aliases.<name>"), "got: {msg}");
    }
}

#[test]
fn get_of_an_unset_key_is_null_and_exits_zero() {
    let tmp = repo(); // no remote, no files: remote is unset everywhere

    let out = stacc(tmp.path(), &["config", "get", "remote", "--format", "json"]);
    assert!(out.status.success());
    let v = json(&out);
    assert!(v["value"].is_null(), "got: {v}");
    assert!(v["source"].is_null(), "got: {v}");

    // Pretty form prints "unset", still exit 0.
    let out = stacc(tmp.path(), &["config", "get", "remote"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "unset");
}

#[test]
fn set_alias_is_picked_up_by_the_alias_loader() {
    let tmp = repo();

    let out = stacc(tmp.path(), &["config", "set", "aliases.statlog", "log", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    // `statlog` -> `log`, which errors "not initialized" on this repo: the
    // alias expanded through the real startup loader and dispatched.
    let out = stacc(tmp.path(), &["statlog", "--format", "json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not initialized"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // And the config surface resolves it.
    let out = stacc(tmp.path(), &["config", "get", "aliases.statlog", "--format", "json"]);
    let v = json(&out);
    assert_eq!(v["value"], "log");
    assert_eq!(v["source"], "repo");
}

#[test]
fn unset_removes_the_key() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["config", "set", "remote", "upstream"]).status.success());
    assert!(stacc(tmp.path(), &["config", "set", "aliases.x", "log"]).status.success());

    let out = stacc(tmp.path(), &["config", "unset", "remote", "--format", "json"]);
    assert!(out.status.success());
    let v = json(&out);
    assert_eq!(v["status"], "unset");
    assert_eq!(v["key"], "remote");

    let out = stacc(tmp.path(), &["config", "get", "remote", "--format", "json"]);
    assert!(out.status.success());
    assert!(json(&out)["value"].is_null());

    // Unsetting the last alias also drops the empty [aliases] table.
    assert!(stacc(tmp.path(), &["config", "unset", "aliases.x"]).status.success());
    let text = std::fs::read_to_string(tmp.path().join(".stacc.toml")).unwrap();
    assert!(!text.contains("[aliases]"), "got: {text}");
}

#[test]
fn global_ops_and_list_work_outside_a_git_repo() {
    // A plain directory: no git repo, no stacc state.
    let tmp = TempDir::new().expect("temp dir");

    let out = stacc(tmp.path(), &["config", "set", "trunk", "main", "--global", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(tmp.path().join(".config/stacc/config.toml").exists());

    let out = stacc(tmp.path(), &["config", "get", "trunk", "--format", "json"]);
    assert!(out.status.success());
    let v = json(&out);
    assert_eq!(v["value"], "main");
    assert_eq!(v["source"], "global");

    let out = stacc(tmp.path(), &["config", "list", "--format", "json"]);
    assert!(out.status.success());
    let v = json(&out);
    assert_eq!(v["values"]["trunk"]["source"], "global");
    assert!(v["values"]["remote"]["value"].is_null());

    let out = stacc(tmp.path(), &["config", "unset", "trunk", "--global", "--format", "json"]);
    assert!(out.status.success());
    let out = stacc(tmp.path(), &["config", "get", "trunk", "--format", "json"]);
    assert!(out.status.success());
    assert!(json(&out)["value"].is_null());
}

#[test]
fn set_without_a_value_is_a_clap_usage_error() {
    let tmp = repo();
    let out = stacc(tmp.path(), &["config", "set", "trunk"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("required"), "got: {stderr}");
}

#[test]
fn pretty_list_is_an_aligned_table_with_sources() {
    let tmp = repo();
    assert!(stacc(tmp.path(), &["config", "set", "trunk", "develop"]).status.success());

    let out = stacc(tmp.path(), &["config", "list"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trunk_line = stdout
        .lines()
        .find(|l| l.starts_with("trunk"))
        .unwrap_or_else(|| panic!("no trunk row in: {stdout}"));
    assert!(trunk_line.contains("develop"), "got: {trunk_line}");
    assert!(trunk_line.ends_with("repo"), "got: {trunk_line}");
    // Shipped aliases appear as aliases.<name> rows.
    assert!(stdout.contains("aliases.co"), "got: {stdout}");
}
