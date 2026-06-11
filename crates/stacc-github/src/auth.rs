//! OAuth device-flow client and OS-keychain token storage.
//!
//! `stacc auth login` invokes [`DeviceFlow::request_code`] to obtain a
//! user-facing code, prints it for the user to type into GitHub, then calls
//! [`DeviceFlow::poll_token`] to block until GitHub returns an access token.
//! The token lands in the platform keychain via [`store_token`].

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use ureq::Agent;

use crate::error::GitHubError;

const DEFAULT_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const DEFAULT_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Placeholder. Replace this constant once the OAuth App is registered under
/// `TinyDogTech` (see the STA-17 PR description for the steps). Local dev can
/// override at run time via `STACC_OAUTH_CLIENT_ID`.
const DEFAULT_OAUTH_CLIENT_ID: &str = "stacc-oauth-client-id-placeholder";

/// OAuth App scopes work coarsely, `repo` is the narrowest scope that grants
/// PR read/write. Users who want least privilege should mint a fine-grained
/// PAT (Pull requests: read+write, Contents: read) and export `GITHUB_TOKEN`.
const OAUTH_SCOPE: &str = "repo";

/// One slot per stacc install: service = "stacc", username = "github.com".
const KEYRING_SERVICE: &str = "stacc";
const KEYRING_USER: &str = "github.com";

/// Bound on `gh auth token`: it is a local read, so a slow run means a wedged
/// credential helper. Kill it and treat the run as "no token" rather than let
/// it block the CLI.
const GH_TOKEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Response from `POST /login/device/code`, the user code is what the user
/// types into GitHub, the device code is what we poll with.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// Client for GitHub's device-authorization flow. Defaults hit github.com;
/// `STACC_OAUTH_*_URL` env vars redirect to a mock server for tests.
pub struct DeviceFlow {
    pub client_id: String,
    pub device_code_url: String,
    pub token_url: String,
    pub scope: String,
    agent: Agent,
}

impl Default for DeviceFlow {
    fn default() -> Self {
        Self {
            client_id: env_or(
                "STACC_OAUTH_CLIENT_ID",
                DEFAULT_OAUTH_CLIENT_ID,
            ),
            device_code_url: env_or(
                "STACC_OAUTH_DEVICE_CODE_URL",
                DEFAULT_DEVICE_CODE_URL,
            ),
            token_url: env_or("STACC_OAUTH_TOKEN_URL", DEFAULT_TOKEN_URL),
            scope: OAUTH_SCOPE.to_string(),
            agent: ureq::AgentBuilder::new().build(),
        }
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

/// Whether `id` is a usable OAuth client ID: not empty/whitespace and not the
/// unregistered placeholder.
fn client_id_is_configured(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && id != DEFAULT_OAUTH_CLIENT_ID
}

impl DeviceFlow {
    /// Whether this flow has a usable OAuth client ID. False when the client ID
    /// is still the unregistered placeholder or empty/whitespace (a stray
    /// `STACC_OAUTH_CLIENT_ID=` leaves it empty), in which case the device flow
    /// would only fail with a confusing GitHub error.
    pub fn is_configured(&self) -> bool {
        client_id_is_configured(&self.client_id)
    }

    /// Step 1: ask GitHub for a device + user code.
    pub fn request_code(&self) -> Result<DeviceCode, GitHubError> {
        let resp = self
            .agent
            .post(&self.device_code_url)
            .set("Accept", "application/json")
            .send_form(&[("client_id", &self.client_id), ("scope", &self.scope)])
            .map_err(GitHubError::from_ureq)?;
        Ok(resp.into_json()?)
    }

    /// Step 2: poll `POST /login/oauth/access_token` until the user authorizes
    /// (success), denies, or the code expires. `sleep` is injected so tests can
    /// stub it out, production passes `std::thread::sleep`.
    pub fn poll_token(
        &self,
        code: &DeviceCode,
        mut sleep: impl FnMut(Duration),
    ) -> Result<String, GitHubError> {
        let mut interval = Duration::from_secs(code.interval.max(1));
        let deadline = Instant::now() + Duration::from_secs(code.expires_in);

        loop {
            if Instant::now() >= deadline {
                return Err(GitHubError::DeviceFlowExpired);
            }
            sleep(interval);
            let raw = self.request_token(&code.device_code)?;
            if let Some(token) = advance_poll(raw, &mut interval)? {
                return Ok(token);
            }
        }
    }

    /// One round-trip against the token endpoint. Split out so `poll_token`
    /// only owns the looping concerns and the state machine sits in
    /// [`advance_poll`].
    fn request_token(&self, device_code: &str) -> Result<TokenResponse, GitHubError> {
        let resp = self
            .agent
            .post(&self.token_url)
            .set("Accept", "application/json")
            .send_form(&[
                ("client_id", &self.client_id),
                ("device_code", device_code),
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code",
                ),
            ])
            .map_err(GitHubError::from_ureq)?;
        Ok(resp.into_json()?)
    }
}

/// `serde(untagged)` lets one enum cover both shapes GitHub returns from the
/// token endpoint: `{access_token, ...}` on success, `{error, ...}` while
/// pending or on a terminal failure.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TokenResponse {
    Success { access_token: String },
    Error { error: String },
}

