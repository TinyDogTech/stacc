//! Conflict recovery: the continuation record that lets a stopped operation
//! resume. When a restack hits a conflict, the caller persists an [`Operation`]
//! describing what was in flight and what remains; `continue` reads it back and
//! finishes, `abort` reads it back and rolls history-rewriting operations back
//! to their anchor.
//!
//! The record lives in a file under the git dir (alongside git's own
//! `rebase-merge/`); the GitHub-enriched conflict-*context* file is written
//! separately by the CLI so this module stays forge-agnostic.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The operation that was in flight when a conflict stopped it. The variant
/// identifies how `continue`/`abort` should finish or unwind; every variant
/// carries the remaining restack queue (conflicting branch first), and the
/// history-rewriting ones carry a rollback anchor for `abort`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// `sync`'s restack pass.
    Sync { remaining: Vec<String> },
    /// A standalone `restack`.
    Restack { remaining: Vec<String> },
    /// `modify`: amended the current branch's tip; `pre_amend` is the tip before
    /// the amend, restored on abort.
    Modify {
        remaining: Vec<String>,
        pre_amend: String,
    },
    /// `move`: re-parented the current branch; `pre_base` is the recorded base
    /// before the move, restored on abort.
    Move {
        remaining: Vec<String>,
        pre_base: String,
    },
}

impl Operation {
    /// The branches still to restack — the conflicting branch first.
    pub fn remaining(&self) -> &[String] {
        match self {
            Operation::Sync { remaining }
            | Operation::Restack { remaining }
            | Operation::Modify { remaining, .. }
            | Operation::Move { remaining, .. } => remaining,
        }
    }
}

/// Failures reading or writing the continuation record.
#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("no operation in progress to continue")]
    NotInProgress,

    #[error("failed to write continuation: {0}")]
    Write(#[source] std::io::Error),

    #[error("corrupt continuation: {0}")]
    Corrupt(#[source] serde_json::Error),
}

const CONTINUE_FILE: &str = "stacc-continue.json";

/// Persist `op` so a later `continue`/`abort` can resume it. `git_dir` is the
/// repository's git directory (e.g. from `Git::git_dir`).
pub fn write_continuation(git_dir: &Path, op: &Operation) -> Result<(), RecoveryError> {
    let json = serde_json::to_string(op).map_err(RecoveryError::Corrupt)?;
    std::fs::write(git_dir.join(CONTINUE_FILE), json).map_err(RecoveryError::Write)
}

/// Read the in-progress operation, or [`RecoveryError::NotInProgress`] if there
/// is none.
pub fn read_continuation(git_dir: &Path) -> Result<Operation, RecoveryError> {
    let text = std::fs::read_to_string(git_dir.join(CONTINUE_FILE))
        .map_err(|_| RecoveryError::NotInProgress)?;
    serde_json::from_str(&text).map_err(RecoveryError::Corrupt)
}

/// Remove the continuation record. Best-effort: a missing file is not an error.
pub fn clear_continuation(git_dir: &Path) {
    let _ = std::fs::remove_file(git_dir.join(CONTINUE_FILE));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ops() -> Vec<Operation> {
        vec![
            Operation::Sync {
                remaining: vec!["a".into(), "b".into()],
            },
            Operation::Restack {
                remaining: vec!["a".into()],
            },
            Operation::Modify {
                remaining: vec!["b".into(), "c".into()],
                pre_amend: "deadbeef".into(),
            },
            Operation::Move {
                remaining: vec!["b".into()],
                pre_base: "cafef00d".into(),
            },
        ]
    }

    #[test]
    fn each_operation_round_trips_through_the_file() {
        let dir = TempDir::new().unwrap();
        for op in ops() {
            write_continuation(dir.path(), &op).unwrap();
            assert_eq!(read_continuation(dir.path()).unwrap(), op);
        }
    }

    #[test]
    fn remaining_exposes_the_queue_for_every_variant() {
        assert_eq!(
            Operation::Modify {
                remaining: vec!["x".into(), "y".into()],
                pre_amend: "h".into(),
            }
            .remaining(),
            ["x", "y"]
        );
    }

    #[test]
    fn read_with_no_file_is_not_in_progress() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            read_continuation(dir.path()),
            Err(RecoveryError::NotInProgress)
        ));
    }

    #[test]
    fn corrupt_file_is_a_structured_error_not_a_panic() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(CONTINUE_FILE), "{not json").unwrap();
        assert!(matches!(
            read_continuation(dir.path()),
            Err(RecoveryError::Corrupt(_))
        ));
    }

    #[test]
    fn clear_removes_the_file_and_tolerates_absence() {
        let dir = TempDir::new().unwrap();
        clear_continuation(dir.path()); // no file yet: no-op
        write_continuation(
            dir.path(),
            &Operation::Sync {
                remaining: vec!["a".into()],
            },
        )
        .unwrap();
        clear_continuation(dir.path());
        assert!(matches!(
            read_continuation(dir.path()),
            Err(RecoveryError::NotInProgress)
        ));
    }
}
