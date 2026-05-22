//! GitHub API client and authentication (PAT or OAuth device flow).
//!
//! Used to create/update PRs, query PR merge state (the basis for squash-merge
//! detection), and fetch upstream PR context on conflict. See `plans/stacc.md`
//! (Forge support) and `plans/algorithms.md` (Squash-merge detection).
