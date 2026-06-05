//! Error types for the CLI.
//!
//! `thiserror` derives the standard `Error` trait, miette's `Diagnostic` adds
//! pretty rendering, and `as_json` produces the machine-readable form for
//! `--format json`. Errors from the library crates are wrapped via `#[from]`.

use miette::Diagnostic;
use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] stacc_config::ConfigError),

    #[error(transparent)]
    State(#[from] stacc_state::StateError),

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
            Error::Config(err) => json!({ "error": "config", "message": err.to_string() }),
            Error::State(err) => json!({ "error": "state", "message": err.to_string() }),
            Error::Git(err) => json!({ "error": "git", "message": err.to_string() }),
            Error::Github(err) => json!({ "error": "github", "message": err.to_string() }),
            Error::Conflict { branch } => json!({
                "error": "conflict",
                "branch": branch,
                "continue": "stacc continue",
                "abort": "stacc abort",
            }),
            Error::Usage(msg) => json!({ "error": "usage", "message": msg }),
            Error::NotInProgress(msg) => json!({ "error": "not_in_progress", "message": msg }),
            Error::Ambiguous { choices } => json!({ "error": "ambiguous", "choices": choices }),
        }
    }
}
