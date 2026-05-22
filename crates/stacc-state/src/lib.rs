//! Stack state stored as a JSON tree in the hidden `refs/stacc/` git ref.

mod model;
mod store;

pub use model::{Base, BranchState, PullRequest, RepoConfig};
pub use store::{State, StateError, StateStore};
