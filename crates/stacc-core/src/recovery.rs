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
    /// The branches still to restack, the conflicting branch first.
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

    #[error("failed to read continuation: {0}")]
    Read(#[source] std::io::Error),

    #[error("corrupt continuation: {0}; run `git rebase --abort` to recover")]
    Corrupt(#[source] serde_json::Error),
}

const CONTINUE_FILE: &str = "stacc-continue.json";

/// Persist `op` so a later `continue`/`abort` can resume it. `git_dir` is the
/// repository's git directory (e.g. from `Git::git_dir`). Writes to a temp
/// sibling then atomically renames, so a crash mid-write never leaves a torn
/// record that would read back as [`RecoveryError::Corrupt`].
pub fn write_continuation(git_dir: &Path, op: &Operation) -> Result<(), RecoveryError> {
    // `Operation` is plain strings and vecs, so serialization is infallible.
    let json = serde_json::to_string(op).expect("Operation always serializes");
    let tmp = git_dir.join(format!(".{CONTINUE_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, json).map_err(RecoveryError::Write)?;
    std::fs::rename(&tmp, git_dir.join(CONTINUE_FILE)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RecoveryError::Write(e)
    })
}

/// Read the in-progress operation. A missing file is
/// [`RecoveryError::NotInProgress`]; any other I/O failure is
/// [`RecoveryError::Read`], so a real failure (e.g. a permission error on an
/// existing record) is not silently reported as "nothing in progress".
pub fn read_continuation(git_dir: &Path) -> Result<Operation, RecoveryError> {
    let text = match std::fs::read_to_string(git_dir.join(CONTINUE_FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(RecoveryError::NotInProgress)
        }
        Err(e) => return Err(RecoveryError::Read(e)),
    };
    serde_json::from_str(&text).map_err(RecoveryError::Corrupt)
}

/// Remove the continuation record and any leftover write temps. Best-effort: a
/// missing file is not an error. The temp sweep reclaims `.{file}.{pid}.tmp`
/// siblings orphaned by a crash between the temp write and the rename.
pub fn clear_continuation(git_dir: &Path) {
    let _ = std::fs::remove_file(git_dir.join(CONTINUE_FILE));
    let tmp_prefix = format!(".{CONTINUE_FILE}.");
    if let Ok(entries) = std::fs::read_dir(git_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&tmp_prefix) && name.ends_with(".tmp") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
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
        let all = ops();
        assert_eq!(all[0].remaining(), ["a", "b"]); // Sync
        assert_eq!(all[1].remaining(), ["a"]); // Restack
        assert_eq!(all[2].remaining(), ["b", "c"]); // Modify
        assert_eq!(all[3].remaining(), ["b"]); // Move
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

    #[test]
    fn writes_the_tagged_wire_format() {
        let dir = TempDir::new().unwrap();
        write_continuation(
            dir.path(),
            &Operation::Sync {
                remaining: vec!["a".into(), "b".into()],
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(dir.path().join(CONTINUE_FILE)).unwrap();
        assert!(raw.contains(r#""op":"sync""#), "got: {raw}");
        assert!(raw.contains(r#""remaining":["a","b"]"#), "got: {raw}");
    }

    #[test]
    fn legacy_bare_array_is_corrupt_not_misread() {
        // The pre-U2 format was a bare JSON array; it must surface as a
        // structured error, never deserialize into something wrong.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(CONTINUE_FILE), r#"["a","b"]"#).unwrap();
        assert!(matches!(
            read_continuation(dir.path()),
            Err(RecoveryError::Corrupt(_))
        ));
    }

    #[test]
    fn write_leaves_no_temp_file() {
        let dir = TempDir::new().unwrap();
        write_continuation(
            dir.path(),
            &Operation::Sync {
                remaining: vec!["a".into()],
            },
        )
        .unwrap();
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray, "temp file left behind after write");
    }

    #[test]
    fn non_notfound_read_error_is_not_masked_as_not_in_progress() {
        // A directory at the continuation path makes the read fail with a
        // non-NotFound error, which must surface as Read, not NotInProgress.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(CONTINUE_FILE)).unwrap();
        assert!(matches!(
            read_continuation(dir.path()),
            Err(RecoveryError::Read(_))
        ));
    }

    #[test]
    fn corrupt_message_names_the_escape_hatch() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(CONTINUE_FILE), "{not json").unwrap();
        let msg = read_continuation(dir.path()).unwrap_err().to_string();
        assert!(msg.contains("git rebase --abort"), "hint missing: {msg}");
    }

    #[test]
    fn clear_sweeps_orphaned_temp_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".stacc-continue.json.99999.tmp"), "x").unwrap();
        clear_continuation(dir.path());
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray, "orphaned temp not swept");
    }
}
