---
title: "feat(stacc): [STA-120] cross-branch absorb routing"
type: feat
date: 2026-06-20
---

# feat(stacc): [STA-120] cross-branch absorb routing

## Summary

`stacc absorb` blames staged hunks against the current branch tip to find which
commit introduced each edited line, then rewrites that commit in-memory. Today
it only considers commits in the CURRENT branch's own range (`base..tip`).
When blame returns a commit introduced by a DOWNSTACK branch, absorb marks the
hunk `outside_branch` and leaves it staged. The module docstring promises to
distribute hunks "into the downstack commits that introduced their lines," so
this behavior breaks the stated contract.

Fix: extend absorb to route outside-branch hunks to the downstack branch that
owns the blame commit, rewrite the full commit chain from that branch to the
current tip in-memory, and move all affected branch refs atomically. The
existing upstack restack and conflict-recovery paths are unchanged.

---

## Problem Frame

The current blame-based mapping in `map_hunks()` checks only `own_set` (a
`BTreeSet` of the current branch's commit SHAs). Any commit SHA outside
`own_set` becomes `outside_branch`. On a two-branch stack like:

```
main -> feat-a -> feat-b (current)
```

if a staged hunk edits a line introduced by a commit in `feat-a`, it is
unreachable by absorb even though `feat-a` is directly below the cursor.

The module docstring (`absorb.rs:1-17`) says absorb distributes hunks "into the
downstack commits that introduced their lines," which users read as including
those downstack branch commits. The fix makes the implementation match this
contract.

---

## Requirements

- R1. A staged hunk whose blame commit belongs to a tracked downstack branch is
  routed to that branch's commit chain, not left as `outside_branch`.
- R2. Routing covers the full linear downstack of the current branch: all
  branches from the recorded base up to and including the current branch.
- R3. Hunks for commits outside the entire chain (trunk, untracked branches,
  sibling stacks) remain `outside_branch`.
- R4. Multiple hunks routing to different branches in the same chain are
  absorbed in a single operation; no repeated user invocations needed.
- R5. `--dry-run` output includes which branch owns each mapped commit.
- R6. JSON mapping array gains an optional `"branch"` field per entry. Existing
  keys are unchanged; this is a backwards-compatible addition.
- R7. Worktree guard is extended to cover all affected downstack branches, not
  only the current branch and its upstack children.
- R8. All affected branch refs (from deepest affected downstack branch to
  current) are moved to their rewritten tips via `git.update_ref` CAS.
- R9. `state.branches[*].base.hash` is updated for every branch in the
  rewritten range so future restack and absorb invocations resolve correctly.
- R10. Existing single-branch absorb behavior is preserved as the common case
  (when all mapped commits are in the current branch's `own_set`, nothing extra
  happens).
- R11. Conflict in the upstack restack after cross-branch absorb is resumable
  via `stacc continue`, the same as today.

---

## Key Technical Decisions

### Rewrite the full chain in a single `rewrite_chain` call

`rewrite_chain` (already `pub(crate)`, shared with `modify --into`) takes an
ordered commit slice and an optional `parent_of_first`. Passing ALL commits from
the deepest affected branch's base through the current tip produces a single
contiguous rewrite. The resulting `new_by_old` map covers every commit in the
affected range, including those in intermediate branches.

This avoids any inter-branch git rebase, consistent with the existing absorb
design principle ("the apply is an in-memory `commit-tree` rewrite of the
branch's own commit chain, never a `git rebase`").

### Use `ops::downstack_chain` to build the absorbable commit set

`ops::downstack_chain(&state, &branch, &repo.trunk)` returns the tracked
branches from the deepest (closest to trunk) up through the current branch,
inclusive, in bottom-up order. For each branch in this chain, compute its own
commit range (`base.hash..tip` with the same stale-hash guard already in
`absorb()`), and union all those SHAs into a single `absorbable_set`. Pass
`absorbable_set` to the updated `map_hunks()` in place of `own_set`.

The `commit_to_branch` map (`BTreeMap<String, String>`, commit SHA -> branch
name) lets `absorb()` determine the deepest affected branch and the rewrite
range without an extra blame pass.

### Move ALL affected branch refs, not only the current branch

After `rewrite_chain` returns `new_by_old`, move refs for every branch in the
rewrite range (from deepest affected branch to current) using `git.update_ref`
with the old tip as a CAS lease. This keeps each branch ref pointing to its
rewritten tip, consistent with what a user would see on `git log` or `git
checkout <branch>`.

This differs from `modify --into`, which only moves the current branch via
`reset_mixed` and leaves intermediate refs stale. For cross-branch absorb,
leaving intermediate refs stale would break `git checkout a` (it would show
old content) and confuse future `stacc absorb` or `stacc restack` calls.

### Update `base.hash` for all branches in the rewrite range

For each consecutive pair `[parent_br, child_br]` in the affected chain slice,
update `child_br.base.hash` to the new tip of `parent_br`
(`new_by_old[old_tip_of_parent_br]`). This matches the `modify_into` pattern
(`chain[pos..].windows(2)`) and keeps the recorded base hash in sync with the
rewritten history so subsequent `is_ancestor` checks in restack and absorb
resolve correctly.

### Upstack restack is unchanged

`ops::upstack_order(&state.branches, &branch)` (the current branch and its
transitive children) is passed to `restack_with_recovery` as today. The restack
engine skips branches that already sit on their base's new tip; all chain
members were rewritten in-memory together, so only true upstack children (above
current) will need rebasing.

### `outside_branch` for commits not in any tracked downstack branch

Blame commits that predate the full chain (e.g., a commit on trunk, or a commit
in a sibling stack) still produce `outside_branch`. R3 is unchanged.

### Non-breaking JSON change

Add `"branch"` to each entry in the `"mapping"` JSON array. Existing consumers
that parse by key are unaffected; new consumers can use it to identify
cross-branch targets. The pretty-print output groups by branch when cross-branch
hunks are present.

---

## Implementation Units

### Unit 1: Core cross-branch routing in `absorb()`

**File:** `crates/stacc/src/commands/absorb.rs`

**Scope of changes:**

1. In `absorb()`, after computing `own_commits`/`own_set`, compute the
   downstack chain and build `commit_to_branch` / `absorbable_set`:

   ```rust
   // Conceptual sketch (not final code):
   let chain = ops::downstack_chain(&state, &branch, &repo.trunk)?;
   let mut commit_to_branch: BTreeMap<String, String> = BTreeMap::new();
   let mut chain_tips: BTreeMap<String, String> = BTreeMap::new();
   let mut chain_excludes: BTreeMap<String, String> = BTreeMap::new();
   for br in &chain {
       let bs = state.branches.get(br).expect("in chain");
       let br_tip = git.rev_parse(br)?;
       chain_tips.insert(br.clone(), br_tip.clone());
       let br_exclude = if git.is_ancestor(&bs.base.hash, br).unwrap_or(false) {
           bs.base.hash.clone()
       } else {
           git.rev_parse(&bs.base.name)?
       };
       chain_excludes.insert(br.clone(), br_exclude.clone());
       for sha in git.rev_list(&br_exclude, &br_tip)? {
           commit_to_branch.insert(sha, br.clone());
       }
   }
   let absorbable_set: BTreeSet<&str> = commit_to_branch.keys().map(String::as_str).collect();
   ```

2. Pass `absorbable_set` to `map_hunks()` instead of `own_set`.

3. After mapping: if `mapping.mapped` is empty, no-op report (unchanged). If
   non-empty, compute `rewrite_start`:

   ```rust
   // Deepest branch index in the chain that has at least one mapped hunk.
   let rewrite_start_idx = chain.iter()
       .position(|br| mapping.mapped.iter().any(|m| commit_to_branch.get(&m.commit) == Some(br)))
       .expect("at least one mapped hunk, so at least one branch");
   let affected_chain = &chain[rewrite_start_idx..]; // from deepest to current
   let parent_of_first = &chain_excludes[&affected_chain[0]];
   let rewrite_commits = git.rev_list(parent_of_first, &tip)?;
   ```

4. Call `rewrite_chain(git, Some(parent_of_first), &rewrite_commits, &mapping.mapped)`.

5. Move all affected branch refs (bottom-up to avoid dangling parent chains):
   ```rust
   for br in affected_chain {
       let old_tip = &chain_tips[br];
       if let Some(new_tip) = new_by_old.get(old_tip) {
           git.update_ref(&format!("refs/heads/{br}"), new_tip, Some(old_tip))?;
       }
   }
   ```

6. Update `state.base.hash` for child branches:
   ```rust
   for pair in affected_chain.windows(2) {
       // pair[0] = parent branch, pair[1] = child branch
       if let Some(b) = state.branches.get_mut(&pair[1]) {
           b.base.hash = git.rev_parse(&pair[0])?;
       }
   }
   ```

7. Extend the worktree guard to include `affected_chain` (excluding the current
   branch, which is already the HEAD) alongside the existing upstack.

8. `reset_mixed` and `restack_with_recovery` calls are unchanged.

9. Update `report_dry_run` and `report_result` to pass `commit_to_branch` for
   branch-aware output.

**Change to `map_hunks()`:**

Rename the `own_set` parameter to `absorbable_set`. The classification logic is
unchanged: commits in `absorbable_set` map; commits outside stay
`outside_branch`. No other changes to `map_hunks`.

**`Mapped` struct:** No change to the struct itself. The `commit` field is
sufficient; `absorb()` uses `commit_to_branch` to derive the owning branch.

**JSON mapping entry:** Add `"branch"` to each `mapping_json` entry by passing
`commit_to_branch` into `mapping_json()`.

**Test scenarios** (in `crates/stacc/tests/absorb.rs`):

- `absorb_cross_branch_routes_hunk_into_downstack_commit`: two-branch stack
  (`a` on `main`, `b` on `a`); stage a hunk editing a line introduced by a
  commit on `a`; verify absorb succeeds, `a`'s commit tree is rewritten, `b`
  is rebased onto new `a`, and the index has no remaining changes for that hunk.

- `absorb_cross_branch_and_own_branch_simultaneously`: same two-branch stack;
  stage two hunks, one blaming to `a`, one blaming to `b`; verify both are
  absorbed in a single call, both branch refs move, and only unrelated staged
  hunks (if any) remain.

- `absorb_cross_branch_dry_run_includes_branch_field`: `--dry-run` JSON output
  includes `"branch": "a"` in the `targets` and `mapping` entries for
  downstack-routed hunks.

- `absorb_outside_branch_unchanged_when_not_in_any_chain_branch`: stage a hunk
  editing a line from a commit on `main` (predating the stack); verify the hunk
  stays `outside_branch` and nothing is mutated.

- `absorb_cross_branch_worktree_guard_fires_for_downstack_branch`: create a
  second worktree checked out on `a`; verify absorb refuses before mutating
  anything.

---

## JSON Output Shape

No breaking changes. Only additions:

```json
{
  "op": "absorb",
  "branch": "feat-b",
  "sha": "<new-tip>",
  "absorbed": 2,
  "mapping": [
    {"path": "foo.rs", "old_start": 10, "old_count": 3, "commit": "<sha>", "branch": "feat-a"},
    {"path": "bar.rs", "old_start": 5,  "old_count": 1, "commit": "<sha>", "branch": "feat-b"}
  ],
  "unabsorbed": [],
  "restacked": []
}
```

Dry-run gains the same `"branch"` field in `targets`:

```json
{
  "op": "absorb",
  "dry_run": true,
  "branch": "feat-b",
  "targets": [
    {"commit": "<sha>", "branch": "feat-a", "hunks": 1, "subject": "feat: add foo"},
    {"commit": "<sha>", "branch": "feat-b", "hunks": 1, "subject": "refactor: clean bar"}
  ],
  "mapping": [...],
  "unabsorbed": []
}
```

---

## Scope Boundaries

**In scope:**
- Linear downstack chains (the current branch's direct ancestors up to trunk).
- Hunks routing to any branch in the chain, including multi-hop (skip one or
  more intermediate branches with no matching commits).
- Dry-run cross-branch reporting.

**Out of scope (deferred):**
- Forked stacks (branches that share a common ancestor but are not in the
  current branch's downstack). A hunk blaming to a sibling branch's commit
  remains `outside_branch`.
- Conflict recovery specific to cross-branch rewrite failures. `rewrite_chain`
  is in-memory and either fully succeeds or returns an error before any mutation.
  Conflicts can only arise in the post-absorb upstack restack, where existing
  `stacc continue` / `stacc abort` recovery already applies.
- Untracked branches in the downstack. `downstack_chain` errors on `Untracked`;
  the error surfaces to the user unchanged.

---

## Dependencies and Sequencing

No new dependencies. All functions used (`ops::downstack_chain`,
`ops::upstack_order`, `absorb::rewrite_chain`, `git.rev_list`, `git.update_ref`,
`git.reset_mixed`) are already available in the crate.

Implement and test Unit 1 in a single commit. The implementation is fully
self-contained; no staging or feature-flag needed.

---

## Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| `rewrite_chain` spans many commits on a deep stack, so a single `splice_hunks` failure aborts with no mutation. | Existing behavior; no change. Failure before any `update_ref` call. |
| Moving multiple branch refs sequentially (not atomically) means a failure mid-sequence leaves some refs moved and others not. | Move refs in reverse order (deepest first) so if a CAS failure occurs, the higher-level branch refs have not moved yet and the inconsistency is minimal. Consider adding a recovery note in the error message. |
| `state.base.hash` updates happen after ref moves; a crash between them leaves state slightly stale. | The existing `is_ancestor` fallback in `absorb()` and `restack_forced()` handles stale hashes gracefully. |
| `ops::downstack_chain` errors for untracked intermediate branches. | Surface the `Untracked` error as a `Usage` error pointing the user to `stacc track`. |
