---
title: "fix(stacc): [STA-125] extend merge retarget to forked stacks and improve post-merge checkout"
type: fix
date: 2026-06-18
---

# fix(stacc): [STA-125] extend merge retarget to forked stacks and improve post-merge checkout

## Summary

STA-119 fixed `stacc merge` to explicitly retarget the direct children of `chain.last()` before any branch deletions. Two gaps remain. First: in a forked stack, non-chain children of intermediate chain branches are not retargeted and get stranded when their parent branch is deleted. Second: when the starting branch is itself merged (its local ref deleted), the user lands on trunk and cannot run `stacc submit` without a manual `stacc checkout` first.

---

## Problem Frame

**Retargeting gap (fork case).** `retarget_children_to_trunk` (`operations.rs:1877`) builds `top_upstack` from `ops::children` of `chain.last()` only. In a forked stack where the user is on one fork's tip, intermediate branches can have off-chain children:

```
trunk -> A -> B -> C   (user on D; chain = [A, B, D])
                -> D
```

`top_upstack` = children of D (chain.last()) not in chain = `[]`. B's other child C is never added to `to_retarget`. When A and B are squash-merged and their branches deleted, C's PR base points at the deleted branch B.

The adoption pass (lines 1766-1775) also only covers chain branches, so a C whose PR was opened outside stacc is never adopted and thus never retargeted either.

**Post-merge checkout gap.** When the starting branch (B in a chain [A, B]) is included in the merge and its local ref is cleaned up, lines 1851-1856 fall back to the trunk:

```rust
let target = if git.ref_missing(&current) {
    &repo.trunk
} else {
    &current
};
```

The user is now on trunk with C remaining in their stack, but `stacc submit` from trunk is a usage error (`"cannot submit the trunk branch"`).

---

## Requirements

**Retarget correctness**

- R1. `retarget_children_to_trunk` retargets open PRs for non-chain children of any chain branch, not only `chain.last()`.
- R2. The adoption pass in `merge` includes non-chain children of all chain branches so externally-opened PRs are recorded in state before the retarget pass runs.
- R3. Retargeting is idempotent: re-running `merge` after a partial failure produces no net-new side effects for already-retargeted PRs.

**Post-merge checkout**

- R4. After a merge that deletes the starting branch's local ref, `stacc merge` checks out the first surviving direct child of the starting branch (as recorded in state before the merge ran), not the trunk.
- R5. If no direct child of the starting branch survives (whole stack merged), checkout falls back to trunk (current behavior unchanged).
- R6. The `--keep-branches` flag is not affected; the checkout target selection logic applies equally regardless of ref cleanup.

---

## Key Technical Decisions

- **Scope `off_chain_children` to direct children of chain members (not transitive).** Grandchildren of chain branches are safe: their immediate parent still exists after the merge (only chain branches are deleted). Only direct children of deleted chain branches are orphaned.

- **Extend adoption to off-chain children before retargeting.** `retarget_children_to_trunk` silently skips branches without a recorded PR (line 1906). Without adoption, a C whose PR was opened via `gh` or the GitHub web UI would still be stranded even after the retarget list is extended. The adoption pass is idempotent and runs `--offline` too (same as the existing chain adoption).

- **Pre-capture `current_children` before state mutation.** `merge_stack` mutates state (reparents children, drops merged branches). By the time lines 1850-1856 run, `current` has been removed from `state.branches` and its children are reparented to trunk. Capture `ops::children(&state.branches, &current)` before calling `retarget_children_to_trunk`.

- **Do not change the `retarget_children_to_trunk` signature.** The change is internal: replace the `top_upstack` binding with an `off_chain_children` binding that iterates all chain branches. The call site (`operations.rs:1782`) stays identical.

---

## Implementation Units

### U1. Extend off-chain retargeting and adoption to all chain branches

**Goal:** Replace `top_upstack` (children of `chain.last()` only) with `off_chain_children` (non-chain children of any chain branch). Also run a second adoption pass for these branches before the retarget executes.

**Files:**
- `crates/stacc/src/commands/operations.rs`
- `crates/stacc/tests/merge.rs`

**Changes:**

In `merge` (before line 1782), compute off-chain children:

```rust
let off_chain_children: Vec<String> = chain
    .iter()
    .flat_map(|branch| {
        ops::children(&state.branches, branch)
            .into_iter()
            .filter(|b| !chain_set.contains(b.as_str()))
    })
    .collect();
```

