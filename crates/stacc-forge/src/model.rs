//! The forge-neutral change vocabulary: the types every forge maps its own API
//! onto. These names are the agent-facing JSON contract (consumed by the CLI in
//! a later unit), so the serde representations decided here are stable.

use serde::{Deserialize, Serialize};

/// The lifecycle state of a change (a GitHub PR or a GitLab MR).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    Open,
    Merged,
    Closed,
}

/// The CI/checks rollup for a change's head commit.
///
/// `NoChecks` is a distinct value from `Passed`: it means no CI is configured,
/// which the merge gate's no-CI guard must be able to tell apart from "all
/// checks passed". Collapsing the two would let a CI-less repo read as green.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Pending,
    Passed,
    Failed,
    NoChecks,
}

/// The review/approval state of a change.
///
/// Display-only: the merge gate never consumes it. A forge that cannot express
/// a given state (GitLab has no "changes requested") signals that through
/// [`crate::Capabilities`], so absence is never misread as "no objections".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    ReviewRequired,
    NoReview,
}

/// A coarse, display-only readiness signal, flattened from each forge's
/// structured readiness field (GitHub `mergeable_state`, GitLab
/// `detailed_merge_status`).
///
/// It carries the human `behind`/`dirty`/`blocked` hint and is the single
/// source for [`MergeRejectionReason`]. It never gates the merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeReadiness {
    Ready,
    Conflicted,
    Behind,
    Blocked,
    NeedsApproval,
    Draft,
    Unknown,
}

/// Why a forge refused to merge a change.
///
/// Derived from the structured readiness field at merge time (never parsed from
/// a free-text rejection body), so an agent is never left with an opaque
/// rejection (R16). There is no `Ready` variant: a ready change is not rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeRejectionReason {
    Conflict,
    Behind,
    Blocked,
    NeedsApproval,
    Draft,
    Unknown,
}

/// A change (a GitHub PR or a GitLab MR) as stacc cares about it: identity,
/// lifecycle state, and the content stacc renders or updates.
///
/// `readiness` replaces today's GitHub-specific `mergeable_state` string;
/// [`MergeReadiness::Unknown`] is the "not computed / absent" case, so the field
/// is non-optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub number: u64,
    pub url: String,
    pub state: ChangeState,
    pub title: String,
    pub body: String,
    /// Whether the change is a draft.
    pub draft: bool,
    /// Coarse, display-only merge readiness; never gates.
    pub readiness: MergeReadiness,
}

/// The review and checks status for a change, read separately from its core
/// state (mirrors today's batched status query). Each field carries the forge's
/// "nothing reported" value ([`ReviewState::NoReview`] / [`ChecksState::NoChecks`])
/// rather than an `Option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeStatus {
    pub review: ReviewState,
    pub checks: ChecksState,
}

/// Fields for opening a new change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitChange {
    pub title: String,
    /// The branch the change is opened from.
    pub head: String,
    /// The branch the change targets (its parent in the stack).
    pub base: String,
    pub body: String,
    /// Open the change as a draft.
    pub draft: bool,
}

/// Fields to change on an existing change. Unset fields are left as-is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// Options for merging a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeOptions {
    /// Squash-merge (slice 2 always squashes; the flag is explicit in the
    /// contract so a later non-squash mode is additive).
    pub squash: bool,
    /// Assert the change's head is at this SHA before merging, so a moved head
    /// is rejected rather than silently merging stale work. `None` skips the
    /// assertion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
}

