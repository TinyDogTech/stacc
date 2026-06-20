---
title: "feat(sync): [STA-88] offer to track untracked local branches"
date: 2026-06-20
origin: docs/brainstorms/2026-06-20-sta88-sync-track-untracked-requirements.md
type: feat
---

# feat(sync): [STA-88] offer to track untracked local branches

## Summary

`stacc sync` currently ignores local branches it does not track. This plan adds a post-reconcile pass
that detects untracked branches and, on an interactive TTY, presents an `inquire`-based multi-select
prompt so the user can track them in the same run. Declined branches are persisted in `RepoConfig`
and never re-prompted. Non-interactive / JSON runs report untracked branches via a new `untracked`
JSON field without prompting.

---

## Problem Frame

Branches created before stacc was initialized or via plain `git checkout -b` accumulate outside
stack management silently. The only discovery path today is `stacc log --show-untracked`; bringing
them in requires a separate `stacc track` invocation per branch. Drift is easy to miss.

Origin: `docs/brainstorms/2026-06-20-sta88-sync-track-untracked-requirements.md`

---

## Requirements Trace

- **R1:** Interactive `stacc sync` surfaces untracked branches and lets the user track them in one flow.
- **R2:** Non-interactive / JSON sync adds `untracked: [...]` to output without prompting.
- **R3:** Declining a branch persists across sync invocations.
- **R4:** `stacc track <branch>` removes the branch from the declined list.
- **R5:** Sync with no surfaced branches (all tracked or all declined) behaves identically to today.

Non-goals carried from origin: ancestry-walk on standalone `stacc track`; a dedicated
declined-list reset command; surfacing untracked in `stacc merge` or other commands.

---

## Key Technical Decisions

**KTD-1: Use `inquire` for the multi-select prompt.**
git-spice (the reference implementation) uses Charmbracelet Bubbles `MultiSelect` for its
interactive branch selection. `inquire::MultiSelect` is the closest Rust analog. The existing
`interactive.rs` helpers (`prompt_select`) are pure-stdio numbered menus; adding `inquire` here
introduces the project's first TUI prompt library. This is intentional: it matches the quality
bar of the reference implementation and is the right place to adopt `inquire` since this is the
first multi-select prompt in the codebase.

**KTD-2: `declined_tracking` lives on `RepoConfig`, not `BranchState`.**
Declined branches are not tracked, so no `BranchState` exists for them. The declined set is a
repo-level concept, making `RepoConfig` the right home. The field must carry
`#[serde(default, skip_serializing_if = "BTreeSet::is_empty")]` so existing serialized `repo`
blobs (which lack the key) deserialize without error.

**KTD-3: Extract `track_branch_impl` helper to share tracking logic.**
The `track` command operates on `git.current_branch()`, but the sync flow must track arbitrary
branch names without a `git checkout`. Extracting a `track_branch_impl(git, store, branch, base)`
helper avoids duplicating the `BranchState` insert logic and lets both callers share the
declined-list clearing in a single `store.update` closure.

**KTD-4: `no_interactive` must be threaded to `sync`.**
Currently `sync` does not receive `no_interactive` (unlike `checkout` which receives it via
`cli.global.no_interactive`). The dispatch in `crates/stacc/src/lib.rs` must be updated to match
the checkout pattern.

**KTD-5: `untracked` in JSON output = branches surfaced before interactive decisions.**
The `untracked` array in `report_sync` reflects the set computed before any prompt (branches not
tracked and not declined). In JSON / non-interactive mode this is the definitive output. In
interactive / pretty mode, the user's prompt decisions (track / decline) happen before
`report_sync`, so the `untracked` array passed in interactive runs will include only branches that
remained surfaced at prompt time (declined ones were added to the declined set, tracked ones moved
to state.branches). For the pretty arm of `report_sync`, print a short hint when the array is
non-empty.