Note: `chain_set` needs to be built before this point; move its construction out of `retarget_children_to_trunk` if it is needed at both call sites, or build it inline in `merge` and pass it as a parameter. The simpler option is to build it inline in `merge` and also update the internal binding inside `retarget_children_to_trunk`.

Run a second adoption pass for `off_chain_children` members that lack a recorded PR (analogous to lines 1766-1772). The returned `adopted_merged` from this pass can be discarded: off-chain children are not part of the merge walk and their merged state has no impact on the walk.

Inside `retarget_children_to_trunk`, replace lines 1891-1904:

```rust
// was: top_upstack = children of chain.last() not in chain
// now: off_chain_children = non-chain children of ANY chain branch
let off_chain_children: Vec<String> = chain
    .iter()
    .flat_map(|branch| {
        ops::children(&state.branches, branch)
            .into_iter()
            .filter(|b| !chain_set.contains(b.as_str()))
    })
    .collect();
let to_retarget: Vec<&String> = chain
    .iter()
    .skip(1)
    .chain(off_chain_children.iter())
    .collect();
```

Update the doc comment at line 1885-1889 to describe both original cases plus the new "intermediate chain branch fork" case.

**Test scenarios in `merge.rs`:**

- Fork case: `trunk -> A -> B -> {C, D}`, PRs for all four; user on D (chain = [A, B, D]). After `merge`, verify C's PR base is patched to trunk before any merge, and C's PR remains open. Modeled after `merge_retargets_upstack_child_of_chain_top`.

- Linear case regression: `trunk -> A -> B -> C`, user on B. Verify C's PR is still retargeted (R3, no regression from the STA-119 path). The existing `merge_retargets_upstack_child_of_chain_top` covers this; no new test needed, but the existing test must continue to pass.

### U2. Post-merge checkout: land on first surviving upstack branch

**Goal:** After a merge that deletes the starting branch's local ref, check out the first surviving direct child of the starting branch rather than falling back to trunk.

**Files:**
- `crates/stacc/src/commands/operations.rs`
- `crates/stacc/tests/merge.rs`

**Changes:**

Before the `retarget_children_to_trunk` call (~line 1782), capture:

```rust
let current_children: Vec<String> = ops::children(&state.branches, &current);
```

Modify lines 1851-1856:

```rust
let target = if git.ref_missing(&current) {
    // Starting branch was merged; land on the first surviving child
    // so the user can run `stacc submit` immediately.
    current_children
        .iter()
        .find(|b| !git.ref_missing(b))
        .map(String::as_str)
        .unwrap_or(&repo.trunk)
} else {
    &current
};
```

No change to the `--keep-branches` path: ref cleanup is what triggers the fallback; `--keep-branches` keeps the ref, so `git.ref_missing(&current)` stays false and the user lands back on `current` as before.

**Test scenarios in `merge.rs`:**

- Mid-stack merge: `trunk -> A -> B -> C`, user on B; A and B merge, C remains. Verify HEAD is C after the merge command returns. Use `--keep-branches` to avoid, then omit it to trigger the new path. New test, analogous to `merge_restores_the_starting_branch`.

- Whole-stack merge: `trunk -> A -> B`, user on B; both A and B merge, no children. Verify HEAD is trunk (R5, existing behavior). Existing test `merge_of_the_whole_stack_ends_on_the_trunk_with_refs_gone` covers this.

---

## Scope Boundaries

- **Deferred: adoption of off-chain children for branches whose PR was opened externally and the branch is not directly reachable from chain members.** U1 extends adoption to direct off-chain children. Deeper upstack branches (grandchildren of chain members) are safe without adoption (their immediate parent still exists after the merge).

- **Out of scope: `stacc submit` from trunk error UX.** The usage error for `stacc submit` on trunk is correct behavior; the post-merge checkout fix (U2) removes the motivation to submit from trunk after a mid-stack merge.

- **Out of scope: `stacc sync` retargeting.** The same retarget logic gap may exist in `sync`'s reconcile path. That is a separate audit.

---

## Acceptance Examples

- AE1. Stack A -> B -> {C, D}, user on D. `stacc merge` completes. Before any branch is deleted, GitHub receives PATCH requests to set C's PR base and D's PR base both to trunk.

- AE2. Stack A -> B -> C, user on B. `stacc merge` completes (A and B merged). `git branch --show-current` prints `c`, not `main`.

- AE3. Stack A -> B, user on B (whole stack). `stacc merge` completes. `git branch --show-current` prints `main`.