/// The outcome of a successful merge: whether the forge reports it merged, and
/// the squash commit SHA when it does. A *blocked* merge is not an outcome; it
/// surfaces as [`crate::ForgeError::Rejected`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeOutcome {
    pub merged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize a value to its bare JSON string form (drops the surrounding
    /// quotes serde adds for a string), for asserting enum wire names.
    fn wire<T: Serialize>(value: &T) -> String {
        serde_json::to_value(value).unwrap().as_str().unwrap().to_string()
    }

    #[test]
    fn change_state_serializes_to_neutral_strings() {
        assert_eq!(wire(&ChangeState::Open), "open");
        assert_eq!(wire(&ChangeState::Merged), "merged");
        assert_eq!(wire(&ChangeState::Closed), "closed");
    }

    #[test]
    fn checks_state_serializes_to_neutral_strings() {
        assert_eq!(wire(&ChecksState::Pending), "pending");
        assert_eq!(wire(&ChecksState::Passed), "passed");
        assert_eq!(wire(&ChecksState::Failed), "failed");
        assert_eq!(wire(&ChecksState::NoChecks), "no_checks");
    }

    #[test]
    fn no_checks_is_distinct_from_passed() {
        // The gate's no-CI branch depends on telling these apart, in the type
        // system and on the wire.
        assert_ne!(ChecksState::NoChecks, ChecksState::Passed);
        assert_ne!(wire(&ChecksState::NoChecks), wire(&ChecksState::Passed));
    }

    #[test]
    fn review_state_serializes_to_neutral_strings() {
        assert_eq!(wire(&ReviewState::Approved), "approved");
        assert_eq!(wire(&ReviewState::ChangesRequested), "changes_requested");
        assert_eq!(wire(&ReviewState::ReviewRequired), "review_required");
        assert_eq!(wire(&ReviewState::NoReview), "no_review");
    }

    #[test]
    fn merge_readiness_serializes_to_neutral_strings() {
        assert_eq!(wire(&MergeReadiness::Ready), "ready");
        assert_eq!(wire(&MergeReadiness::Conflicted), "conflicted");
        assert_eq!(wire(&MergeReadiness::Behind), "behind");
        assert_eq!(wire(&MergeReadiness::Blocked), "blocked");
        assert_eq!(wire(&MergeReadiness::NeedsApproval), "needs_approval");
        assert_eq!(wire(&MergeReadiness::Draft), "draft");
        assert_eq!(wire(&MergeReadiness::Unknown), "unknown");
    }

    #[test]
    fn merge_rejection_reason_serializes_to_neutral_strings() {
        assert_eq!(wire(&MergeRejectionReason::Conflict), "conflict");
        assert_eq!(wire(&MergeRejectionReason::Behind), "behind");
        assert_eq!(wire(&MergeRejectionReason::Blocked), "blocked");
        assert_eq!(wire(&MergeRejectionReason::NeedsApproval), "needs_approval");
        assert_eq!(wire(&MergeRejectionReason::Draft), "draft");
        assert_eq!(wire(&MergeRejectionReason::Unknown), "unknown");
    }

    #[test]
    fn enums_round_trip_through_json() {
        for state in [ChangeState::Open, ChangeState::Merged, ChangeState::Closed] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<ChangeState>(&json).unwrap(), state);
        }
        for checks in [
            ChecksState::Pending,
            ChecksState::Passed,
            ChecksState::Failed,
            ChecksState::NoChecks,
        ] {
            let json = serde_json::to_string(&checks).unwrap();
            assert_eq!(serde_json::from_str::<ChecksState>(&json).unwrap(), checks);
        }
        for review in [
            ReviewState::Approved,
            ReviewState::ChangesRequested,
            ReviewState::ReviewRequired,
            ReviewState::NoReview,
        ] {
            let json = serde_json::to_string(&review).unwrap();
            assert_eq!(serde_json::from_str::<ReviewState>(&json).unwrap(), review);
        }
        for readiness in [
            MergeReadiness::Ready,
            MergeReadiness::Conflicted,
            MergeReadiness::Behind,
            MergeReadiness::Blocked,
            MergeReadiness::NeedsApproval,
            MergeReadiness::Draft,
            MergeReadiness::Unknown,
        ] {
            let json = serde_json::to_string(&readiness).unwrap();
            assert_eq!(serde_json::from_str::<MergeReadiness>(&json).unwrap(), readiness);
        }
        for reason in [
            MergeRejectionReason::Conflict,
            MergeRejectionReason::Behind,
            MergeRejectionReason::Blocked,
            MergeRejectionReason::NeedsApproval,
            MergeRejectionReason::Draft,
            MergeRejectionReason::Unknown,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(serde_json::from_str::<MergeRejectionReason>(&json).unwrap(), reason);
        }
    }

    #[test]
    fn change_round_trips_through_json() {
        let change = Change {
            number: 7,
            url: "https://example.test/changes/7".into(),
            state: ChangeState::Open,
            title: "Add the thing".into(),
            body: "Body".into(),
            draft: false,
            readiness: MergeReadiness::Ready,
        };
        let json = serde_json::to_string(&change).unwrap();
        assert_eq!(serde_json::from_str::<Change>(&json).unwrap(), change);
    }

    #[test]
    fn change_status_round_trips_through_json() {
        let status = ChangeStatus {
            review: ReviewState::Approved,
            checks: ChecksState::Passed,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<ChangeStatus>(&json).unwrap(), status);
    }

    #[test]
    fn merge_options_omits_absent_head_sha() {
        let opts = MergeOptions {
            squash: true,
            head_sha: None,
        };
        let json = serde_json::to_string(&opts).unwrap();
        assert!(!json.contains("head_sha"), "absent head_sha must be omitted: {json}");

        let with_sha = MergeOptions {
            squash: true,
            head_sha: Some("abc123".into()),
        };
        let json = serde_json::to_string(&with_sha).unwrap();
        assert_eq!(serde_json::from_str::<MergeOptions>(&json).unwrap(), with_sha);
    }
}
