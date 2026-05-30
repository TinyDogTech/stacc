//! HTTP-level coverage for the device-flow client. The poll state machine
//! itself is unit-tested in `auth.rs::tests`; these tests cover only the
//! request shapes and the wiring through `ureq`/`serde`.

use httpmock::MockServer;
use serde_json::json;
use stacc_github::{DeviceFlow, GitHubError};

/// Build a DeviceFlow pointed at the mock server. The polling loop's
/// `sleep` callback is a no-op so tests don't actually wait.
fn flow_against(server: &MockServer) -> DeviceFlow {
    let mut flow = DeviceFlow::default();
    flow.device_code_url = format!("{}/login/device/code", server.base_url());
    flow.token_url = format!("{}/login/oauth/access_token", server.base_url());
    flow.client_id = "test-client".into();
    flow
}

fn code_body() -> serde_json::Value {
    json!({
        "device_code": "DC123",
        "user_code": "WDJB-MJHT",
        "verification_uri": "https://github.com/login/device",
        "expires_in": 900,
        "interval": 1,
    })
}

#[test]
fn happy_path_returns_token() {
    let server = MockServer::start();
    let device_mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/login/device/code");
        then.status(200).json_body(code_body());
    });
    let token_mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/login/oauth/access_token");
        then.status(200)
            .json_body(json!({ "access_token": "ghu_first", "token_type": "bearer" }));
    });

    let flow = flow_against(&server);
    let code = flow.request_code().unwrap();
    assert_eq!(code.user_code, "WDJB-MJHT");

    let token = flow.poll_token(&code, |_| {}).unwrap();
    assert_eq!(token, "ghu_first");

    device_mock.assert();
    token_mock.assert();
}

#[test]
fn access_denied_is_surfaced_as_typed_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/login/device/code");
        then.status(200).json_body(code_body());
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/login/oauth/access_token");
        then.status(200).json_body(json!({ "error": "access_denied" }));
    });

    let flow = flow_against(&server);
    let code = flow.request_code().unwrap();
    let err = flow.poll_token(&code, |_| {}).unwrap_err();
    assert!(matches!(err, GitHubError::DeviceFlowDenied), "got {err:?}");
}

#[test]
fn expired_token_is_surfaced_as_typed_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/login/device/code");
        then.status(200).json_body(code_body());
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/login/oauth/access_token");
        then.status(200).json_body(json!({ "error": "expired_token" }));
    });

    let flow = flow_against(&server);
    let code = flow.request_code().unwrap();
    let err = flow.poll_token(&code, |_| {}).unwrap_err();
    assert!(matches!(err, GitHubError::DeviceFlowExpired), "got {err:?}");
}
