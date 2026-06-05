use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("no GitHub token found; run `stacc auth login` or set GITHUB_TOKEN")]
    MissingToken,

    #[error("GitHub API returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("pull request is not mergeable (head moved, or required checks/reviews not satisfied)")]
    NotMergeable,

    #[error("HTTP transport error: {0}")]
    Transport(String),

    #[error("failed to decode GitHub response: {0}")]
    Decode(#[from] std::io::Error),

    #[error("unexpected GitHub response: {0}")]
    Unexpected(String),

    #[error("device flow expired before authorization completed")]
    DeviceFlowExpired,

    #[error("device flow was denied by the user")]
    DeviceFlowDenied,

    #[error("keyring error: {0}")]
    Keyring(String),
}

impl GitHubError {
    /// Convert a `ureq::Error` into our typed error, capturing the response
    /// body on non-2xx statuses (ureq treats those as errors).
    pub(crate) fn from_ureq(err: ureq::Error) -> Self {
        match err {
            ureq::Error::Status(status, response) => {
                let body = response.into_string().unwrap_or_default();
                GitHubError::Status { status, body }
            }
            ureq::Error::Transport(transport) => GitHubError::Transport(transport.to_string()),
        }
    }
}
