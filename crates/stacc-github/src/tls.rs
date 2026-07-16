//! TLS trust for stacc's HTTP clients.
//!
//! ureq's default `tls` backend verifies certificates against a bundled copy of
//! the Mozilla root set (`webpki-roots`), which is compiled into the binary and
//! never consults the operating system trust store. Behind a corporate MITM
//! proxy that re-signs TLS with a private root CA installed only in the OS store
//! (the macOS keychain), that root is unknown to the bundled set and every
//! request fails with `invalid peer certificate: UnknownIssuer`.
//!
//! `git` and `gh` work in the same environment because they verify through the
//! platform's own APIs (the macOS Security framework), which read the keychain
//! and tolerate real-world corporate roots that stricter verifiers reject (for
//! example a CA missing the `keyUsage` extension).
//!
//! We match that behaviour by delegating verification to the platform verifier
//! via `rustls-platform-verifier`, so stacc trusts exactly the roots the rest of
//! the user's toolchain already does.

use std::sync::Arc;

use rustls::crypto::ring;
use rustls_platform_verifier::BuilderVerifierExt;

/// A `ureq::AgentBuilder` whose TLS trust is delegated to the OS platform
/// verifier instead of ureq's bundled roots.
///
/// The rustls `ClientConfig` is built with an explicit `ring` provider (the same
/// provider ureq pulls) rather than relying on a process-default provider, which
/// ureq never installs, so this cannot panic on a missing default.
pub(crate) fn agent_builder() -> ureq::AgentBuilder {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the safe default TLS versions")
        .with_platform_verifier()
        .expect("platform certificate verifier is available")
        .with_no_client_auth();

    ureq::AgentBuilder::new().tls_config(Arc::new(config))
}
