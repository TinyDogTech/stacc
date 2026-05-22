//! Thin, typed wrappers over the `git` command line.
//!
//! stacc shells out to `git` rather than linking a git library so behaviour
//! matches the user's installed git exactly. See `plans/algorithms.md`
//! (Restack, Conflict resume) for the operations this crate must cover.
