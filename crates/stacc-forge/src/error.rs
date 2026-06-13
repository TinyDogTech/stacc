//! The neutral forge error: a `thiserror` enum returned by every [`crate::Forge`]
//! method, plus the structured, serializable envelope an agent consumes.
//!
//! The envelope deliberately carries no `forge` discriminator (KTD4): an agent
//! must never branch on *which* forge failed. It also never carries a raw forge
//! response body; each forge implementation scrubs the body and extracts a safe
//! structured reason before constructing a `ForgeError`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::MergeRejectionReason;

/// The neutral, structured error type code for the agent-facing envelope.
///
/// This is the branching key an agent reads instead of a forge name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeErrorType {
    /// Authentication failed, or no token was available.
    ForgeAuth,
    /// The forge refused an otherwise well-formed request (e.g. a blocked merge).
    ForgeRejected,
    /// A conflicting state on the forge.
    Conflict,
    /// The requested resource does not exist.
    NotFound,
    /// The forge rate-limited the request.
    RateLimited,
    /// The operation is not supported by this forge.
    Unsupported,
    /// A transport/network error reaching the forge.
    Transport,
    /// The forge returned a response stacc could not interpret.
    Unexpected,
}

/// An error from a forge operation, in forge-neutral terms.
#[derive(Debug, Error)]
pub enum ForgeError {
    #[error("no forge token found; run `stacc auth login` or set the forge token env var")]
    MissingToken,

    #[error("forge authentication failed")]
    AuthFailed,

    /// The forge refused to merge. Every block carries a structured reason
    /// ([`MergeRejectionReason::Unknown`] when the forge gives no mappable
    /// cause), so an agent is never left with an opaque rejection (R16). There
    /// is deliberately no unstructured "not mergeable" variant.
    #[error("the forge refused to merge the change: {0:?}")]
    Rejected(MergeRejectionReason),

    #[error("conflicting state on the forge")]
    Conflict,

    #[error("resource not found")]
    NotFound,

    #[error("forge rate limit exceeded")]
    RateLimited,

    #[error("operation `{0}` is not supported by this forge")]
    Unsupported(String),

    #[error("transport error reaching the forge: {0}")]
    Transport(String),

    #[error("unexpected forge response: {0}")]
    Unexpected(String),
}

impl ForgeError {
    /// The neutral type code for this error, for the agent-facing envelope.
    pub fn error_type(&self) -> ForgeErrorType {
        match self {
            ForgeError::MissingToken | ForgeError::AuthFailed => ForgeErrorType::ForgeAuth,
            ForgeError::Rejected(_) => ForgeErrorType::ForgeRejected,
            ForgeError::Conflict => ForgeErrorType::Conflict,
            ForgeError::NotFound => ForgeErrorType::NotFound,
            ForgeError::RateLimited => ForgeErrorType::RateLimited,
            ForgeError::Unsupported(_) => ForgeErrorType::Unsupported,
            ForgeError::Transport(_) => ForgeErrorType::Transport,
            ForgeError::Unexpected(_) => ForgeErrorType::Unexpected,
        }
    }

    /// The structured reason for a blocked merge, if this error is one. Keeps an
    /// agent un-blinded on a rejection (R16) instead of leaving it an opaque error.
    pub fn reason(&self) -> Option<MergeRejectionReason> {
        match self {
            ForgeError::Rejected(reason) => Some(*reason),
            _ => None,
        }
    }

    /// Build the neutral, serializable envelope an agent consumes.
    pub fn to_envelope(&self) -> ForgeErrorEnvelope {
        ForgeErrorEnvelope {
            error_type: self.error_type(),
            reason: self.reason(),
            message: self.to_string(),
            schema_version: crate::SCHEMA_VERSION,
        }
    }
}

/// The neutral, serializable error envelope an agent consumes.
///
/// Carries a structured `type` code, an optional structured `reason`, a human
/// `message`, and `schema_version`. Never a `forge` discriminator and never a
/// raw forge response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeErrorEnvelope {
    #[serde(rename = "type")]
    pub error_type: ForgeErrorType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<MergeRejectionReason>,
    pub message: String,
    pub schema_version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_type_serializes_to_neutral_codes() {
        let code = |t: &ForgeErrorType| serde_json::to_value(t).unwrap().as_str().unwrap().to_string();
        assert_eq!(code(&ForgeErrorType::ForgeAuth), "forge_auth");
        assert_eq!(code(&ForgeErrorType::ForgeRejected), "forge_rejected");
        assert_eq!(code(&ForgeErrorType::Conflict), "conflict");
        assert_eq!(code(&ForgeErrorType::NotFound), "not_found");
        assert_eq!(code(&ForgeErrorType::RateLimited), "rate_limited");
        assert_eq!(code(&ForgeErrorType::Unsupported), "unsupported");
        assert_eq!(code(&ForgeErrorType::Transport), "transport");
        assert_eq!(code(&ForgeErrorType::Unexpected), "unexpected");
    }

    #[test]
    fn errors_map_to_their_type_codes() {
        assert_eq!(ForgeError::MissingToken.error_type(), ForgeErrorType::ForgeAuth);
        assert_eq!(ForgeError::AuthFailed.error_type(), ForgeErrorType::ForgeAuth);
        assert_eq!(
            ForgeError::Rejected(MergeRejectionReason::Conflict).error_type(),
            ForgeErrorType::ForgeRejected
        );
        assert_eq!(
            ForgeError::Unsupported("rename_branch".into()).error_type(),
            ForgeErrorType::Unsupported
        );
        assert_eq!(ForgeError::Conflict.error_type(), ForgeErrorType::Conflict);
        assert_eq!(ForgeError::NotFound.error_type(), ForgeErrorType::NotFound);
        assert_eq!(ForgeError::RateLimited.error_type(), ForgeErrorType::RateLimited);
        assert_eq!(
            ForgeError::Transport("boom".into()).error_type(),
            ForgeErrorType::Transport
        );
        assert_eq!(
            ForgeError::Unexpected("huh".into()).error_type(),
            ForgeErrorType::Unexpected
        );
    }

    #[test]
    fn rejection_envelope_carries_reason_and_schema_version() {
        let envelope = ForgeError::Rejected(MergeRejectionReason::Conflict).to_envelope();
        assert_eq!(envelope.error_type, ForgeErrorType::ForgeRejected);
        assert_eq!(envelope.reason, Some(MergeRejectionReason::Conflict));
        assert_eq!(envelope.schema_version, crate::SCHEMA_VERSION);

        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("\"type\":\"forge_rejected\""), "{json}");
        assert!(json.contains("\"reason\":\"conflict\""), "{json}");
        assert!(json.contains("schema_version"), "{json}");
        // No `forge` discriminator: an agent branches on `type`, never the forge.
        assert!(!json.contains("\"forge\""), "{json}");

        // The structured reason must survive a full round-trip, not merely
        // appear in the serialized form.
        let back: ForgeErrorEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
        assert_eq!(back.reason, Some(MergeRejectionReason::Conflict));
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let envelope = ForgeError::Unsupported("rename_branch".into()).to_envelope();
        let json = serde_json::to_string(&envelope).unwrap();
        let back: ForgeErrorEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
        // A non-rejection error has no structured reason and omits the field.
        assert_eq!(back.reason, None);
        assert!(!json.contains("reason"), "{json}");
    }
}
