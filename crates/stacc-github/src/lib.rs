//! Synchronous GitHub REST API client (via `ureq`) with PAT authentication.

pub mod auth;
mod error;

pub use auth::{clear_token, load_token, store_token, DeviceCode, DeviceFlow};
pub use error::GitHubError;

use std::collections::BTreeMap;
use std::fmt::Write as _;
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
    /// Whether the PR is a draft.
    pub draft: bool,
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
    /// Open the PR as a draft.
    pub draft: bool,
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
    // The list endpoint (`GET /pulls`) omits `merged` and carries `merged_at`
    // instead; without reading it a merged PR found by head parses as Closed.
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    // Existing fixtures omit this; without the default they fail to deserialize.
    #[serde(default)]
    mergeable_state: Option<String>,
}

impl From<RawPullRequest> for PullRequest {
    fn from(raw: RawPullRequest) -> Self {
        let state = if raw.merged || raw.merged_at.is_some() {
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
            draft: raw.draft,
            mergeable_state: raw.mergeable_state,
        }
    }
}

/// The review decision GitHub reports for a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// The CI check rollup for a pull request's head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckRollup {
    Pass,
    Fail,
    Pending,
}

/// Per-PR review decision and CI rollup, from the batched status query.
/// Either side is `None` when GitHub reports nothing (no reviewers configured,
/// no checks on the head commit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrChecks {
    pub review: Option<ReviewDecision>,
    pub checks: Option<CheckRollup>,
}

/// The outcome of a merge request: whether GitHub merged it, and the squash
/// commit SHA when it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub merged: bool,
    pub sha: Option<String>,
}

/// GitHub's response to a merge request: the bits stacc reads.
#[derive(Debug, Deserialize)]
struct MergeResponse {
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    sha: Option<String>,
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

    /// Close a pull request without merging it
    /// (`PATCH /repos/{owner}/{repo}/pulls/{number}` with `state: closed`).
    pub fn close_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/pulls/{number}", self.base_url);
        let body = serde_json::json!({ "state": "closed" });
        let raw: RawPullRequest = self.send("PATCH", &url, &body)?;
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

    /// Find the open pull request whose head is `owner:branch`, if one exists
    /// (`GET /repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=open`).
    /// Returns the first match, or `None` when no open PR has that head. Used
    /// to adopt PRs created outside stacc (e.g. via `gh` before a migration).
    pub fn pull_request_for_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Option<PullRequest>, GitHubError> {
        self.find_pull_request_by_head(owner, repo, branch, "open", None)
    }

    /// Like [`pull_request_for_branch`](Self::pull_request_for_branch) but caps
    /// this single call at `timeout`, mirroring
    /// [`get_pull_request_within`](Self::get_pull_request_within) for callers
    /// working under a wall-clock budget.
    pub fn pull_request_for_branch_within(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        timeout: Duration,
    ) -> Result<Option<PullRequest>, GitHubError> {
        self.find_pull_request_by_head(owner, repo, branch, "open", Some(timeout))
    }