**KTD-6: Ancestry walk for base inference uses `git.is_ancestor`.**
For each untracked branch, collect all tracked branches for which `git.is_ancestor(tracked, untracked)` returns true. Among those candidates, the "nearest" is the one that is NOT itself an ancestor of any other candidate (i.e., the deepest in the chain). Fall back to trunk when no candidates exist. This is O(n*m) where n = untracked and m = tracked branches -- acceptable for realistic branch counts.

**KTD-7: Do not bump `schema_version`.**
The `untracked` field is additive. Existing callers that parse sync JSON are unaffected by a new
optional field. Prior art: `likely_merged` was added without a version bump.

---

## High-Level Technical Design

### Flow: sync post-reconcile untracked pass

```
sync() called
  └─ [existing: reconcile_detection, reconcile_with, cleanup_merged_refs]
  │
  ├─ compute `surfaced`:
  │    local_branches() - trunk - state.branches.keys() - repo.declined_tracking
  │
  ├─ [JSON / non-interactive mode]
  │    report_sync(..., untracked: &surfaced)
  │    └─ JSON: "untracked": [...]
  │    └─ Pretty: hint if non-empty
  │
  └─ [interactive mode: interactive::allowed() = true]
       inquire::MultiSelect(surfaced with inferred bases)
       └─ [for each selected branch]
       │    inquire::Text(confirm or override base)
       │    track_branch_impl(git, store, branch, base)
       └─ [unselected = declined]
            store.update -> repo.declined_tracking.extend(declined)
       report_sync(..., untracked: &[])   ← all were acted on
```

### Base inference

```
fn infer_base(git, state_branches, trunk, untracked) -> String:
  candidates = state_branches.keys()
               .filter(|t| git.is_ancestor(t, untracked))
               .collect()

  nearest = candidates.iter().find(|t|
    !candidates.iter().any(|other| other != t && git.is_ancestor(t, other))
  )

  nearest.unwrap_or(trunk)
```

---

## Scope Boundaries

### Deferred to follow-up work

- Ancestry-walk base inference on standalone `stacc track` command (separate ticket).
- A `stacc declined` or `stacc decline --reset` command to inspect / clear the declined list.
- Surfacing untracked branches in `stacc merge` or `stacc log` hint.
- Keyboard-navigable `inquire` integration for other existing single-select prompts in
  `interactive.rs` (tracked separately as a UX consistency pass).

### Out of scope

- Any change to the non-interactive JSON contract beyond adding the `untracked` array.
- Changes to `stacc track`'s base-defaulting behavior (still defaults to trunk).

---

## Implementation Units

### U1. Add `declined_tracking` to `RepoConfig`

**Goal:** Persist per-repo declined branch names in stacc state with zero-cost backward compat.

**Requirements:** R3

**Dependencies:** none

**Files:**
- `crates/stacc-state/src/model.rs` -- add field + serde attributes
- `crates/stacc-state/src/model.rs` -- new roundtrip test (backward compat)

**Approach:**
Add to `RepoConfig`:

```rust
#[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
pub declined_tracking: BTreeSet<String>,
```

Add `use std::collections::BTreeSet;` at the top of the file. The `#[serde(default)]` ensures
existing stored blobs (which lack this key) deserialize to an empty set.

**Patterns to follow:**
`BranchState.pr_title` and `.pr_description` use the same `serde(default, skip_serializing_if = "Option::is_none")` pattern. Adapt for `BTreeSet::is_empty`.

**Test scenarios:**
- Deserialize `{"trunk":"main","remote":"origin"}` (no `declined_tracking` key) -> field is
  empty set. (Backward compat: repos initialized before STA-88 must not error on `store.load()`.)
- Roundtrip `RepoConfig` with a non-empty `declined_tracking` -> serializes and deserializes
  without loss.
- `skip_serializing_if`: serialize a `RepoConfig` with empty `declined_tracking` -> JSON blob
  does not contain the key (matches `print_compact` behavior on other omitted fields).

**Verification:** `cargo test -p stacc-state` passes with all three scenarios above.

