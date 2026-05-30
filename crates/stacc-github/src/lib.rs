//! Synchronous GitHub REST API client (via `ureq`) with PAT authentication.

mod error;

pub use error::GitHubError;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("stacc/", env!("CARGO_PKG_VERSION"));

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
        }
    }
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
            agent: ureq::AgentBuilder::new().build(),
            token: token.into(),
            base_url: base_url.into(),
        }
    }

    /// Build a client from `GITHUB_TOKEN`/`GH_TOKEN` in the environment.
    /// `GITHUB_API_URL` overrides the base URL (for GitHub Enterprise or tests).
    pub fn from_env() -> Result<Self, GitHubError> {
        let token = std::env::var("GITHUB_TOKEN")
            .or_else(|_| std::env::var("GH_TOKEN"))
            .map_err(|_| GitHubError::MissingToken)?;
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
}
