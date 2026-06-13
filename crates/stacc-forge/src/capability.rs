//! What a forge can express, so stacc never misreads a forge's silence.

use serde::{Deserialize, Serialize};

/// The capabilities of a forge.
///
/// Minimal for slice 2: one field. It grows to a richer struct only when a
/// second capability has a real consumer (KTD2), rather than being padded with
/// speculative flags now.
///
/// The [`Default`] is conservative on purpose: every capability is `false`
/// unless a forge explicitly claims it, so a missing claim is never read as a
/// guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// Whether the forge can express a "changes requested" review state. GitHub
    /// can; GitLab cannot, so on GitLab the absence of an approval must not be
    /// read as "changes requested".
    pub expresses_changes_requested: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_conservative_not_accidentally_true() {
        // A forge that says nothing must not appear more capable than it is.
        assert!(!Capabilities::default().expresses_changes_requested);
    }

    #[test]
    fn round_trips_through_json() {
        let caps = Capabilities {
            expresses_changes_requested: true,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert_eq!(serde_json::from_str::<Capabilities>(&json).unwrap(), caps);
    }
}
