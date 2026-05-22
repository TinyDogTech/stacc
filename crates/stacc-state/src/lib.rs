//! Stack state stored as a JSON tree in the hidden `refs/stacc/` git ref.
//!
//! State travels with the repo via push/fetch and needs no server. See
//! `plans/algorithms.md` (State storage) for the data model and the
//! compare-and-swap write loop.
