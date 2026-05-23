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
    /// A command exists in the CLI but has no behavior yet.
    #[error("`{0}` is not implemented yet")]
    #[diagnostic(
        code(stacc::not_implemented),
        help("This command is scaffolded but not wired up yet.")
    )]
    NotImplemented(&'static str),

    #[error(transparent)]
    Config(#[from] stacc_config::ConfigError),

    #[error(transparent)]
    State(#[from] stacc_state::StateError),
}

impl Error {
    /// The machine-readable JSON form, used by `--format json`.
    pub fn as_json(&self) -> Value {
        match self {
            Error::NotImplemented(command) => json!({
                "error": "not_implemented",
                "command": command,
            }),
            Error::Config(err) => json!({ "error": "config", "message": err.to_string() }),
            Error::State(err) => json!({ "error": "state", "message": err.to_string() }),
        }
    }
}
