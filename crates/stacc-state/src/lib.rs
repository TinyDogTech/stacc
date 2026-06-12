//! Stack state stored as a JSON tree in the hidden `refs/stacc/` git ref.

mod model;
mod store;

pub use model::{Base, BranchState, Disposal, PullRequest, RepoConfig};
pub use store::{dropped_ref, State, StateError, StateStore};
