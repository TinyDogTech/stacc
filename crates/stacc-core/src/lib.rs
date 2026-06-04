//! Stack operations, the engine that ties `git` and the `refs/stacc/` state
//! together for `submit`, `sync`, and `restack` (and later `modify`/`move`/
//! `merge`). The CLI crate is a thin caller over this.
//!
//! See `plans/algorithms.md` (Squash-merge detection, Restack, Conflict resume).

pub mod ops;
pub mod recovery;
