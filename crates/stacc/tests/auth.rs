use std::process::{Command, Output};

/// Run `stacc auth login --format json` with `STACC_OAUTH_CLIENT_ID` controlled:
/// `None` clears it (so the build's placeholder client ID applies), `Some(id)`
/// sets it. The device-flow URLs are never reached on the fail-fast path, so no
/// mock server is needed.
fn login(client_id: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stacc"));
    cmd.args(["auth", "login", "--format", "json"]);
    cmd.env_remove("STACC_OAUTH_CLIENT_ID");
    if let Some(id) = client_id {
        cmd.env("STACC_OAUTH_CLIENT_ID", id);
    }
    cmd.output().expect("spawn stacc")
}

#[test]
fn login_fails_fast_on_the_placeholder_client_id() {
    let out = login(None);
    assert!(!out.status.success(), "login must fail without a registered app");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""type":"usage""#), "usage error: {s}");
    // The message points at the working alternatives.
    assert!(s.contains("gh") && s.contains("GITHUB_TOKEN"), "names alternatives: {s}");
}

#[test]
fn login_fails_fast_on_an_empty_client_id_override() {
    // `STACC_OAUTH_CLIENT_ID=` must be treated as unset, not as a valid empty
    // client ID that starts a doomed device flow.
    let out = login(Some(""));
    assert!(!out.status.success(), "empty override must fail fast");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""type":"usage""#), "usage error: {s}");
}