---

### U2. Add `inquire` dependency and interactive prompt helpers

**Goal:** Provide `prompt_multi_select` and `prompt_confirm_or_change` in `interactive.rs`
using `inquire`, so the sync flow can present branch selection and base confirmation prompts.

**Requirements:** R1

**Dependencies:** none

**Files:**
- `Cargo.toml` (workspace `[workspace.dependencies]`) -- add `inquire`
- `crates/stacc/Cargo.toml` -- reference workspace dep
- `crates/stacc/src/interactive.rs` -- add two new pub functions

**Approach:**

Add `inquire` to the workspace `[workspace.dependencies]` table and reference it from the
`crates/stacc` crate. The `inquire` version should be the latest stable at implementation time.

Add to `crates/stacc/src/interactive.rs`:

```
pub fn prompt_multi_select(
    prompt: &str,
    items: &[String],      // display strings (e.g. "jillian/foo  (base: main)")
) -> Result<Vec<usize>, Error>
```

Returns the indices of selected items. Uses `inquire::MultiSelect`. On a non-interactive or
dumb terminal this should not be called (the gate in sync is `interactive::allowed`).

```
pub fn prompt_confirm_or_change(
    prompt: &str,          // e.g. "Base for 'jillian/foo'"
    default: &str,         // pre-filled value (inferred base)
) -> Result<String, Error>
```

Returns the confirmed or overridden value. Uses `inquire::Text` with `default` pre-filled.

Map `inquire::InquireError` to `Error::Usage`.

**Patterns to follow:**
Existing `prompt_select` pattern: write to stderr, read from stdin, return `Error::Usage` on
bad input. The `inquire` functions handle their own I/O, so the wrapper is thin.

**Test scenarios:**
- `allowed()` gate behavior is unchanged (existing tests pass).
- Unit testing `prompt_multi_select` / `prompt_confirm_or_change` requires a live TTY. Add
  `#[cfg(test)]` integration notes but do not try to mock `inquire` internals -- rely on the
  sync integration test (U5) to exercise the gate path.
- When `inquire` returns `InquireError::OperationCanceled` (user pressed Esc), verify the
  wrapper treats it as "select none" / empty selection rather than an error.

**Verification:** `cargo build -p stacc` succeeds; `cargo test -p stacc` passes; the two new
functions are visible from `crates/stacc/src/commands/operations.rs`.

---

### U3. Base inference helper

**Goal:** Given an untracked branch name and the current tracked-branch state, return the
nearest tracked ancestor or trunk.

**Requirements:** R1

**Dependencies:** none (uses existing `git.is_ancestor` and `state.branches`)

**Files:**
- `crates/stacc/src/commands/operations.rs` -- new private `fn infer_base(...) -> String`
- `crates/stacc/src/commands/operations.rs` -- unit tests in `#[cfg(test)]` block

**Approach:**
The function signature (directional):

```
fn infer_base(
    git: &Git,
    tracked: &BTreeMap<String, BranchState>,
    trunk: &str,
    untracked: &str,
) -> String
```

Algorithm (see KTD-6): collect all tracked branches for which `git.is_ancestor(t, untracked)` is
true; from those candidates, find the one that is not an ancestor of any other candidate; return
it or fall back to trunk. Skip `is_ancestor` errors gracefully (treat as non-ancestor).

**Patterns to follow:**
Other helpers in `operations.rs` that call `git.is_ancestor` (e.g., `branch_is_squash_merged`).

**Test scenarios:**
- Linear stack `main -> A (tracked) -> B (tracked) -> C (untracked)`: `infer_base` for C
  returns B.
- No tracked branches other than trunk: returns trunk.
- Untracked branch with no relationship to any tracked branch (diverged from trunk before any
  tracking): returns trunk.
- Two tracked ancestors at different depths: returns the deeper one (nearest to tip).
- `git.is_ancestor` returns an error for one candidate: that candidate is skipped, result is
  still the correct nearest among remaining candidates.

