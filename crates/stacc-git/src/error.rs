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