    /// Find the newest pull request whose head is `owner:branch` in *any*
    /// state (`GET /pulls?head=...&state=all`, newest first). Unlike
    /// [`pull_request_for_branch`](Self::pull_request_for_branch), merged and
    /// closed PRs are included; `sync` uses this to adopt PRs created outside
    /// stacc and reconcile ones that already merged.
    pub fn pull_request_for_branch_any_state(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Option<PullRequest>, GitHubError> {
        self.find_pull_request_by_head(owner, repo, branch, "all", None)
    }

    fn find_pull_request_by_head(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        state: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<PullRequest>, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/pulls", self.base_url);
        // Newest first, so with `state=all` the head's most recent PR wins
        // (e.g. an old closed PR never shadows the branch's current one).
        let mut request = self
            .request("GET", &url)
            .query("head", &format!("{owner}:{branch}"))
            .query("state", state)
            .query("sort", "created")
            .query("direction", "desc")
            .query("per_page", "1");
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request.call().map_err(GitHubError::from_ureq)?;
        let raws: Vec<RawPullRequest> = response.into_json()?;
        Ok(raws.into_iter().next().map(PullRequest::from))
    }

    /// Like [`get_pull_request`](Self::get_pull_request) but caps this single
    /// call at `timeout`, so a caller polling several PRs under a wall-clock
    /// budget can bound in-flight time, not just when it stops starting calls.
    pub fn get_pull_request_within(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        timeout: Duration,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/pulls/{number}", self.base_url);
        let response = self
            .request("GET", &url)
            .timeout(timeout)
            .call()
            .map_err(GitHubError::from_ureq)?;
        let raw: RawPullRequest = response.into_json()?;
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
    ) -> Result<MergeOutcome, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/pulls/{number}/merge", self.base_url);
        let body = serde_json::json!({ "merge_method": "squash" });
        match self.send::<_, MergeResponse>("PUT", &url, &body) {
            Ok(resp) => Ok(MergeOutcome {
                merged: resp.merged,
                sha: resp.sha,
            }),
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

    /// Review decision and CI check rollup for a set of pull requests, in ONE
    /// GraphQL call (`POST /graphql`) regardless of how many numbers are asked
    /// for, capped at `timeout`. Each number maps to its [`PrChecks`]; a PR the
    /// response omits (deleted, or an unknown number) is simply absent. Parsing
    /// is defensive: a partial or error-bearing GraphQL response yields an
    /// emptier map, never an error, so callers degrade silently.
    pub fn pull_request_checks_within(
        &self,
        owner: &str,
        repo: &str,
        numbers: &[u64],
        timeout: Duration,
    ) -> Result<BTreeMap<u64, PrChecks>, GitHubError> {
        if numbers.is_empty() {
            return Ok(BTreeMap::new());
        }
        // One aliased `pullRequest` field per number; the repo coordinates ride
        // as variables so they need no escaping inside the query text.
        let mut fields = String::new();
        for n in numbers {
            let _ = write!(
                fields,
                "pr{n}: pullRequest(number: {n}) {{ reviewDecision \
                 commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }} }} "
            );
        }
        let query = format!(
            "query($owner: String!, $name: String!) {{ \
             repository(owner: $owner, name: $name) {{ {fields}}} }}"
        );
        let body = serde_json::json!({
            "query": query,
            "variables": { "owner": owner, "name": repo },
        });
        let url = graphql_url(&self.base_url);
        let response = self
            .request("POST", &url)
            .timeout(timeout)
            .send_json(&body)
            .map_err(GitHubError::from_ureq)?;
        let value: serde_json::Value = response.into_json()?;

        let repository = &value["data"]["repository"];
        let mut map = BTreeMap::new();
        for &number in numbers {
            let pr = &repository[format!("pr{number}")];
            if !pr.is_object() {
                continue;
            }
            let review = match pr["reviewDecision"].as_str() {
                Some("APPROVED") => Some(ReviewDecision::Approved),
                Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
                Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
                _ => None,
            };
            let rollup = &pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"];
            let checks = match rollup.as_str() {
                Some("SUCCESS") => Some(CheckRollup::Pass),
                Some("FAILURE" | "ERROR") => Some(CheckRollup::Fail),
                Some("PENDING" | "EXPECTED") => Some(CheckRollup::Pending),
                _ => None,
            };
            map.insert(number, PrChecks { review, checks });
        }
        Ok(map)
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

/// The GraphQL endpoint for a REST base URL. github.com (and the test mocks)
/// serve GraphQL at `{base}/graphql`, but a GitHub Enterprise REST base ends
/// in `/api/v3` while its GraphQL endpoint is `/api/graphql`.
fn graphql_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    match trimmed.strip_suffix("/api/v3") {
        Some(host) => format!("{host}/api/graphql"),
        None => format!("{trimmed}/graphql"),
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
            merged_at: None,
            title: "t".into(),
            body: None,
            draft: false,
            mergeable_state: None,
        }
    }

    #[test]
    fn pr_state_mapping_covers_all_states() {
        assert_eq!(PullRequest::from(raw("closed", true)).state, PrState::Merged);
        assert_eq!(PullRequest::from(raw("closed", false)).state, PrState::Closed);
        assert_eq!(PullRequest::from(raw("open", false)).state, PrState::Open);
        // The list endpoint signals a merge via `merged_at` with `merged` absent.
        let listed = RawPullRequest {
            merged_at: Some("2026-06-10T18:00:00Z".into()),
            ..raw("closed", false)
        };
        assert_eq!(PullRequest::from(listed).state, PrState::Merged);
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
                    draft: false,
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
    fn pull_request_for_branch_finds_the_open_pr() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/repos/o/r/pulls")
                .query_param("head", "o:feature")
                .query_param("state", "open")
                .query_param("per_page", "1");
            then.status(200).json_body(json!([{
                "number": 12,
                "html_url": "https://github.com/o/r/pull/12",
                "state": "open",
                "merged": false,
            }]));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let pr = gh
            .pull_request_for_branch("o", "r", "feature")
            .unwrap()
            .expect("an open PR for the head");
        assert_eq!(pr.number, 12);
        assert_eq!(pr.url, "https://github.com/o/r/pull/12");
        assert_eq!(pr.state, PrState::Open);
        mock.assert();
    }

    #[test]
    fn pull_request_for_branch_any_state_detects_a_merged_pr() {
        let server = MockServer::start();
        // The list endpoint reports merges via `merged_at`, not `merged`.
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/repos/o/r/pulls")
                .query_param("head", "o:feature")
                .query_param("state", "all")
                .query_param("sort", "created")
                .query_param("direction", "desc")
                .query_param("per_page", "1");
            then.status(200).json_body(json!([{
                "number": 7,
                "html_url": "https://github.com/o/r/pull/7",
                "state": "closed",
                "merged_at": "2026-06-10T18:00:00Z",
            }]));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let pr = gh
            .pull_request_for_branch_any_state("o", "r", "feature")
            .unwrap()
            .expect("a PR for the head");
        assert_eq!(pr.number, 7);
        assert_eq!(pr.state, PrState::Merged);
        mock.assert();
    }

    #[test]
    fn pull_request_for_branch_any_state_reports_closed_unmerged() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/repos/o/r/pulls")
                .query_param("head", "o:feature")
                .query_param("state", "all");
            then.status(200).json_body(json!([{
                "number": 8,
                "html_url": "u",
                "state": "closed",
                "merged_at": null,
            }]));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let pr = gh
            .pull_request_for_branch_any_state("o", "r", "feature")
            .unwrap()
            .expect("a PR for the head");
        assert_eq!(pr.state, PrState::Closed);
        mock.assert();
    }

    #[test]
    fn pull_request_for_branch_is_none_when_no_pr_matches() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/repos/o/r/pulls")
                .query_param("head", "o:feature");
            then.status(200).json_body(json!([]));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        // Exercise the budgeted variant; it shares the lookup with the plain one.
        let pr = gh
            .pull_request_for_branch_within("o", "r", "feature", Duration::from_secs(5))
            .unwrap();
        assert_eq!(pr, None);
        mock.assert();
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
    fn close_pull_request_patches_state_closed() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH)
                .path("/repos/o/r/pulls/7")
                .json_body(json!({ "state": "closed" }));
            then.status(200).json_body(json!({
                "number": 7, "html_url": "u", "state": "closed", "merged": false,
            }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let pr = gh.close_pull_request("o", "r", 7).unwrap();
        assert_eq!(pr.state, PrState::Closed);
        mock.assert();
    }

    #[test]
    fn ready_reflects_mergeable_state() {
        let mk = |ms: Option<&str>| {
            PullRequest::from(RawPullRequest {
                mergeable_state: ms.map(String::from),
                ..raw("open", false)
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
        let outcome = gh.merge_pull_request("o", "r", 7).unwrap();
        assert!(outcome.merged);
        assert_eq!(outcome.sha.as_deref(), Some("abc"));
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
    fn draft_field_deserializes_and_defaults_to_false() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/repos/o/r/pulls/7");
            then.status(200).json_body(json!({
                "number": 7, "html_url": "u", "state": "open", "merged": false,
                "draft": true,
            }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        assert!(gh.get_pull_request("o", "r", 7).unwrap().draft);
        // Fixtures without the field (and the From helper) read as not-draft.
        assert!(!PullRequest::from(raw("open", false)).draft);
    }

    #[test]
    fn pull_request_checks_batches_one_graphql_call() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/graphql")
                .body_contains("pr7: pullRequest(number: 7)")
                .body_contains("pr8: pullRequest(number: 8)");
            then.status(200).json_body(json!({
                "data": { "repository": {
                    "pr7": {
                        "reviewDecision": "APPROVED",
                        "commits": { "nodes": [ { "commit": {
                            "statusCheckRollup": { "state": "SUCCESS" }
                        } } ] }
                    },
                    "pr8": {
                        "reviewDecision": "CHANGES_REQUESTED",
                        "commits": { "nodes": [ { "commit": {
                            "statusCheckRollup": { "state": "FAILURE" }
                        } } ] }
                    },
                } }
            }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let map = gh
            .pull_request_checks_within("o", "r", &[7, 8], Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            map[&7],
            PrChecks {
                review: Some(ReviewDecision::Approved),
                checks: Some(CheckRollup::Pass),
            }
        );
        assert_eq!(
            map[&8],
            PrChecks {
                review: Some(ReviewDecision::ChangesRequested),
                checks: Some(CheckRollup::Fail),
            }
        );
        mock.assert(); // exactly one call covered both PRs
    }

    #[test]
    fn pull_request_checks_tolerates_missing_data() {
        let server = MockServer::start();
        // pr7: no review decision, pending checks; pr8: GitHub returned null
        // (e.g. an unknown number); pr9: no checks configured at all.
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/graphql");
            then.status(200).json_body(json!({
                "data": { "repository": {
                    "pr7": {
                        "reviewDecision": null,
                        "commits": { "nodes": [ { "commit": {
                            "statusCheckRollup": { "state": "PENDING" }
                        } } ] }
                    },
                    "pr8": null,
                    "pr9": { "reviewDecision": "REVIEW_REQUIRED", "commits": { "nodes": [] } },
                } }
            }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let map = gh
            .pull_request_checks_within("o", "r", &[7, 8, 9], Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            map.get(&7),
            Some(&PrChecks { review: None, checks: Some(CheckRollup::Pending) })
        );
        assert_eq!(map.get(&8), None, "a null PR is simply absent");
        assert_eq!(
            map.get(&9),
            Some(&PrChecks { review: Some(ReviewDecision::ReviewRequired), checks: None })
        );
    }

    #[test]
    fn graphql_url_handles_dotcom_and_enterprise_bases() {
        assert_eq!(graphql_url("https://api.github.com"), "https://api.github.com/graphql");
        assert_eq!(
            graphql_url("https://ghe.corp/api/v3"),
            "https://ghe.corp/api/graphql"
        );
        assert_eq!(graphql_url("http://127.0.0.1:8080/"), "http://127.0.0.1:8080/graphql");
    }

    #[test]
    fn pull_request_checks_empty_input_short_circuits() {
        // No server: an empty number list must not touch the network.
        let gh = GitHub::with_base_url("t", "http://127.0.0.1:1");
        let map = gh
            .pull_request_checks_within("o", "r", &[], Duration::from_secs(5))
            .unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn pull_request_checks_surfaces_http_errors() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/graphql");
            then.status(500).json_body(json!({ "message": "boom" }));
        });

        let gh = GitHub::with_base_url("t", server.base_url());
        let err = gh
            .pull_request_checks_within("o", "r", &[7], Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(err, GitHubError::Status { status: 500, .. }));
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
