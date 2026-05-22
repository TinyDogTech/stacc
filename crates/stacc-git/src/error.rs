use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("failed to spawn git: {source}")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    #[error("git {args:?} exited with {status:?}: {stderr}")]
    Command {
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
}

/// A rebase that stopped on a conflict and is waiting to be resolved.
#[derive(Debug, Error)]
#[error("rebase of `{branch}` stopped on a conflict")]
pub struct RebaseInterrupt {
    pub branch: String,
}

/// The outcome of a rebase that didn't succeed: either an ordinary git failure
/// or a recoverable conflict the caller can resolve and continue.
#[derive(Debug, Error)]
pub enum RebaseError {
    #[error(transparent)]
    Git(#[from] GitError),

    #[error(transparent)]
    Interrupt(#[from] RebaseInterrupt),
}
