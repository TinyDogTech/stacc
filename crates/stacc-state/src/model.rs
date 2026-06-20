//! The serializable shapes that make up stacc's stored state.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Repository-level configuration, stored at the `repo` key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub trunk: String,
    pub remote: String,
    /// Branches the user has declined to track via `stacc sync`. Excluded from
    /// the untracked set on subsequent syncs until `stacc track` clears the entry.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub declined_tracking: BTreeSet<String>,
}

/// The branch (and the commit on it) that a tracked branch is stacked on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub name: String,
    pub hash: String,
}

/// The pull request associated with a tracked branch, once submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// State for one tracked branch, stored at `branches/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchState {
    pub base: Base,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullRequest>,
    /// Caller-supplied PR title, persisted across re-submits. Overrides the
    /// commit subject when set; falls back to commit subject when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_title: Option<String>,
    /// Caller-supplied PR body, persisted across re-submits. Overrides the
    /// commit body when set; falls back to commit body when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_description: Option<String>,
}

/// A record of a branch dropped by `stacc merged`, kept in the `disposals` blob
/// so a wrong drop is diagnosable and, via its keep-alive ref, recoverable.
/// Keyed in state by branch and tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disposal {
    /// The dropped branch.
    pub branch: String,
    /// The branch tip at drop time; the keep-alive ref points here.
    pub tip: String,
    /// The surviving base its children were re-parented onto (stack shape).
    pub base: String,
    /// The children re-parented off the dropped branch (stack shape).
    #[serde(default)]
    pub children: Vec<String>,
    /// The merge signal that authorized the drop: `ancestor`, `same_tree`,
    /// `net_diff`, or `assume_merged`.
    pub evidence: String,
    /// Unix-millis when the branch was dropped. Retention prunes keep-alive refs
    /// by this drop time, never by the dropped tip's commit date (a long-lived
    /// branch can have an old tip). `#[serde(default)]` => 0 for pre-slice records,
    /// which sort oldest. Best-effort: 0 if the clock is unavailable.
    #[serde(default)]
    pub dropped_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_state_roundtrips_through_json() {
        let state = BranchState {
            base: Base {
                name: "main".into(),
                hash: "abc123".into(),
            },
            pr: Some(PullRequest {
                number: 7,
                url: None,
            }),
            pr_title: None,
            pr_description: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: BranchState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn pr_field_omitted_when_absent() {
        let state = BranchState {
            base: Base {
                name: "main".into(),
                hash: "abc123".into(),
            },
            pr: None,
            pr_title: None,
            pr_description: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("pr"));
    }

    #[test]
    fn missing_optional_fields_default_to_none() {
        let json = r#"{"base":{"name":"main","hash":"abc"}}"#;
        let state: BranchState = serde_json::from_str(json).unwrap();
        assert_eq!(state.pr, None);
        assert_eq!(state.pr_title, None);
        assert_eq!(state.pr_description, None);
    }

    #[test]
    fn pr_title_and_description_roundtrip() {
        let state = BranchState {
            base: Base {
                name: "main".into(),
                hash: "abc123".into(),
            },
            pr: None,
            pr_title: Some("Custom title".into()),
            pr_description: Some("Custom body".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("pr_title"), "pr_title in json: {json}");
        assert!(json.contains("pr_description"), "pr_description in json: {json}");
        let back: BranchState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pr_title.as_deref(), Some("Custom title"));
        assert_eq!(back.pr_description.as_deref(), Some("Custom body"));
    }

    #[test]
    fn pr_title_and_description_omitted_when_absent() {
        let state = BranchState {
            base: Base {
                name: "main".into(),
                hash: "abc123".into(),
            },
            pr: None,
            pr_title: None,
            pr_description: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("pr_title"), "absent pr_title omitted: {json}");
        assert!(!json.contains("pr_description"), "absent pr_description omitted: {json}");
    }

    #[test]
    fn repo_config_roundtrips() {
        let cfg = RepoConfig {
            trunk: "main".into(),
            remote: "origin".into(),
            declined_tracking: BTreeSet::new(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<RepoConfig>(&json).unwrap(), cfg);
    }

    #[test]
    fn repo_config_backward_compat_no_declined_key() {
        // Blobs written before STA-88 have no declined_tracking key.
        let json = r#"{"trunk":"main","remote":"origin"}"#;
        let cfg: RepoConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.trunk, "main");
        assert_eq!(cfg.remote, "origin");
        assert!(cfg.declined_tracking.is_empty());
    }

    #[test]
    fn repo_config_declined_tracking_roundtrips() {
        let mut cfg = RepoConfig {
            trunk: "main".into(),
            remote: "origin".into(),
            declined_tracking: BTreeSet::new(),
        };
        cfg.declined_tracking.insert("jillian/wip".into());
        cfg.declined_tracking.insert("jillian/old-exp".into());
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RepoConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.declined_tracking, cfg.declined_tracking);
    }

    #[test]
    fn repo_config_empty_declined_omitted_from_json() {
        let cfg = RepoConfig {
            trunk: "main".into(),
            remote: "origin".into(),
            declined_tracking: BTreeSet::new(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("declined_tracking"), "empty set must be omitted: {json}");
    }
}
