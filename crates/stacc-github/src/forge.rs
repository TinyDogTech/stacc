//! The GitHub implementation of the forge boundary.
//!
//! [`GitHubForge`] wraps the concrete [`GitHub`] client plus a single project's
//! coordinates (`owner`/`repo`) and maps GitHub's PR-shaped API onto the neutral
//! [`Forge`] vocabulary. It is behavior-preserving: the same HTTP calls the CLI
//! makes today, re-expressed in neutral types. The CLI keeps constructing
//! `GitHub` directly and emitting the old JSON field names until the wiring unit
//! routes it through `dyn Forge`.

use std::collections::BTreeMap;
use std::time::Duration;

use stacc_forge::{
    Capabilities, Change, ChangeState, ChangeStatus, ChangeUpdate, ChecksState, Forge, ForgeError,
    MergeOptions, MergeOutcome, MergeReadiness, MergeRejectionReason, ReviewState, SubmitChange,
};

use crate::{
    CheckRollup, GitHub, GitHubError, NewPullRequest, PrChecks, PrState, PullRequest,
    PullRequestUpdate, ReviewDecision,
};

/// Map a GitHub client error onto the neutral forge error.
///
/// The body carried by [`GitHubError::Status`] is already scrubbed to GitHub's
/// safe `message` (see `error.rs`), so including it here leaks no raw response.
/// A blocked merge does not flow through this conversion: [`GitHubForge::merge_change`]
/// turns [`GitHubError::NotMergeable`] into a structured [`ForgeError::Rejected`]
/// before it can reach here.
impl From<&GitHubError> for ForgeError {
    fn from(err: &GitHubError) -> Self {
        match err {
            GitHubError::MissingToken => ForgeError::MissingToken,
            // Reaching here means NotMergeable escaped a call other than a merge;
            // fall back to an unspecified rejection rather than an opaque error.
            GitHubError::NotMergeable => ForgeError::Rejected(MergeRejectionReason::Unknown),
            GitHubError::Status { status, body } => match *status {
                401 | 403 => ForgeError::AuthFailed,
                404 => ForgeError::NotFound,
                409 => ForgeError::Conflict,
                429 => ForgeError::RateLimited,
                _ => ForgeError::Unexpected(format!("GitHub status {status}: {body}")),
            },
            GitHubError::Transport(msg) => ForgeError::Transport(msg.clone()),
            GitHubError::Decode(err) => ForgeError::Unexpected(format!("decode: {err}")),
            GitHubError::Unexpected(msg) => ForgeError::Unexpected(msg.clone()),
            GitHubError::DeviceFlowExpired | GitHubError::DeviceFlowDenied => ForgeError::AuthFailed,
            GitHubError::Keyring(msg) => ForgeError::Unexpected(format!("keyring: {msg}")),
        }
    }
}

impl From<GitHubError> for ForgeError {
    fn from(err: GitHubError) -> Self {
        Self::from(&err)
    }
}

fn change_state(state: PrState) -> ChangeState {
    match state {
        PrState::Open => ChangeState::Open,
        PrState::Closed => ChangeState::Closed,
        PrState::Merged => ChangeState::Merged,
    }
}

/// Flatten GitHub's `mergeable_state` string into the neutral readiness signal.
/// Only the states stacc acts on are named; everything else, including absence
/// and GitHub's not-yet-computed `unknown`, is [`MergeReadiness::Unknown`].
fn readiness(mergeable_state: Option<&str>) -> MergeReadiness {
    match mergeable_state {
        Some("clean") => MergeReadiness::Ready,
        Some("dirty") => MergeReadiness::Conflicted,
        Some("behind") => MergeReadiness::Behind,
        Some("blocked") => MergeReadiness::Blocked,
        _ => MergeReadiness::Unknown,
    }
}

fn to_change(pr: PullRequest) -> Change {
    Change {
        number: pr.number,
        url: pr.url,
        state: change_state(pr.state),
        title: pr.title,
        body: pr.body,
        draft: pr.draft,
        readiness: readiness(pr.mergeable_state.as_deref()),
    }
}

fn review_state(review: Option<ReviewDecision>) -> ReviewState {
    match review {
        Some(ReviewDecision::Approved) => ReviewState::Approved,
        Some(ReviewDecision::ChangesRequested) => ReviewState::ChangesRequested,
        Some(ReviewDecision::ReviewRequired) => ReviewState::ReviewRequired,
        None => ReviewState::NoReview,
    }
}