**Verification:** `cargo test -p stacc -- infer_base` passes all scenarios above.

---

### U4. `stacc track` refactor -- extract helper and clear declined list

**Goal:** Extract reusable `track_branch_impl` helper from the `track` command, and update it
to atomically clear the tracked branch from `declined_tracking` in the same `store.update` closure.

**Requirements:** R4

**Dependencies:** U1 (needs `declined_tracking` field on `RepoConfig`)

**Files:**
- `crates/stacc/src/commands.rs` -- extract helper, update `track` function
- `crates/stacc/tests/track.rs` -- new test: tracking a declined branch clears it

**Approach:**

Extract:

```
fn track_branch_impl(
    git: &Git,
    store: &StateStore,
    branch: &str,
    base: &str,
) -> Result<(), Error>
```

This function resolves `git.rev_parse(base)`, then calls `store.update` with a closure that:
1. Inserts `BranchState` for `branch` into `state.branches`.
2. Removes `branch` from `state.repo.declined_tracking` (if repo is `Some`).

The existing `track` command becomes: validate current branch != trunk, resolve `base` from
args or trunk default, call `track_branch_impl(git, store, &branch, &base)`, print output.

The sync flow (U5) calls `track_branch_impl` directly with an arbitrary branch name, bypassing
the current-branch check.

**Patterns to follow:**
Existing `store.update(|state| { state.branches.insert(...); Ok(()) })` in `commands.rs:118`.

**Test scenarios:**
- `stacc track` on a branch in `declined_tracking` -> branch moves to `state.branches` and
  `declined_tracking` no longer contains it (both mutations in one `store.update` call).
- `stacc track` on a branch NOT in `declined_tracking` -> normal track, no error on missing key.
- Existing `track_records_branch_with_trunk_base` and `track_accepts_explicit_base` tests
  continue to pass (regression).

**Verification:** `cargo test -p stacc -- track` passes all scenarios above.

---

### U5. Untracked detection and interactive prompt in `sync`

**Goal:** Wire the full STA-88 feature: detect untracked branches post-reconcile, run the
interactive prompt flow on TTY, add `untracked` to JSON output, persist declined branches.

**Requirements:** R1, R2, R3, R5

**Dependencies:** U1, U2, U3, U4

**Files:**
- `crates/stacc/src/lib.rs` -- thread `no_interactive` to `sync` dispatch
- `crates/stacc/src/commands/operations.rs` -- `sync` signature, hook-in, `report_sync` updated
- `crates/stacc/tests/sync.rs` -- new tests

**Approach:**

**1. Thread `no_interactive` to sync.**
Update `sync` signature to accept `no_interactive: bool`. In `lib.rs`, update dispatch from:

```
Command::Sync(args) => commands::sync(args, cli.global.output_format(), work_dir)
```

to pass `cli.global.no_interactive` as well, matching the `checkout` pattern at `lib.rs:130`.

**2. Hook-in location.**
Insert the untracked pass between `cleanup_merged_refs` (line 1112) and `report_sync` (line 1113):

```
// [new] post-reconcile untracked pass
let surfaced = {
    local = git.local_branches().unwrap_or_default()
    declined = repo.declined_tracking  // BTreeSet<String>
    local - trunk - state.branches.keys() - declined
};

if interactive::allowed(stdin().is_terminal(), no_interactive, format) {
    // interactive path
    // build display items: "branch-name  (base: inferred-base)"
    // prompt_multi_select -> selected indices
    // for each selected: prompt_confirm_or_change(base) -> track_branch_impl
    // declined = surfaced items NOT selected -> store.update(extend declined_tracking)
} // else: surfaced passes through to report_sync
```

**3. Update `report_sync`.**
Add `untracked: &[String]` parameter. In the `Json` arm, add:

```rust
"untracked": untracked,
```

In the `Pretty` arm, if `!untracked.is_empty()`:

```
hint: N untracked branch(es) not yet tracked. Run `stacc sync` interactively to track them.
```

