//! Synchronous GitHub REST API client (via `ureq`) with PAT authentication.

mod error;

pub use error::GitHubError;

const DEFAULT_BASE_URL: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("stacc/", env!("CARGO_PKG_VERSION"));

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

    /// Build a client from `GITHUB_TOKEN` or `GH_TOKEN` in the environment.
    pub fn from_env() -> Result<Self, GitHubError> {
        let token = std::env::var("GITHUB_TOKEN")
            .or_else(|_| std::env::var("GH_TOKEN"))
            .map_err(|_| GitHubError::MissingToken)?;
        Ok(Self::new(token))
    }

    /// The login of the authenticated user (`GET /user`). Proves the token works.
    pub fn current_user(&self) -> Result<String, GitHubError> {
        let value: serde_json::Value = self.get(&format!("{}/user", self.base_url))?;
        value
            .get("login")
            .and_then(|login| login.as_str())
            .map(|login| login.to_string())
            .ok_or_else(|| GitHubError::Unexpected("missing `login` in /user response".into()))
    }

    fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, GitHubError> {
        let response = self
            .agent
            .get(url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/vnd.github+json")
            .call()
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

    #[test]
    fn current_user_returns_login() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/user")
                .header("authorization", "Bearer test-token");
            then.status(200).json_body(serde_json::json!({ "login": "octocat" }));
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
            then.status(401).json_body(serde_json::json!({ "message": "Bad credentials" }));
        });

        let gh = GitHub::with_base_url("bad", server.base_url());
        let err = gh.current_user().unwrap_err();
        assert!(matches!(err, GitHubError::Status { status: 401, .. }));
    }
}
