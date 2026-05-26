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