fn checks_state(checks: Option<CheckRollup>) -> ChecksState {
    match checks {
        Some(CheckRollup::Pass) => ChecksState::Passed,
        Some(CheckRollup::Fail) => ChecksState::Failed,
        Some(CheckRollup::Pending) => ChecksState::Pending,
        // No CI configured: distinct from Passed so the gate's no-CI guard can
        // tell them apart.
        None => ChecksState::NoChecks,
    }
}

fn to_status(checks: PrChecks) -> ChangeStatus {
    ChangeStatus {
        review: review_state(checks.review),
        checks: checks_state(checks.checks),
    }
}

/// The rejection reason implied by a change's readiness, for when GitHub blocks
/// a merge. Derived from the structured readiness, never parsed from a free-text
/// rejection body, so an agent always gets a typed reason (R16).
///
/// `NeedsApproval` and `Draft` are unreachable from GitHub data: [`readiness`]
/// never yields them from `mergeable_state` (a draft or unapproved PR reads as
/// `blocked`). They are part of the neutral contract for forges that surface
/// those states, and are mapped here so the function stays total over
/// [`MergeReadiness`].
fn rejection_reason(readiness: MergeReadiness) -> MergeRejectionReason {
    match readiness {
        MergeReadiness::Conflicted => MergeRejectionReason::Conflict,
        MergeReadiness::Behind => MergeRejectionReason::Behind,
        MergeReadiness::Blocked => MergeRejectionReason::Blocked,
        MergeReadiness::NeedsApproval => MergeRejectionReason::NeedsApproval,
        MergeReadiness::Draft => MergeRejectionReason::Draft,
        MergeReadiness::Ready | MergeReadiness::Unknown => MergeRejectionReason::Unknown,
    }
}

/// The GitHub forge: a project-scoped adapter over the [`GitHub`] client.
pub struct GitHubForge {
    client: GitHub,
    owner: String,
    repo: String,
}

impl GitHubForge {
    /// Wrap an existing client, scoped to one `owner`/`repo`.
    pub fn new(client: GitHub, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            client,
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    /// Build a GitHub forge from the ambient token ladder (env, keychain, `gh`),
    /// scoped to one `owner`/`repo`.
    pub fn from_env(
        owner: impl Into<String>,
        repo: impl Into<String>,
    ) -> Result<Self, GitHubError> {
        Ok(Self::new(GitHub::from_env()?, owner, repo))
    }
}

impl Forge for GitHubForge {
    fn current_user(&self) -> Result<String, ForgeError> {
        Ok(self.client.current_user()?)
    }

    fn create_change(&self, change: &SubmitChange) -> Result<Change, ForgeError> {
        let pr = self.client.create_pull_request(
            &self.owner,
            &self.repo,
            &NewPullRequest {
                title: change.title.clone(),
                head: change.head.clone(),
                base: change.base.clone(),
                body: change.body.clone(),
                draft: change.draft,
            },
        )?;
        Ok(to_change(pr))
    }

    fn update_change(&self, number: u64, update: &ChangeUpdate) -> Result<Change, ForgeError> {
        let pr = self.client.update_pull_request(
            &self.owner,
            &self.repo,
            number,
            &PullRequestUpdate {
                title: update.title.clone(),
                body: update.body.clone(),
                base: update.base.clone(),
            },
        )?;
        Ok(to_change(pr))
    }

    fn get_change(&self, number: u64) -> Result<Change, ForgeError> {
        Ok(to_change(
            self.client.get_pull_request(&self.owner, &self.repo, number)?,
        ))
    }

    fn get_change_within(&self, number: u64, timeout: Duration) -> Result<Change, ForgeError> {
        Ok(to_change(self.client.get_pull_request_within(
            &self.owner,
            &self.repo,
            number,
            timeout,
        )?))
    }

    fn change_for_branch(&self, branch: &str) -> Result<Option<Change>, ForgeError> {
        Ok(self
            .client
            .pull_request_for_branch(&self.owner, &self.repo, branch)?
            .map(to_change))
    }

