//! Synchronous GitHub REST API client (via `ureq`) with PAT authentication.

pub mod auth;
mod error;

pub use auth::{clear_token, load_token, store_token, DeviceCode, DeviceFlow};
pub use error::GitHubError;

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("stacc/", env!("CARGO_PKG_VERSION"));
/// Overall per-request deadline, so a slow or hung GitHub endpoint can't wedge
/// the CLI (ureq's default read timeout is unbounded).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The lifecycle state of a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// A pull request, as stacc cares about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub state: PrState,
    pub title: String,
    pub body: String,
    /// GitHub's `mergeable_state` (e.g. `clean`, `blocked`, `behind`, `dirty`,
    /// `unknown`), or `None` when GitHub has not computed it yet.
    pub mergeable_state: Option<String>,
}

impl PullRequest {
    /// Whether GitHub reports the PR as cleanly mergeable. `null`/absent or any
    /// state other than `clean` reads as not-ready.
    pub fn ready(&self) -> bool {
        self.mergeable_state.as_deref() == Some("clean")
    }
}

/// Fields for creating a pull request.
#[derive(Debug, Clone, Serialize)]
pub struct NewPullRequest {
    pub title: String,
    pub head: String,
    pub base: String,
    pub body: String,
}

/// Fields to change on an existing pull request. Unset fields are left as-is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PullRequestUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// The subset of GitHub's PR JSON we read.
#[derive(Debug, Deserialize)]
struct RawPullRequest {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    // Existing fixtures omit this; without the default they fail to deserialize.
    #[serde(default)]
    mergeable_state: Option<String>,
}

impl From<RawPullRequest> for PullRequest {
    fn from(raw: RawPullRequest) -> Self {
        let state = if raw.merged {
            PrState::Merged
        } else if raw.state == "closed" {
            PrState::Closed
        } else {
            PrState::Open
        };
        PullRequest {
            number: raw.number,
            url: raw.html_url,
            state,
            title: raw.title,
            body: raw.body.unwrap_or_default(),
            mergeable_state: raw.mergeable_state,
        }
    }
}

/// GitHub's response to a merge request: the bits stacc reads.
#[derive(Debug, Deserialize)]
struct MergeResponse {
    #[serde(default)]
    merged: bool,
}

/// An authenticated GitHub API client.
pub struct GitHub {
    agent: ureq::Agent,
    token: String,
    base_url: String,
}

