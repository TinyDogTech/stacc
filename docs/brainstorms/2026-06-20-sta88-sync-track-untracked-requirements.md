# STA-88: sync -- offer to track untracked local branches

**Status:** Ready for planning
**Linear:** STA-88
**Date:** 2026-06-20

---

## Problem

`stacc sync` silently ignores local branches it does not track. Branches created before stacc was initialized, or via plain `git checkout -b`, accumulate outside stack management. The only way to discover them is `stacc log --show-untracked`, and the only way to bring them in is to remember to run `stacc track` individually. Drift is silent and easy to miss.

## Users

Developers running `stacc sync` interactively after creating branches with plain git or after adopting a repo mid-flight.

## Goals

- Surface untracked local branches during interactive sync and offer to track them in a single flow.
- Non-interactive (`--no-interactive` / `--json`) runs report untracked branches in output without prompting.
- Declining a branch is persistent: the user is never re-prompted for the same branch unless they revoke the decline.

## Non-Goals

- Adding ancestry-walk base inference to the `stacc track` command itself (follow-on, separate ticket).
- A dedicated "clear declined" or "reset declined list" command. Tracking a branch via `stacc track` implicitly removes it from the declined list.
- Surfacing untracked branches in `stacc merge` or other commands.

---

## Core Behavior

### Interactive flow (TTY, not `--no-interactive`, not `--json`)

After the existing sync reconcile and restack pass completes, stacc:

1. Computes the untracked set: all local branches except trunk and branches already in state, minus any in the declined list.
2. If the set is empty, skips silently.
3. If non-empty, presents a multi-select prompt listing each untracked branch with its inferred base:
   ```
   Untracked branches found. Select branches to track (space to toggle, enter to confirm):
   [x] jillian/sta-5-foo  (base: jillian/sta-4-bar)
   [x] jillian/wip-thing  (base: main)
   [ ] jillian/old-exp    (base: main)
   ```
   Unselected branches (those the user leaves unchecked when confirming) are added to the declined list.
4. For each selected branch, in sequence, prompt to confirm or change the inferred base:
   ```
   Track 'jillian/sta-5-foo' with base 'jillian/sta-4-bar'? [Y/change]
   ```
   - Pressing enter (or Y) accepts the inferred base.
   - Entering a branch name overrides the base.
5. Tracking is performed in the same way `stacc track` operates: records the branch and its base in state.

Gate: use `interactive::allowed(is_terminal, args.no_interactive, format)` (see `crates/stacc/src/interactive.rs`). No changes to the gate logic itself.

### Base inference

For each untracked branch, walk its git parents and return the first tracked branch (one whose name is a key in `state.branches`) found in the ancestry chain. If no tracked ancestor exists, use trunk.

This logic is implemented specifically for this sync flow. It is not added to the `track` command itself in this scope.

### Declined list: persistence rules

- Declined branches are stored in a new field on `RepoConfig` in `crates/stacc-state/src/model.rs`: a `BTreeSet<String>` (e.g., `declined_tracking: BTreeSet<String>`).
- Added via `StateStore::update()` at the end of the sync prompt flow.
- A branch is removed from the declined list automatically when it becomes tracked (via `stacc track` or via a future sync where the user selects it). Removal must be implemented in `stacc track` as well as the sync flow.
- The declined list applies only within the same repo (scoped to `RepoConfig`, not global config).

### Non-interactive flow (`--no-interactive` or `--json`)

No prompt is shown. After the existing reconcile pass, the untracked set (excluding trunk, already-tracked, and declined branches) is computed and added to sync JSON output:

```json
{
  "op": "sync",
  "merged": [...],
  "untracked": ["jillian/sta-5-foo", "jillian/old-exp"],
  ...
  "schema_version": 2
}
```

If the untracked set is empty, `untracked` is an empty array (or absent, per the existing `print_compact` null-stripping behavior).

Non-interactive runs do not modify the declined list.

Pretty (non-JSON, non-interactive) mode: print a short hint after the existing sync output if untracked branches exist, e.g.:
```
  hint: 2 untracked branch(es) found. Run `stacc sync` interactively to track them.
```

---

## State Model Changes

- `crates/stacc-state/src/model.rs`: add `declined_tracking: BTreeSet<String>` to `RepoConfig`.
- `crates/stacc/src/commands.rs` (`track` command): on successful track, remove the branch from `declined_tracking`.
- `crates/stacc/src/commands/operations.rs` (`sync`): hook in after `reconcile_detection` returns (around line 1085), compute untracked, gate on `interactive::allowed`, run the multi-select flow.

---

## JSON Output Shape (updated)

```
{
  "op": "sync",
  "merged": [...],
  "pruned": [...],
  "adopted": [...],
  "reparented": [...],
  "restacked": [...],
  "cleaned": [...],
  "cleanup_skipped": [...],
  "detection_skipped": false,
  "likely_merged": [...],
  "untracked": ["branch-name", ...],
  "schema_version": 2
}
```

`untracked` reflects branches surfaced (not already declined), not branches that were tracked during this run (those would appear in a future `adopted`-style field if ever needed).

---

## Success Criteria

- Interactive `stacc sync` surfaces untracked branches (excluding declined) and allows the user to track them in one flow.
- Declining a branch persists across sync invocations.
- Running `stacc track <branch>` removes the branch from the declined list.
- `--no-interactive` / `--json` sync adds `untracked: [...]` to output and never prompts.
- `stacc sync` with no untracked branches (or all declined) behaves identically to today.

---

## Outstanding Assumptions

- The `print_compact` null-stripping convention means an empty `untracked` array may be omitted from JSON. Callers that parse sync JSON should treat a missing `untracked` key as equivalent to `untracked: []`.
- Ancestry walk performance is acceptable for typical branch counts (tens, not thousands). No caching planned.