    fn change_for_branch_within(
        &self,
        branch: &str,
        timeout: Duration,
    ) -> Result<Option<Change>, ForgeError> {
        Ok(self
            .client
            .pull_request_for_branch_within(&self.owner, &self.repo, branch, timeout)?
            .map(to_change))
    }

    fn change_for_branch_any_state(&self, branch: &str) -> Result<Option<Change>, ForgeError> {
        Ok(self
            .client
            .pull_request_for_branch_any_state(&self.owner, &self.repo, branch)?
            .map(to_change))
    }

    fn merge_change(&self, number: u64, opts: &MergeOptions) -> Result<MergeOutcome, ForgeError> {
        // GitHub squash-merges (the only mode in slice 2); `opts.squash` is the
        // contract's explicit acknowledgement of that, not a branch point.
        match self
            .client
            .merge_pull_request_with(&self.owner, &self.repo, number, opts.head_sha.as_deref())
        {
            Ok(outcome) => Ok(MergeOutcome {
                merged: outcome.merged,
                sha: outcome.sha,
            }),
            Err(GitHubError::NotMergeable) => {
                // Re-read the change so the rejection reason comes from its
                // structured readiness rather than an opaque "not mergeable".
                // Bound this enrichment probe so a slow forge cannot stall the
                // merge call; a failed re-read or an unmappable state yields
                // Unknown, and the merge is still correctly reported as rejected.
                //
                // Known limitation: when a head_sha assertion catches a moved
                // head, the re-read sees the new (clean) head and yields Unknown.
                // Naming that case (a HeadMoved reason) is deferred to the
                // merge-gate unit (U11); merge enforcement is unaffected here,
                // only the fidelity of the reported reason.
                let reason = self
                    .client
                    .get_pull_request_within(
                        &self.owner,
                        &self.repo,
                        number,
                        Duration::from_secs(10),
                    )
                    .map_or(MergeRejectionReason::Unknown, |pr| {
                        rejection_reason(readiness(pr.mergeable_state.as_deref()))
                    });
                Err(ForgeError::Rejected(reason))
            }
            Err(err) => Err(err.into()),
        }
    }

    fn change_checks(
        &self,
        numbers: &[u64],
        timeout: Duration,
    ) -> Result<BTreeMap<u64, ChangeStatus>, ForgeError> {
        let raw = self
            .client
            .pull_request_checks_within(&self.owner, &self.repo, numbers, timeout)?;
        Ok(raw
            .into_iter()
            .map(|(number, checks)| (number, to_status(checks)))
            .collect())
    }

    fn branch_protected(&self, branch: &str) -> Result<bool, ForgeError> {
        Ok(self
            .client
            .branch_protected(&self.owner, &self.repo, branch)?)
    }

    fn close_change(&self, number: u64) -> Result<Change, ForgeError> {
        Ok(to_change(
            self.client.close_pull_request(&self.owner, &self.repo, number)?,
        ))
    }

    fn rename_branch(&self, branch: &str, new_name: &str) -> Result<(), ForgeError> {
        Ok(self
            .client
            .rename_branch(&self.owner, &self.repo, branch, new_name)?)
    }