impl GitHub {
    /// Build a client from a token, using the public github.com API.
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// Build a client pointed at a specific base URL (used by tests).
    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            agent: ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build(),
            token: token.into(),
            base_url: base_url.into(),
        }
    }

    /// Resolve the access token from `GITHUB_TOKEN`/`GH_TOKEN`, falling back
    /// to the OS keychain entry written by `stacc auth login`.
    /// `GITHUB_API_URL` overrides the base URL (for GitHub Enterprise or tests).
    pub fn from_env() -> Result<Self, GitHubError> {
        let token = std::env::var("GITHUB_TOKEN")
            .ok()
            .or_else(|| std::env::var("GH_TOKEN").ok())
            .or_else(auth::load_token)
            .ok_or(GitHubError::MissingToken)?;
        let base_url =
            std::env::var("GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self::with_base_url(token, base_url))
    }

    /// The login of the authenticated user (`GET /user`). Proves the token works.
    pub fn current_user(&self) -> Result<String, GitHubError> {
        let value: serde_json::Value = self.get(&format!("{}/user", self.base_url))?;
        value
            .get("login")
            .and_then(|login| login.as_str())
            .map(ToString::to_string)
            .ok_or_else(|| GitHubError::Unexpected("missing `login` in /user response".into()))
    }

    /// Create a pull request.
    pub fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pr: &NewPullRequest,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/pulls", self.base_url);
        let raw: RawPullRequest = self.send("POST", &url, pr)?;
        Ok(raw.into())
    }

    /// Update an existing pull request.
    pub fn update_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        update: &PullRequestUpdate,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/pulls/{number}", self.base_url);
        let raw: RawPullRequest = self.send("PATCH", &url, update)?;
        Ok(raw.into())
    }

    /// Rename a branch on the remote
    /// (`POST /repos/{owner}/{repo}/branches/{branch}/rename`). GitHub retargets
    /// the base of open pull requests to the renamed branch, but CLOSES any pull
    /// request whose head is the renamed branch.
    pub fn rename_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        new_name: &str,
    ) -> Result<(), GitHubError> {
        let url = format!(
            "{}/repos/{owner}/{repo}/branches/{branch}/rename",
            self.base_url
        );
        let body = serde_json::json!({ "new_name": new_name });
        let _: serde_json::Value = self.send("POST", &url, &body)?;
        Ok(())
    }

    /// Fetch a pull request, including whether it was merged.
    pub fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/pulls/{number}", self.base_url);
        let raw: RawPullRequest = self.get(&url)?;
        Ok(raw.into())
    }

    /// Squash-merge a pull request (`PUT /repos/{o}/{r}/pulls/{number}/merge`).
    /// Returns whether GitHub reports it merged. 405/409 (not mergeable, or the
    /// head moved since readiness was read) map to [`GitHubError::NotMergeable`].
    pub fn merge_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<bool, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/pulls/{number}/merge", self.base_url);
        let body = serde_json::json!({ "merge_method": "squash" });
        match self.send::<_, MergeResponse>("PUT", &url, &body) {
            Ok(resp) => Ok(resp.merged),
            Err(GitHubError::Status { status, .. }) if status == 405 || status == 409 => {
                Err(GitHubError::NotMergeable)
            }
            Err(err) => Err(err),
        }
    }

    /// Whether `branch` has branch protection enabled
    /// (`GET /repos/{o}/{r}/branches/{branch}/protection`; 404 means none).
    pub fn branch_protected(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<bool, GitHubError> {
        let url = format!(
            "{}/repos/{owner}/{repo}/branches/{branch}/protection",
            self.base_url
        );
        match self.get::<serde_json::Value>(&url) {
            Ok(_) => Ok(true),
            Err(GitHubError::Status { status: 404, .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Build a request with the auth + GitHub headers already set.
    fn request(&self, method: &str, url: &str) -> ureq::Request {
        self.agent
            .request(method, url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/vnd.github+json")
    }

    fn get<T: DeserializeOwned>(&self, url: &str) -> Result<T, GitHubError> {
        let response = self
            .request("GET", url)
            .call()
            .map_err(GitHubError::from_ureq)?;
        Ok(response.into_json()?)
    }

    fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        url: &str,
        body: &B,
    ) -> Result<T, GitHubError> {
        let response = self
            .request(method, url)
            .send_json(body)
            .map_err(GitHubError::from_ureq)?;
        Ok(response.into_json()?)
    }
}

/// Parse a GitHub remote URL into `(owner, repo)`. Handles both
/// `https://github.com/owner/repo(.git)` and `git@github.com:owner/repo(.git)`.
pub fn parse_remote(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use serde_json::json;

    #[test]
    fn parse_remote_handles_https_and_ssh() {
        assert_eq!(
            parse_remote("https://github.com/TinyDogTech/stacc.git"),
            Some(("TinyDogTech".to_string(), "stacc".to_string()))
        );
        assert_eq!(
            parse_remote("git@github.com:TinyDogTech/stacc.git"),
            Some(("TinyDogTech".to_string(), "stacc".to_string()))
        );
        assert_eq!(
            parse_remote("https://github.com/owner/repo"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(parse_remote("https://gitlab.com/owner/repo"), None);
        assert_eq!(parse_remote("not a url"), None);
    }

    fn raw(state: &str, merged: bool) -> RawPullRequest {
        RawPullRequest {
            number: 1,
            html_url: "u".into(),
            state: state.into(),
            merged,
            title: "t".into(),
            body: None,
            mergeable_state: None,
        }
    }

    #[test]
    fn pr_state_mapping_covers_all_states() {
        assert_eq!(PullRequest::from(raw("closed", true)).state, PrState::Merged);
        assert_eq!(PullRequest::from(raw("closed", false)).state, PrState::Closed);
        assert_eq!(PullRequest::from(raw("open", false)).state, PrState::Open);
    }

    #[test]
    fn current_user_returns_login() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/user")
                .header("authorization", "Bearer test-token");
            then.status(200)
                .json_body(json!({ "login": "octocat" }));
        });

        let gh = GitHub::with_base_url("test-token", server.base_url());
        assert_eq!(gh.current_user().unwrap(), "octocat");
        mock.assert();
    }

    #[test]
    fn api_error_status_is_captured() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/user");
            then.status(401)
                .json_body(json!({ "message": "Bad credentials" }));
        });

        let gh = GitHub::with_base_url("bad", server.base_url());
        let err = gh.current_user().unwrap_err();
        assert!(matches!(err, GitHubError::Status { status: 401, .. }));
    }

    #[test]
    fn create_pull_request_parses_response() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/repos/o/r/pulls");
            then.status(201).json_body(json!({
                "number": 42,
                "html_url": "https://github.com/o/r/pull/42",
                "state": "open",
                "merged": false,
            }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let pr = gh
            .create_pull_request(
                "o",
                "r",
                &NewPullRequest {
                    title: "T".into(),
                    head: "feature".into(),
                    base: "main".into(),
                    body: "B".into(),
                },
            )
            .unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.url, "https://github.com/o/r/pull/42");
        mock.assert();
    }

    #[test]
    fn get_pull_request_detects_merge() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/repos/o/r/pulls/7");
            then.status(200).json_body(json!({
                "number": 7, "html_url": "u", "state": "closed", "merged": true,
            }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        assert_eq!(gh.get_pull_request("o", "r", 7).unwrap().state, PrState::Merged);
    }

    #[test]
    fn update_pull_request_sends_patch_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH)
                .path("/repos/o/r/pulls/7")
                .body_contains("Renamed");
            then.status(200).json_body(json!({
                "number": 7, "html_url": "u", "state": "open", "merged": false,
            }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let update = PullRequestUpdate {
            title: Some("Renamed".into()),
            ..Default::default()
        };
        assert_eq!(gh.update_pull_request("o", "r", 7, &update).unwrap().number, 7);
        mock.assert();
    }

    #[test]
    fn ready_reflects_mergeable_state() {
        let mk = |ms: Option<&str>| {
            PullRequest::from(RawPullRequest {
                number: 1,
                html_url: "u".into(),
                state: "open".into(),
                merged: false,
                title: "t".into(),
                body: None,
                mergeable_state: ms.map(String::from),
            })
        };
        assert!(mk(Some("clean")).ready());
        assert!(!mk(Some("blocked")).ready());
        assert!(!mk(Some("behind")).ready());
        assert!(!mk(Some("dirty")).ready());
        assert!(!mk(Some("unknown")).ready());
        assert!(!mk(None).ready()); // absent/null reads as not-ready
    }

    #[test]
    fn merge_pull_request_squashes() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/repos/o/r/pulls/7/merge")
                .json_body(json!({ "merge_method": "squash" }));
            then.status(200).json_body(json!({ "merged": true, "sha": "abc" }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        assert!(gh.merge_pull_request("o", "r", 7).unwrap());
        mock.assert();
    }

    #[test]
    fn merge_405_maps_to_not_mergeable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/repos/o/r/pulls/7/merge");
            then.status(405)
                .json_body(json!({ "message": "Pull Request is not mergeable" }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        assert!(matches!(
            gh.merge_pull_request("o", "r", 7).unwrap_err(),
            GitHubError::NotMergeable
        ));
    }

    #[test]
    fn branch_protected_reads_404_as_unprotected() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/repos/o/r/branches/main/protection");
            then.status(404)
                .json_body(json!({ "message": "Branch not protected" }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        assert!(!gh.branch_protected("o", "r", "main").unwrap());
    }
}
