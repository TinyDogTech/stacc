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
// CLI crate; map it onto the user-facing `Error`. `Conflict` carries a
// `remaining` queue the engine doesn't surface here — the caller that persists
// recovery artifacts handles that variant directly and only falls back to this
// mapping for the non-conflict cases.
impl From<stacc_core::ops::OpsError> for Error {
    fn from(err: stacc_core::ops::OpsError) -> Self {
        use stacc_core::ops::OpsError;
        match err {
            OpsError::Git(e) => Error::Git(e),
            OpsError::State(e) => Error::State(e),
            OpsError::Conflict { branch, .. } => Error::Conflict { branch },
            OpsError::ForkPointLost { branch, base } => Error::Usage(format!(
                "cannot recover the fork point of `{branch}` from `{base}`; rebase manually"
            )),
            OpsError::Untracked(name) => {
                Error::Usage(format!("branch `{name}` is not tracked; run `stacc track` first"))
            }
            OpsError::Cycle(name) => {
                Error::Usage(format!("circular base chain reached at `{name}`"))
            }
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