    fn capabilities(&self) -> Capabilities {
        // GitHub expresses an explicit "changes requested" review state.
        Capabilities {
            expresses_changes_requested: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use serde_json::json;

    fn forge(server: &MockServer) -> GitHubForge {
        GitHubForge::new(GitHub::with_base_url("t", server.base_url()), "o", "r")
    }

    #[test]
    fn readiness_flattens_only_the_states_stacc_acts_on() {
        assert_eq!(readiness(Some("clean")), MergeReadiness::Ready);
        assert_eq!(readiness(Some("dirty")), MergeReadiness::Conflicted);
        assert_eq!(readiness(Some("behind")), MergeReadiness::Behind);
        assert_eq!(readiness(Some("blocked")), MergeReadiness::Blocked);
        assert_eq!(readiness(Some("unstable")), MergeReadiness::Unknown);
        assert_eq!(readiness(Some("unknown")), MergeReadiness::Unknown);
        assert_eq!(readiness(None), MergeReadiness::Unknown);
    }

    #[test]
    fn change_state_maps_every_pr_state() {
        assert_eq!(change_state(PrState::Open), ChangeState::Open);
        assert_eq!(change_state(PrState::Closed), ChangeState::Closed);
        assert_eq!(change_state(PrState::Merged), ChangeState::Merged);
    }

    #[test]
    fn checks_state_keeps_no_checks_distinct_from_passed() {
        assert_eq!(checks_state(None), ChecksState::NoChecks);
        assert_eq!(checks_state(Some(CheckRollup::Pass)), ChecksState::Passed);
        assert_ne!(checks_state(None), checks_state(Some(CheckRollup::Pass)));
        assert_eq!(checks_state(Some(CheckRollup::Fail)), ChecksState::Failed);
        assert_eq!(checks_state(Some(CheckRollup::Pending)), ChecksState::Pending);
    }

    #[test]
    fn review_state_maps_every_decision_and_absence() {
        assert_eq!(review_state(None), ReviewState::NoReview);
        assert_eq!(review_state(Some(ReviewDecision::Approved)), ReviewState::Approved);
        assert_eq!(
            review_state(Some(ReviewDecision::ChangesRequested)),
            ReviewState::ChangesRequested
        );
        assert_eq!(
            review_state(Some(ReviewDecision::ReviewRequired)),
            ReviewState::ReviewRequired
        );
    }

    #[test]
    fn to_status_pairs_review_and_checks() {
        let status = to_status(PrChecks {
            review: Some(ReviewDecision::Approved),
            checks: None,
        });
        assert_eq!(status.review, ReviewState::Approved);
        assert_eq!(status.checks, ChecksState::NoChecks);
    }

    #[test]
    fn rejection_reason_derives_from_readiness() {
        assert_eq!(rejection_reason(MergeReadiness::Conflicted), MergeRejectionReason::Conflict);
        assert_eq!(rejection_reason(MergeReadiness::Behind), MergeRejectionReason::Behind);
        assert_eq!(rejection_reason(MergeReadiness::Blocked), MergeRejectionReason::Blocked);
        assert_eq!(
            rejection_reason(MergeReadiness::NeedsApproval),
            MergeRejectionReason::NeedsApproval
        );
        assert_eq!(rejection_reason(MergeReadiness::Draft), MergeRejectionReason::Draft);
        assert_eq!(rejection_reason(MergeReadiness::Ready), MergeRejectionReason::Unknown);
        assert_eq!(rejection_reason(MergeReadiness::Unknown), MergeRejectionReason::Unknown);
    }

    #[test]
    fn create_change_opens_and_maps_to_neutral() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/repos/o/r/pulls");
            then.status(201).json_body(json!({
                "number": 42,
                "html_url": "https://github.com/o/r/pull/42",
                "state": "open",
                "merged": false,
                "draft": true,
                "title": "T",
                "body": "B",
                "mergeable_state": "clean",
            }));
        });

        let change = forge(&server)
            .create_change(&SubmitChange {
                title: "T".into(),
                head: "feature".into(),
                base: "main".into(),
                body: "B".into(),
                draft: true,
            })
            .unwrap();
        assert_eq!(change.number, 42);
        assert_eq!(change.state, ChangeState::Open);
        assert_eq!(change.url, "https://github.com/o/r/pull/42");
        assert!(change.draft);
        assert_eq!(change.readiness, MergeReadiness::Ready);
        mock.assert();
    }

