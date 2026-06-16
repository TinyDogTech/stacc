//! Error types for the CLI.
//!
//! `thiserror` derives the standard `Error` trait, miette's `Diagnostic` adds
//! pretty rendering, and `as_json` produces the machine-readable form for
//! `--format json`. Errors from the library crates are wrapped via `#[from]`.

use miette::Diagnostic;
use serde_json::{json, Value};
use stacc_forge::SCHEMA_VERSION;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] stacc_config::ConfigError),

    #[error(transparent)]
    State(stacc_state::StateError),

    /// The state-ref compare-and-swap lost every retry: another process kept
    /// updating the same stack. Distinct from `State` so an agent can recognize
    /// contention by code and retry, rather than treating it as a hard failure.
    #[error("{0}")]
    #[diagnostic(code(stacc::contention))]
    Contention(String),

    #[error(transparent)]
    Git(#[from] stacc_git::GitError),

    #[error(transparent)]
    Github(#[from] stacc_github::GitHubError),

    #[error("rebase conflict on `{branch}`; resolve it, then run `stacc continue` (or `stacc abort` to undo)")]
    #[diagnostic(code(stacc::conflict))]
    Conflict { branch: String },

    #[error("{0}")]
    #[diagnostic(code(stacc::usage))]
    Usage(String),

    /// Nothing to continue or abort. Distinct from `Usage` so an agent polling
    /// `continue`/`abort` can recognize the no-op condition by code.
    #[error("{0}")]
    #[diagnostic(code(stacc::not_in_progress))]
    NotInProgress(String),

    /// A navigation step has multiple candidates and stacc cannot choose without
    /// a prompt; the caller should pick one of `choices` and check it out.
    #[error("multiple choices: {}; check out one directly", choices.join(", "))]
    #[diagnostic(code(stacc::ambiguous))]
    Ambiguous { choices: Vec<String> },

    /// A focused operation would rewrite `branch`, but it is checked out in
    /// another `worktree`; rewriting it there would desync that worktree. The
    /// caller should finish or relocate that branch first.
    #[error("`{branch}` is checked out in another worktree ({worktree}); finish or move it there first, or run from that worktree")]
    #[diagnostic(code(stacc::worktree_conflict))]
    WorktreeConflict { branch: String, worktree: String },
}

// Map the state layer's errors onto the user-facing `Error`. Contention gets its
// own discriminator (so an agent can branch on it and retry); everything else
// stays transparent under `State`. A manual impl is required because singling
// out one variant rules out a blanket `#[from]`.
impl From<stacc_state::StateError> for Error {
    fn from(err: stacc_state::StateError) -> Self {
        match err {
            stacc_state::StateError::Contention { attempts } => Error::Contention(format!(
                "state ref contention: gave up after {attempts} attempts; another agent is updating the same stack, retry"
            )),
            other => Error::State(other),
        }
    }
}

// The operations engine has its own error type so `stacc-core` stays off the
// CLI crate; map it onto the user-facing `Error`. `Conflict` is never mapped
// here, `restack_with_recovery` intercepts it to write the recovery artifacts,
// so reaching that arm means a caller wrongly `?`-propagated the engine; we fail
// loud rather than silently drop the resume queue.
impl From<stacc_core::ops::OpsError> for Error {
    fn from(err: stacc_core::ops::OpsError) -> Self {
        use stacc_core::ops::OpsError;
        match err {
            OpsError::Git(e) => Error::Git(e),
            OpsError::State(e) => Error::State(e),
            OpsError::Conflict { .. } => {
                unreachable!("OpsError::Conflict must be handled by restack_with_recovery")
            }
            // ForkPointLost / Untracked / Cycle reuse OpsError's own `Display`
            // so the user-facing message has a single source of truth.
            other => Error::Usage(other.to_string()),
        }
    }
}

// Continuation read/write failures surface as usage errors, reusing
// RecoveryError's own `Display`.
impl From<stacc_core::recovery::RecoveryError> for Error {
    fn from(err: stacc_core::recovery::RecoveryError) -> Self {
        use stacc_core::recovery::RecoveryError;
        match err {
            RecoveryError::NotInProgress => {
                Error::NotInProgress("no operation in progress to continue".into())
            }
            other => Error::Usage(other.to_string()),
        }
    }
}

impl Error {
    /// The machine-readable JSON form, used by `--format json`.
    pub fn as_json(&self) -> Value {
        match self {
            Error::Config(err) => {
                json!({ "type": "config", "message": err.to_string(), "schema_version": SCHEMA_VERSION })
            }
            Error::State(err) => {
                json!({ "type": "state", "message": err.to_string(), "schema_version": SCHEMA_VERSION })
            }
            Error::Contention(msg) => {
                json!({ "type": "contention", "message": msg, "schema_version": SCHEMA_VERSION })
            }
            Error::Git(err) => {
                json!({ "type": "git", "message": err.to_string(), "schema_version": SCHEMA_VERSION })
            }
            // The forge error carries the neutral type/reason envelope (its body
            // was scrubbed in U4); no `github` discriminator survives, and the
            // envelope already stamps `schema_version`.
            Error::Github(err) => {
                let envelope = stacc_forge::ForgeError::from(err).to_envelope();
                serde_json::to_value(&envelope).unwrap_or_else(|_| {
                    json!({ "type": "unexpected", "message": err.to_string(), "schema_version": SCHEMA_VERSION })
                })
            }
            Error::Conflict { branch } => json!({
                "type": "conflict",
                "branch": branch,
                "continue": "stacc continue",
                "abort": "stacc abort",
                "schema_version": SCHEMA_VERSION,
            }),
            Error::Usage(msg) => {
                json!({ "type": "usage", "message": msg, "schema_version": SCHEMA_VERSION })
            }
            Error::NotInProgress(msg) => {
                json!({ "type": "not_in_progress", "message": msg, "schema_version": SCHEMA_VERSION })
            }
            Error::Ambiguous { choices } => json!({
                "type": "ambiguous",
                "message": format!("multiple choices: {}; check out one directly", choices.join(", ")),
                "choices": choices,
                "schema_version": SCHEMA_VERSION,
            }),
            Error::WorktreeConflict { branch, worktree } => json!({
                "type": "worktree_conflict",
                "branch": branch,
                "worktree": worktree,
                "schema_version": SCHEMA_VERSION,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contention_maps_to_its_own_discriminator() {
        let err: Error = stacc_state::StateError::Contention { attempts: 5 }.into();
        assert!(matches!(err, Error::Contention(_)));
        assert_eq!(err.as_json()["type"].as_str(), Some("contention"));
        assert_eq!(err.as_json()["schema_version"].as_u64(), Some(u64::from(SCHEMA_VERSION)));
    }

    #[test]
    fn other_state_errors_stay_transparent() {
        let json_err = serde_json::from_str::<i32>("not a number").unwrap_err();
        let err: Error = stacc_state::StateError::Json(json_err).into();
        assert!(matches!(err, Error::State(_)));
        assert_eq!(err.as_json()["type"].as_str(), Some("state"));
    }
}
