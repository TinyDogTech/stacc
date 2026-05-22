//! Stack state stored as a JSON tree in the hidden `refs/stacc/` git ref.

mod model;

pub use model::{Base, BranchState, PullRequest, RepoConfig};