    #[test]
    fn get_change_maps_a_merged_pr() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/repos/o/r/pulls/7");
            then.status(200).json_body(json!({
                "number": 7, "html_url": "u", "state": "closed", "merged": true,
            }));
        });
        assert_eq!(forge(&server).get_change(7).unwrap().state, ChangeState::Merged);
    }

    #[test]
    fn merge_change_returns_the_outcome() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/repos/o/r/pulls/7/merge");
            then.status(200).json_body(json!({ "merged": true, "sha": "abc123" }));
        });
        let outcome = forge(&server)
            .merge_change(7, &MergeOptions { squash: true, head_sha: None })
            .unwrap();
        assert!(outcome.merged);
        assert_eq!(outcome.sha.as_deref(), Some("abc123"));
    }

    #[test]
    fn merge_change_passes_head_sha_assertion_through() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/repos/o/r/pulls/7/merge")
                .json_body(json!({ "merge_method": "squash", "sha": "deadbeef" }));
            then.status(200).json_body(json!({ "merged": true, "sha": "abc123" }));
        });
        forge(&server)
            .merge_change(7, &MergeOptions { squash: true, head_sha: Some("deadbeef".into()) })
            .unwrap();
        mock.assert();
    }

    #[test]
    fn merge_change_rejection_derives_a_structured_reason() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/repos/o/r/pulls/7/merge");
            then.status(405).json_body(json!({ "message": "not mergeable" }));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/repos/o/r/pulls/7");
            then.status(200).json_body(json!({
                "number": 7, "html_url": "u", "state": "open", "merged": false,
                "mergeable_state": "blocked",
            }));
        });
        let err = forge(&server)
            .merge_change(7, &MergeOptions { squash: true, head_sha: None })
            .unwrap_err();
        assert!(matches!(err, ForgeError::Rejected(MergeRejectionReason::Blocked)), "got {err:?}");
    }

    #[test]
    fn merge_change_rejection_maps_dirty_to_conflict() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/repos/o/r/pulls/8/merge");
            then.status(409).json_body(json!({ "message": "conflict" }));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/repos/o/r/pulls/8");
            then.status(200).json_body(json!({
                "number": 8, "html_url": "u", "state": "open", "merged": false,
                "mergeable_state": "dirty",
            }));
        });
        let err = forge(&server)
            .merge_change(8, &MergeOptions { squash: true, head_sha: None })
            .unwrap_err();
        assert!(matches!(err, ForgeError::Rejected(MergeRejectionReason::Conflict)), "got {err:?}");
    }

    #[test]
    fn merge_change_rejection_falls_back_to_unknown_when_reread_fails() {
        // A rejected merge whose enrichment re-read itself fails still reports a
        // rejection (the merge was refused), with reason Unknown.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/repos/o/r/pulls/7/merge");
            then.status(405).json_body(json!({ "message": "not mergeable" }));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/repos/o/r/pulls/7");
            then.status(500).json_body(json!({ "message": "boom" }));
        });
        let err = forge(&server)
            .merge_change(7, &MergeOptions { squash: true, head_sha: None })
            .unwrap_err();
        assert!(matches!(err, ForgeError::Rejected(MergeRejectionReason::Unknown)), "got {err:?}");
    }

    #[test]
    fn status_errors_map_to_neutral_forge_errors() {
        // GitHub HTTP statuses surface as the neutral error type an agent
        // branches on, with no forge discriminator.
        for (status, kind) in [(401u16, "auth"), (404u16, "not_found"), (429u16, "rate")] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(httpmock::Method::GET).path("/repos/o/r/pulls/7");
                then.status(status).json_body(json!({ "message": "x" }));
            });
            let err = forge(&server).get_change(7).unwrap_err();
            match kind {
                "auth" => assert!(matches!(err, ForgeError::AuthFailed), "status {status}: {err:?}"),
                "not_found" => assert!(matches!(err, ForgeError::NotFound), "status {status}: {err:?}"),
                "rate" => assert!(matches!(err, ForgeError::RateLimited), "status {status}: {err:?}"),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn forge_error_never_surfaces_a_token_bearing_body() {
        // A 500 whose body echoes a secret must reach the agent as a neutral
        // error carrying only GitHub's `message`, never the token (R18).
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/repos/o/r/pulls/7");
            then.status(500).json_body(json!({
                "message": "boom", "token": "ghp_SECRET",
            }));
        });
        let err = forge(&server).get_change(7).unwrap_err();
        let rendered = err.to_string();
        assert!(!rendered.contains("ghp_SECRET"), "leaked a token: {rendered}");
        assert!(rendered.contains("boom"), "dropped the safe message: {rendered}");
    }

    #[test]
    fn branch_protected_reads_false_on_404() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/repos/o/r/branches/main/protection");
            then.status(404).json_body(json!({ "message": "Branch not protected" }));
        });
        assert!(!forge(&server).branch_protected("main").unwrap());
    }

    #[test]
    fn change_checks_is_empty_without_numbers() {
        let server = MockServer::start();
        let checks = forge(&server)
            .change_checks(&[], Duration::from_secs(5))
            .unwrap();
        assert!(checks.is_empty());
    }

    #[test]
    fn capabilities_express_changes_requested() {
        let server = MockServer::start();
        assert!(forge(&server).capabilities().expresses_changes_requested);
    }
}
