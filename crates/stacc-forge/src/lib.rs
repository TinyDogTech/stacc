//! The forge boundary.
//!
//! This crate defines the forge-neutral change vocabulary and the [`Forge`]
//! trait that every forge (GitHub, GitLab, ...) implements. It carries no
//! network code, no auth, and no forge-specific types: those live in the
//! per-forge crates that depend on this one. Keeping the boundary in a leaf-ish
//! crate gives the CLI one neutral-vocabulary home and adds only additive
//! dependency edges (KTD1).
//!
//! A forge instance is scoped to a single project: the project identity
//! (owner/repo for GitHub, an encoded full path for GitLab) is established when
//! the forge is constructed, so trait methods speak only in change numbers and
//! branch names, never repository coordinates. This keeps the trait object-safe
//! (`Box<dyn Forge>`) and free of any per-forge identifier type.

pub mod capability;
pub mod error;
pub mod model;

pub use capability::Capabilities;
pub use error::{ForgeError, ForgeErrorEnvelope, ForgeErrorType};
pub use model::{
    Change, ChangeState, ChangeStatus, ChangeUpdate, ChecksState, MergeOptions, MergeOutcome,
    MergeReadiness, MergeRejectionReason, ReviewState, SubmitChange,
};

use std::collections::BTreeMap;
use std::time::Duration;

/// The version of the neutral CLI JSON schema.
///
/// A present `schema_version` means the versioned v2 schema; a consumer treats
/// the field's *absence* as legacy/untrusted output, since v1 never emitted it.
/// This is the first versioned schema, so it is `2`, not `1`.
pub const SCHEMA_VERSION: u32 = 2;

/// The operations every forge implements, in forge-neutral vocabulary.
///
/// The method set mirrors today's GitHub surface, neutralized. Every method
/// returns [`ForgeError`] on failure; an operation a forge cannot perform (for
/// example branch rename on GitLab) returns [`ForgeError::Unsupported`] rather
/// than panicking or silently succeeding.
///
/// The trait is object-safe so the CLI can hold a `Box<dyn Forge>` chosen at
/// runtime from the remote host.
pub trait Forge {
    /// The login of the authenticated user. Proves the token works.
    fn current_user(&self) -> Result<String, ForgeError>;

    /// Open a new change for the stack.
    fn create_change(&self, change: &SubmitChange) -> Result<Change, ForgeError>;

    /// Update an existing change. Unset fields in `update` are left as-is.
    fn update_change(&self, number: u64, update: &ChangeUpdate) -> Result<Change, ForgeError>;

    /// Fetch a change by number, including whether it was merged.
    fn get_change(&self, number: u64) -> Result<Change, ForgeError>;

    /// Like [`get_change`](Self::get_change) but caps this single call at
    /// `timeout`, so a caller polling several changes under a wall-clock budget
    /// can bound in-flight time.
    fn get_change_within(&self, number: u64, timeout: Duration) -> Result<Change, ForgeError>;

    /// The open change whose head is `branch`, if one exists.
    fn change_for_branch(&self, branch: &str) -> Result<Option<Change>, ForgeError>;

    /// Like [`change_for_branch`](Self::change_for_branch) but caps this single
    /// call at `timeout`.
    fn change_for_branch_within(
        &self,
        branch: &str,
        timeout: Duration,
    ) -> Result<Option<Change>, ForgeError>;

    /// The newest change whose head is `branch` in *any* state (open, merged, or
    /// closed). `sync` uses this to reconcile changes that already merged.
    fn change_for_branch_any_state(&self, branch: &str) -> Result<Option<Change>, ForgeError>;

    /// Merge a change, honoring `opts` (squash, head-SHA assertion). A merge the
    /// forge blocks returns [`ForgeError::Rejected`] carrying a structured
    /// [`MergeRejectionReason`], never an opaque error.
    fn merge_change(&self, number: u64, opts: &MergeOptions) -> Result<MergeOutcome, ForgeError>;

    /// Review and checks status for a set of changes, in one batched call capped
    /// at `timeout`. A change the forge omits (deleted, or an unknown number) is
    /// simply absent from the map.
    fn change_checks(
        &self,
        numbers: &[u64],
        timeout: Duration,
    ) -> Result<BTreeMap<u64, ChangeStatus>, ForgeError>;

    /// Whether `branch` has branch protection enabled on the forge.
    fn branch_protected(&self, branch: &str) -> Result<bool, ForgeError>;

    /// Close a change without merging it.
    fn close_change(&self, number: u64) -> Result<Change, ForgeError>;

    /// Rename a branch on the remote. A forge that cannot do this returns
    /// [`ForgeError::Unsupported`] (GitLab, in slice 2).
    fn rename_branch(&self, branch: &str, new_name: &str) -> Result<(), ForgeError>;

    /// What this forge can express, so stacc never misreads a forge's silence.
    fn capabilities(&self) -> Capabilities;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time guard that `Forge` stays object-safe: the CLI selects a
    /// forge at runtime and holds it as `Box<dyn Forge>`, so any change that
    /// breaks object safety must fail here, not downstream.
    #[allow(dead_code)]
    fn assert_object_safe(_forge: &dyn Forge) {}

    #[test]
    fn schema_version_is_the_first_versioned_schema() {
        assert_eq!(SCHEMA_VERSION, 2);
    }
}
