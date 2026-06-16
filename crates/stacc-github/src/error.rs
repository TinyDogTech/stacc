use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("no GitHub token found; run `stacc auth login`, set GITHUB_TOKEN, or log in with `gh`")]
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
    /// Convert a `ureq::Error` into our typed error. On a non-2xx status the
    /// response body is scrubbed to its safe `message` field before being
    /// stored, so a token-bearing or otherwise sensitive body never reaches an
    /// error `Display` or the JSON error envelope (R18).
    pub(crate) fn from_ureq(err: ureq::Error) -> Self {
        match err {
            ureq::Error::Status(status, response) => {
                let body = scrub_body(&response.into_string().unwrap_or_default());
                GitHubError::Status { status, body }
            }
            ureq::Error::Transport(transport) => GitHubError::Transport(transport.to_string()),
        }
    }
}

/// Reduce a GitHub error response body to its `message` field, dropping
/// everything else. GitHub's `message` is a fixed human string (`Bad
/// credentials`, `Not Found`, ...) and is safe to surface; the rest of the body
/// is discarded so nothing sensitive can ride along. A body that is not JSON, or
/// carries no string `message`, is replaced with a generic placeholder rather
/// than echoed raw.
fn scrub_body(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "<response body omitted>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_body_keeps_message_and_drops_everything_else() {
        // A body that echoes a secret alongside the message must surface only
        // the message; the secret must not survive the scrub.
        let raw = r#"{"message":"Bad credentials","token":"ghp_SECRET","documentation_url":"https://docs"}"#;
        let scrubbed = scrub_body(raw);
        assert_eq!(scrubbed, "Bad credentials");
        assert!(!scrubbed.contains("ghp_SECRET"), "scrubbed body leaked a token: {scrubbed}");
        assert!(!scrubbed.contains("documentation_url"), "scrubbed body kept extra fields: {scrubbed}");
    }

    #[test]
    fn scrub_body_omits_a_non_json_body() {
        // A non-JSON body (e.g. an HTML proxy error page) is dropped wholesale,
        // not echoed, so it cannot leak whatever it happens to contain.
        let scrubbed = scrub_body("<html>oops ghp_SECRET</html>");
        assert_eq!(scrubbed, "<response body omitted>");
        assert!(!scrubbed.contains("ghp_SECRET"));
    }

    #[test]
    fn scrub_body_omits_json_without_a_message() {
        assert_eq!(scrub_body(r#"{"errors":[{"code":"x"}]}"#), "<response body omitted>");
    }
}
