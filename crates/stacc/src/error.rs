//! Error types for the CLI.
//!
//! `thiserror` derives the standard `Error` trait from an enum, and miette's
//! `Diagnostic` adds an error code and a help note for pretty rendering. The
//! `as_json` method produces the machine-readable form for `--format json`.

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
}

impl Error {
    /// The machine-readable JSON form, used by `--format json`.
    pub fn as_json(&self) -> Value {
        match self {
            Error::NotImplemented(command) => json!({
                "error": "not_implemented",
                "command": command,
            }),
        }
    }
}
