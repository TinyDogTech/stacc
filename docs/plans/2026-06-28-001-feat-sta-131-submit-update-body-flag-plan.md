---
title: "feat(stacc): [STA-131] add --update-body flag to refresh stale PR bodies"
type: feat
plan_for: STA-131
date: 2026-06-28
---

# feat(stacc): [STA-131] add --update-body flag to refresh stale PR bodies

## Summary

After STA-121 shipped no-clobber body protection, `stacc submit` omits the `body`
field on UPDATE whenever there is no explicit description source (`--description`
on this run, or a previously-stored description). This correctly preserves manual
PR body edits, but it also means a PR body written once at creation never tracks
the commit body as the change grows. There is no clean way to say "refresh this
PR's body from the current commit" short of retyping the whole thing via
`--description`.

This plan adds an opt-in `--update-body` flag to `stacc submit`. When set, the
current branch's PR body is refreshed on UPDATE from the current (reflowed) commit
body, overriding a stale stored description. The flag is a one-shot sync, not a
mode switch: it clears any stored description so future plain submits return to the
no-clobber default and continue to preserve later manual edits. Default behavior
(no flag) is unchanged.

---

## Problem Frame

`stacc submit` resolves the PR body through a cascade and, post-STA-121, decides
separately whether to send it on UPDATE (`crates/stacc/src/commands.rs:744-768`):

- `body` (used on CREATE) = `--description` > `stored_desc` > `reflow_body(commit_body)`.
- `update_body` (used on UPDATE) = `--description` > `stored_desc` > **omit**.

The `omit` arm is STA-121's no-clobber protection: when the user never supplied a
description, the body is left as GitHub holds it. The asymmetry the issue describes
is now code-true: title is always sent on UPDATE (`title: Some(title)`,
`commands.rs:776`), body is conditionally omitted.