/// Pure state machine over one device-flow poll response.
///
/// - `Ok(Some(token))` → terminal success
/// - `Ok(None)` → keep polling (pending or slow_down, possibly with a wider interval)
/// - `Err(_)` → terminal failure (denied, expired, or an unknown error code)
///
/// Extracted from [`DeviceFlow::poll_token`] so we can unit-test it without
/// spinning up an HTTP mock.
fn advance_poll(
    resp: TokenResponse,
    interval: &mut Duration,
) -> Result<Option<String>, GitHubError> {
    match resp {
        TokenResponse::Success { access_token } => Ok(Some(access_token)),
        // The pending-state signals all come back 200 OK with an `error`
        // field, the IETF device-flow spec, not REST-style failures.
        TokenResponse::Error { error } => match error.as_str() {
            "authorization_pending" => Ok(None),
            "slow_down" => {
                // GitHub asks us to back off; spec says add five seconds.
                *interval += Duration::from_secs(5);
                Ok(None)
            }
            "expired_token" => Err(GitHubError::DeviceFlowExpired),
            "access_denied" => Err(GitHubError::DeviceFlowDenied),
            other => Err(GitHubError::Unexpected(format!(
                "device flow error: {other}"
            ))),
        },
    }
}

/// Store the access token in the OS keychain (Keychain / Credential Manager /
/// Secret Service depending on the platform).
pub fn store_token(token: &str) -> Result<(), GitHubError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| GitHubError::Keyring(e.to_string()))?;
    entry
        .set_password(token)
        .map_err(|e| GitHubError::Keyring(e.to_string()))
}

/// Load the access token from the OS keychain, if any.
pub fn load_token() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    entry.get_password().ok()
}

/// Clear the stored token. A missing entry is *not* an error, logout is
/// idempotent.
pub fn clear_token() -> Result<(), GitHubError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| GitHubError::Keyring(e.to_string()))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(GitHubError::Keyring(e.to_string())),
    }
}

