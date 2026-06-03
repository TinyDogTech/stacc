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

    #[error("rebase conflict on `{branch}`; resolve it, then re-run `stacc sync`")]
    #[diagnostic(code(stacc::conflict))]
    Conflict { branch: String },

    #[error("{0}")]
    #[diagnostic(code(stacc::usage))]
    Usage(String),
}

// The operations engine has its own error type so `stacc-core` stays off the
// CLI crate; map it onto the user-facing `Error`. `Conflict` is never mapped
// here — `restack_with_recovery` intercepts it to write the recovery artifacts,
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

impl Error {
    /// The machine-readable JSON form, used by `--format json`.
    pub fn as_json(&self) -> Value {
        match self {
            Error::Config(err) => json!({ "error": "config", "message": err.to_string() }),
            Error::State(err) => json!({ "error": "state", "message": err.to_string() }),
            Error::Git(err) => json!({ "error": "git", "message": err.to_string() }),
            Error::Github(err) => json!({ "error": "github", "message": err.to_string() }),
            Error::Conflict { branch } => json!({ "error": "conflict", "branch": branch }),
            Error::Usage(msg) => json!({ "error": "usage", "message": msg }),
        }
    }
}