Also update the `continue_op` call to `report_sync` at line 3073 to pass `&[]` (a resumed
sync never re-detects untracked branches).

**4. Interactive prompt detail.**
The multi-select display items are strings of the form `"jillian/foo  (base: main)"` where the
base is computed by `infer_base(git, &state.branches, &repo.trunk, branch)`. After selection:
- For each selected branch: call `prompt_confirm_or_change("Base for '{branch}'", &inferred_base)`,
  then `track_branch_impl(git, &store, branch, &confirmed_base)`.
- Collect indices not selected; compute branch names; call `store.update` to extend
  `declined_tracking` with those names.

If the multi-select returns an empty selection (user confirmed with none checked), all surfaced
branches are declined.

**Patterns to follow:**
`checkout` command for `no_interactive` threading (`lib.rs:130`, `commands/navigation.rs`).
`print_untracked` in `commands/log.rs:1017` for the untracked detection filtering logic.

**Test scenarios:**
- `--json --offline` on a repo with one untracked local branch: JSON output contains
  `"untracked":["branch-name"]`. (Use `--offline` to skip GitHub API mock.)
- `--json --offline` on a repo where all local branches are tracked: `untracked` key absent
  or `"untracked":[]`.
- `--json --offline` on a repo where the untracked branch is in `declined_tracking`: `untracked`
  key absent (declined branches are excluded from the surfaced set before report).
- After running interactive sync and declining a branch: subsequent `--json` sync run shows
  `untracked` absent for that branch (persisted decline).
- Sync with `--continue` (resumed sync): `untracked` key absent (the `continue_op` path passes
  `&[]`).
- Regression: existing sync tests (merged, pruned, reparented, restacked) still pass.

**Verification:** `cargo test -p stacc -- sync` passes; `cargo clippy -p stacc` clean.

---

## Open Questions

- **`inquire` version pinning:** Determine the latest stable `inquire` version at implementation
  time. If `inquire` is behind in its MSRV compared to the workspace, check compatibility.
- **Esc / Ctrl-C behavior on `inquire::MultiSelect`:** Confirm whether `InquireError::OperationCanceled`
  should be treated as "decline all" or as an error that surfaces to the user. The interactive.rs
  wrapper should encode this choice explicitly.

---

## Risks and Dependencies

- **`inquire` MSRV:** Verify `inquire`'s minimum supported Rust version is compatible with the
  workspace edition (2021). If not, a version pin may be required.
- **Stdin conflicts in tests:** Integration tests that invoke the `stacc` binary cannot easily
  inject stdin to exercise the interactive prompt path. The interactive TTY tests are therefore
  limited to unit tests of the helpers (U2, U3) and JSON-mode integration tests (U5). This is
  consistent with how `checkout`'s interactive mode is currently tested.
- **`report_sync` signature change:** Adding `untracked: &[String]` is a breaking change to an
  internal function. Both call sites (line 1113 and line 3073) must be updated; the compiler
  will catch any missed sites.

---

## Sources and Research

- `crates/stacc/src/interactive.rs` -- existing prompt gate and single-select pattern
- `crates/stacc-git/src/lib.rs:239` -- `git.is_ancestor` used for base inference
- `crates/stacc-git/src/lib.rs:1021` -- `git.local_branches()`
- `crates/stacc/src/commands/operations.rs:1038` -- sync entry point, hook-in site
- `crates/stacc/src/commands/operations.rs:3427` -- `report_sync` to update
- `crates/stacc/src/commands.rs:97` -- `track` command to refactor
- `crates/stacc-state/src/model.rs:7` -- `RepoConfig` to extend
- `crates/stacc/src/lib.rs:109,130` -- dispatch pattern for `no_interactive` threading
- `../git-spice/internal/ui/multi_select.go` -- reference: git-spice uses `charm.land/bubbles/v2 MultiSelect`; `inquire::MultiSelect` is the Rust analog