Consequence (the issue's concrete case, PR #77): a PR opened early, whose change
later grew via `stacc modify`, keeps its original body forever. The body describes
only the first slice of work. The only refresh path is `stacc submit --description
"<retype the whole body>"`, which also flips the branch into stored-description
mode permanently.

The motivating case is the common one: the user **never** used `--description`, so
`stored_desc` is `None` and the body is write-once-at-create. That is exactly the
state `--update-body` targets.

This plan implements **Option 1** from the issue (explicit refresh flag, default
never clobbers). Options 2 (`stacc pr edit` command) and 3 (document `gh pr edit`
as the escape hatch) are not pursued; see Scope Boundaries.

---

## Requirements

- R1. Add a boolean `--update-body` flag to `stacc submit`. Default off; absent flag
  reproduces today's behavior exactly.
- R2. On UPDATE, when `--update-body` is set and `--description` is not, the PATCH
  payload includes `body` set to `reflow_body(commit_body)` for the **current
  branch**, even when a `stored_desc` exists (refresh overrides a stale stored body).
- R3. `--description` takes precedence over `--update-body`: if both are passed,
  the explicit `--description` text is used and persisted, and `--update-body` has
  no additional effect.
- R4. `--update-body` is a one-shot sync, not a stored-description mode: when it
  refreshes the body, the branch's stored description is cleared so subsequent plain
  submits return to the no-clobber `omit` arm (GitHub holds the refreshed value;
  later manual edits are preserved).
- R5. `--update-body` only affects the UPDATE path. On CREATE it is a no-op (the new
  PR body is already `reflow_body(commit_body)` when no `--description` is given).
- R6. `--update-body` applies to the current branch only, mirroring `--title` /
  `--description` scoping. Non-current downstack branches are unaffected.

---

## Key Technical Decisions

**Current-branch-only scope, mirroring `--description` / `--title`.**
`--title` and `--description` already resolve as current-branch-only overrides
(`commands.rs:733-759`); the downstack branches fall back to their stored/commit
values. `--update-body` follows the same rule. Refreshing every downstack body in
one run would clobber bodies on PRs the user did not mean to touch, and the flag
name reads as "this PR's body." Downstack-wide refresh is deferred (see Scope
Boundaries).

**Precedence: `--description` > `--update-body` > stored/omit.**
Explicit user text is the strongest signal, so `--description` wins. `--update-body`
sits between explicit text and the default: it forces the commit body even over a
stale `stored_desc`, which the plain default would otherwise keep. This is expressed
as a small if/else chain at the `update_body` computation site, not a combinator,
because the three arms have distinct persistence side effects (below).

**One-shot sync that preserves no-clobber, implemented by clearing `stored_desc`.**
The subtle correctness point: if `--update-body` merely sent the commit body without
touching state, a branch that already had a `stored_desc` would revert to that stale
stored value on the very next plain submit. To stay stable in all cases, when
`--update-body` refreshes the body it also clears the branch's stored description
(persists `pr_description = None`). After the refresh: this submit sends the commit
body; the next plain submit takes the `omit` arm; GitHub retains the refreshed body;
and STA-121's protection of later manual edits is intact. This keeps `--update-body`
a deliberate action rather than a mode that silently disables no-clobber.

**Reuse the existing `reflow_body` + `commit_body` path.**
The CREATE branch already computes `reflow_body(&git.commit_body(branch)...)`
(`commands.rs:753`). `--update-body` uses the identical expression so wrapped commit
bodies render as clean GFM, consistent with STA-121's reflow fix. No new text
helper.

**Empty commit body sends an empty string.**
If the commit body is empty and `--update-body` is set, the PATCH sends `body: ""`.
The user explicitly asked to sync to the commit, so the synced value is empty. This
is distinct from the `omit` arm (field absent). Covered by a test.

**Lenient precedence over a clap `conflicts_with`.**
Passing both `--description` and `--update-body` is resolved by precedence (R3)
rather than a hard clap conflict error. An agent that sets both still gets a
well-defined result. `conflicts_with = "description"` was considered and rejected as
unnecessary friction; noted under Open Questions.

---

## High-Level Technical Design

The flag adds one arm to the `update_body` resolution and one branch to the state
persistence. Resolution for the **current branch** on UPDATE:

| `--description` | `--update-body` | `stored_desc` | `update_body` sent | stored `pr_description` after |
|---|---|---|---|---|
| set | (any) | (any) | explicit text | set to explicit text (unchanged from today) |
| unset | set | any | `reflow_body(commit_body)` | **cleared to None** |
| unset | unset | `Some(d)` | `d` | `Some(d)` (unchanged) |
| unset | unset | `None` | omit (field absent) | `None` (unchanged) |

Only the second row is new. CREATE is unaffected (body always set from the cascade).
Non-current branches always take the bottom two rows (no `--description` /
`--update-body` reaches them).

State transition the flag is designed around (current branch, no `--description`):

```
write-once stale body  --(submit --update-body)-->  body = commit body, stored_desc cleared
        |                                                       |
        | (plain submit: omit, GitHub keeps stale)              | (plain submit: omit, GitHub keeps refreshed)
        v                                                       v
   stays stale                                          manual GitHub edits preserved
```

Directional only, not implementation specification. The prose above is authoritative
where they differ.

---

## Scope Boundaries

### In scope
- A `--update-body` boolean flag on `stacc submit`.
- UPDATE-path body refresh from the reflowed commit body for the current branch.
- Clearing the stored description on refresh to keep no-clobber intact afterward.
- Flag documentation (clap `--help`, AGENTS.md, README submit reference if present).

### Deferred to Follow-Up Work
- **Downstack-wide refresh.** A variant that refreshes every branch in the chain
  from its own commit body (closer to how `title` already auto-refreshes downstack).
  Out of scope to keep the flag's blast radius to the current branch.
- **`stacc pr edit` command** (Option 2 from the issue). A dedicated body/title
  subcommand is a larger surface; not built here.
- **Auto-refresh without a flag.** Making body track the commit body by default
  would re-clobber manual edits and undo STA-121. Explicitly not pursued.
- **Title overwrite symmetry.** `title` is still always sent on UPDATE. A matching
  opt-out / no-clobber for title is a separate concern (already deferred by STA-121).

### Outside scope
- **Option 3, documenting `gh pr edit` as a sanctioned escape hatch.** Superseded by
  the native flag; AGENTS.md gains `--update-body`, not a `gh pr edit` exception.
- Changes to CREATE behavior or to the `reflow_body` algorithm itself.

---

## Implementation Units

### U1. Declare the `--update-body` flag on `SubmitArgs`

**Goal:** Add the boolean flag and its `--help` text so `stacc submit --update-body`
parses.

**Requirements:** R1.

**Dependencies:** None.

**Files:**
- `crates/stacc/src/cli.rs` (add field to `SubmitArgs`)

**Approach:**
Add a `pub update_body: bool` field to `SubmitArgs` (`cli.rs:457`) with an `#[arg(long)]`
attribute, mirroring the existing `update_only` / `draft` boolean flags
(`cli.rs:477-482`). Write a doc comment that becomes the `--help` line, e.g. that it
refreshes the current branch's PR body from the latest commit body on update and is a
no-op on first submit. `SubmitArgs` is not annotated with
`#[allow(clippy::struct_excessive_bools)]` today; adding a fourth bool may trip
`clippy::struct_excessive_bools` (threshold is >3 bools). If clippy flags it, add the
same `#[allow(...)]` with the existing justification comment used on `SyncArgs`
(`cli.rs:486-488`) and `RestackArgs`.

**Patterns to follow:** `update_only` and `draft` fields on `SubmitArgs`
(`cli.rs:475-482`); the `#[allow(clippy::struct_excessive_bools)]` precedent on
`SyncArgs` / `RestackArgs`.

**Test scenarios:**
- `Test expectation: none, flag declaration only.` Behavior is exercised in U2; clap
  parsing is covered transitively by the U2 integration tests invoking
  `["submit", "--update-body", "--json"]`.

**Verification:** `cargo build --workspace` succeeds; `stacc submit --help` lists
`--update-body`; `cargo clippy --workspace --all-targets` is clean (add the
`#[allow]` if `struct_excessive_bools` fires).

---

### U2. Wire `--update-body` into the UPDATE body resolution and state persistence

**Goal:** When `--update-body` is set on the current branch and `--description` is
not, send the reflowed commit body on UPDATE and clear the stored description so
future submits revert to no-clobber.

**Requirements:** R2, R3, R4, R5, R6.

**Dependencies:** U1.

**Files:**
- `crates/stacc/src/commands.rs` (extend `update_body` computation and the persist closure)
- `crates/stacc/tests/submit.rs` (new tests)

**Approach:**
Two touch points in `submit` (`commands.rs:655`):

1. Resolution (`commands.rs:764-768`). Replace the current `or` combinator with an
   explicit chain for the current branch so the new arm and its side effect are clear:

   ```
   // Directional sketch only -- not implementation specification.
   update_body = if is_current {
       if let Some((_, d)) = &desc_update { Some(d.clone()) }          // --description wins
       else if args.update_body { Some(reflow_body(&commit_body)) }    // refresh from commit
       else { stored_desc }                                            // default: stored or omit
   } else {
       stored_desc
   };
   ```

   Reuse the same `reflow_body(&git.commit_body(branch).unwrap_or_default())`
   expression already used on the CREATE side (`commands.rs:753`).

2. Persistence (`commands.rs:812-829`). When `--update-body` refreshed the current
   branch's body (i.e. `is_current && args.update_body && desc_update.is_none()`),
   clear that branch's `pr_description` (set to `None`) in the `store.update` closure.
   Track this with a small signal alongside the existing `title_update` /
   `desc_update` locals (e.g. a `clear_desc: Option<String>` holding the branch name),
   set where `update_body` is computed, and consumed in the closure next to the
   existing `desc_update` write (`commands.rs:823-826`). Do not set `desc_update` for
   the refresh, persisting a `Some` would defeat R4.

Leave the CREATE dispatch (`commands.rs:781-791`) untouched: R5 is satisfied because
the flag never reaches the create arm's body.

**Execution note:** Start with the httpmock UPDATE test asserting the PATCH body
contains the reflowed commit text, then make it pass. The exact-body assertion
pattern at `tests/submit.rs:945-950` makes the contract precise.

**Patterns to follow:** The `title_update` / `desc_update` optional-pair tracking
(`commands.rs:693-694, 818-827`); the no-clobber `omit` test
`submit_resubmit_without_description_omits_body_in_patch` (`tests/submit.rs:919-959`);
the reflow tests `submit_reflowed_wrapped_commit_body_on_create` (`tests/submit.rs:808`).

**Test scenarios:**
- Re-submit existing PR with `--update-body`, no `--description`, `stored_desc = None`,
  commit body grew since create: PATCH body contains the new reflowed commit body.
- Re-submit with `--update-body`, no `--description`, `stored_desc = Some("old")`:
  PATCH body contains the reflowed commit body (not `"old"`), proving refresh overrides
  a stale stored description.
- After a `--update-body` refresh, run a plain `stacc submit` (no flags): the second
  PATCH omits the `body` key (exact-body match `{"title":...,"base":...}`), proving
  R4, stored description was cleared and no-clobber resumed.
- Re-submit with both `--description "Explicit"` and `--update-body`: PATCH body is
  `"Explicit"` and the stored description persists as `"Explicit"` (R3, `--description`
  wins, no clear).
- First submit (CREATE) with `--update-body`: POST still includes the commit-body body;
  flag is a no-op on create (R5).
- Re-submit with `--update-body` and an empty commit body: PATCH includes `"body":""`
  (empty string sent, not omitted).
- Stack of two branches, current is the top: re-submit top with `--update-body`. Top
  branch PATCH carries the refreshed body; the downstack branch PATCH is unchanged from
  its no-flag behavior (omit or stored), proving R6 current-branch-only scope.
- Default regression: existing `submit_resubmit_without_description_omits_body_in_patch`
  and the reflow-on-create tests still pass unchanged (no-flag behavior preserved, R1).

**Verification:** `cargo test -p stacc submit` passes; httpmock assertions confirm
PATCH body presence/absence and content match the precedence table.

---

### U3. Document the flag

**Goal:** Reflect `--update-body` in project docs so the stale-body gap has a
documented native resolution and the agent workflow uses it instead of an escape hatch.

**Requirements:** R1 (discoverability).

**Dependencies:** U1, U2.

**Files:**
- `AGENTS.md` (submit guidance / `gh` substitution context)
- `README.md` (if it documents `submit` flags; implementer greps for the submit flag list)

**Approach:**
In `AGENTS.md`, note `--update-body` where submit's body handling is discussed and in
the "Still plain `git` + `gh`" context: the PR-body refresh is now a stacc native
command, so do **not** add `gh pr edit` as a sanctioned escape hatch (that was the
deferred Option 3). Keep `gh pr checks` / `gh auth` as the remaining escape hatches.
If `README.md` enumerates submit flags, add `--update-body` with a one-line gloss
matching the `--help` text from U1. Do not invent doc sections that do not already
exist; this unit only extends current flag references.

**Patterns to follow:** Existing `--description` / `--draft` flag mentions in the same
docs.

**Test scenarios:**
- `Test expectation: none, documentation only.`

**Verification:** `rg -n 'update-body' AGENTS.md README.md` shows the new references;
the `gh pr edit` escape hatch is not introduced.

---

## Open Questions

- **Deferred to implementation:** Whether to add `#[arg(conflicts_with = "description")]`
  instead of lenient precedence (R3). Current decision is lenient (precedence resolves
  it). If implementation finds the silent-precedence behavior confusing in `--help` or
  testing, a clap conflict is a one-line alternative, but it would reject an agent that
  sets both rather than doing the sensible thing.
- **Deferred to implementation:** Exact name of the persistence signal local
  (`clear_desc` vs threading a richer enum through the existing `desc_update`). Either
  works; pick whichever reads cleanest next to `title_update` / `desc_update` without
  widening their types unnecessarily.

---

## Risks & Dependencies

- **Re-opening STA-121 by accident.** The one real correctness risk is the clear-on-
  refresh step (R4). If it is omitted, a branch with a stored description reverts on the
  next submit, and a branch whose body was refreshed could later clobber manual edits.
  The third U2 test scenario (plain submit after refresh omits body) is the guard;
  treat it as required, not optional.
- **clippy `struct_excessive_bools`.** Adding a fourth bool to `SubmitArgs` may trip the
  lint. Mitigation is the existing `#[allow(...)]` precedent (U1). Low risk, mechanical.
- No new dependencies, no schema or state-format changes (the existing
  `pr_description: Option<String>` field already supports being cleared to `None`).

---

## Sources & Research

- Linear STA-131 issue (gap description, three options, interim decision).
- `crates/stacc/src/commands.rs:655-829` (`submit`: title/body cascade, `update_body`
  no-clobber arm, persistence closure).
- `crates/stacc/src/cli.rs:455-483` (`SubmitArgs` flag declarations).
- `crates/stacc/tests/submit.rs:919-959` (STA-121 no-clobber exact-body test pattern),
  `:808-867` (reflow-on-create tests).
- Sibling plan `docs/plans/2026-06-19-001-fix-sta-121-submit-pr-body-reflow-preserve-plan.md`
  and PR #114 (STA-121, no-clobber + reflow, the feature this builds on).