/// An env var read with empty (or whitespace-only) treated as unset, so a
/// stray `GITHUB_TOKEN=` does not resolve to an empty bearer token.
pub fn env_token(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The OS keychain token, unless `STACC_KEYCHAIN` is set to an empty value.
/// That empty-disables knob keeps the integration suite hermetic on a machine
/// that has run `stacc auth login`; production leaves it unset and reads the
/// keychain normally.
pub fn keychain_token() -> Option<String> {
    if matches!(std::env::var("STACC_KEYCHAIN"), Ok(v) if v.is_empty()) {
        return None;
    }
    load_token()
}

/// Resolve a token from `gh auth token`. The binary is `STACC_GH_BIN` when set
/// (the test hook); an empty value disables the fallback (the kill switch), an
/// unset value uses `gh` from `PATH`. A missing binary, non-zero exit, empty
/// output, or a timeout all read as "no token".
pub fn gh_token() -> Option<String> {
    let bin = match std::env::var("STACC_GH_BIN") {
        Ok(v) if v.is_empty() => return None,
        Ok(v) => v,
        Err(_) => "gh".to_string(),
    };
    run_gh_token(&bin)
}

/// Spawn `<bin> auth token --hostname github.com` and return the trimmed stdout
/// when it is non-empty and the process exits 0. The captured stdout *is* the
/// token, so it never appears in an error; spawn and exit failures map to None.
/// A wedged `gh` is killed after [`GH_TOKEN_TIMEOUT`] so it cannot wedge the CLI.
fn run_gh_token(bin: &str) -> Option<String> {
    let mut child = Command::new(bin)
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Read stdout on a worker thread so the wait is bounded: a hung credential
    // helper must not wedge the CLI. On timeout, kill the child.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    let Ok(buf) = rx.recv_timeout(GH_TOKEN_TIMEOUT) else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return None;
    };
    let _ = reader.join();

    match child.wait() {
        Ok(status) if status.success() => {
            let token = buf.trim();
            (!token.is_empty()).then(|| token.to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_returns_token() {
        let mut interval = Duration::from_secs(5);
        let resp = TokenResponse::Success { access_token: "ghu_x".into() };
        let out = advance_poll(resp, &mut interval).unwrap();
        assert_eq!(out, Some("ghu_x".to_string()));
        assert_eq!(interval, Duration::from_secs(5));
    }

    #[test]
    fn pending_continues_and_leaves_interval_alone() {
        let mut interval = Duration::from_secs(5);
        let resp = TokenResponse::Error { error: "authorization_pending".into() };
        let out = advance_poll(resp, &mut interval).unwrap();
        assert_eq!(out, None);
        assert_eq!(interval, Duration::from_secs(5));
    }

    #[test]
    fn slow_down_grows_the_interval_by_five_seconds() {
        let mut interval = Duration::from_secs(5);
        let resp = TokenResponse::Error { error: "slow_down".into() };
        let out = advance_poll(resp, &mut interval).unwrap();
        assert_eq!(out, None);
        assert_eq!(interval, Duration::from_secs(10));
    }

    #[test]
    fn denied_is_terminal() {
        let mut interval = Duration::from_secs(5);
        let resp = TokenResponse::Error { error: "access_denied".into() };
        let err = advance_poll(resp, &mut interval).unwrap_err();
        assert!(matches!(err, GitHubError::DeviceFlowDenied));
    }

    #[test]
    fn expired_is_terminal() {
        let mut interval = Duration::from_secs(5);
        let resp = TokenResponse::Error { error: "expired_token".into() };
        let err = advance_poll(resp, &mut interval).unwrap_err();
        assert!(matches!(err, GitHubError::DeviceFlowExpired));
    }

    #[test]
    fn unknown_error_is_wrapped_as_unexpected() {
        let mut interval = Duration::from_secs(5);
        let resp = TokenResponse::Error { error: "what_even".into() };
        let err = advance_poll(resp, &mut interval).unwrap_err();
        assert!(matches!(err, GitHubError::Unexpected(_)), "got {err:?}");
    }

    #[test]
    fn client_id_configured_rejects_placeholder_and_empty() {
        assert!(!client_id_is_configured(DEFAULT_OAUTH_CLIENT_ID), "placeholder");
        assert!(!client_id_is_configured(""), "empty");
        assert!(!client_id_is_configured("   "), "whitespace");
        assert!(client_id_is_configured("Iv1.realclientid0000"), "real id");
    }

    #[test]
    fn gh_token_none_when_binary_is_missing() {
        // A spawn failure (no such binary) reads as "no token", never an error.
        assert_eq!(run_gh_token("/no/such/stacc-gh-binary"), None);
    }

    #[cfg(unix)]
    fn fake_gh(dir: &std::path::Path, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn gh_token_returns_trimmed_stdout() {
        let dir = tempfile::TempDir::new().unwrap();
        let gh = fake_gh(dir.path(), "#!/bin/sh\necho '  gho_faketoken  '\n");
        assert_eq!(
            run_gh_token(gh.to_str().unwrap()),
            Some("gho_faketoken".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn gh_token_none_on_nonzero_exit() {
        // gh writes "no oauth token found" to stderr and exits 1 when logged out.
        let dir = tempfile::TempDir::new().unwrap();
        let gh = fake_gh(dir.path(), "#!/bin/sh\necho 'no oauth token' >&2\nexit 1\n");
        assert_eq!(run_gh_token(gh.to_str().unwrap()), None);
    }

    #[cfg(unix)]
    #[test]
    fn gh_token_none_on_empty_stdout() {
        let dir = tempfile::TempDir::new().unwrap();
        let gh = fake_gh(dir.path(), "#!/bin/sh\nexit 0\n");
        assert_eq!(run_gh_token(gh.to_str().unwrap()), None);
    }
}
