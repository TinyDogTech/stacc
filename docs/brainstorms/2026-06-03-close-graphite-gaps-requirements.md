---
date: 2026-06-03
topic: close-graphite-gaps
title: Close the graphite command gaps (dual-audience command surface)
---

# Close the graphite command gaps

## Summary

Grow stacc from its 7 shipped commands to a full stacked-diff command surface
that both an AI agent and a human at a terminal can drive end-to-end: a core
edit loop (`create`, `modify`, standalone `restack`), unified conflict recovery
(`continue`, `abort`), stack navigation (`checkout`, `up`, `down`, `top`,
`bottom`), stack manipulation (`move`, `rename`), a graphite-style visual `log`
(plus `log --short`), a `pr` URL command, and a `merge` that closes the loop.
Every command stays fully scriptable and JSON-complete; interactive pickers are
a TTY-only convenience for humans, never required.

## Problem Frame

stacc today ships the end-to-end happy path (`init`, `track`, `submit`, `sync`,
`log`, `status`, `auth`) and proxies everything else to `git`. That is enough to
prove the model but not enough to live in. Compared against graphite, stacc is
missing the everyday primitives a stacked workflow leans on: there is no
one-step way to start a stacked branch, no way to amend-and-restack, no
standalone restack (the engine exists but is reachable only through the heavier
`sync`), no navigation, no rename that keeps the stack intact, and no way to
merge from the CLI. The restack logic, conflict-context capture, and
fork-point recovery already exist inside `sync` — the gap is exposure and
ergonomics, not a missing core.

Two audiences feel the gap differently. An agent falls back to raw `git` plus
manual `track`/`sync`, which works but loses the stack-aware guarantees. A human
gets none of graphite's daily-driver ergonomics (navigation, a readable stack
graph). This batch closes both at once.

## Key Decisions

- **Dual-audience, non-interactive core.** stacc is optimized for both the agent
  and the human. The invariant from the original thesis holds: every command is
  fully scriptable, honors `--no-interactive` and `--format json`, and never
  requires a prompt. Interactive selection (e.g. `checkout` with no argument) is
  a convenience layer that appears only on a TTY and always has an explicit
  non-interactive form.

- **Everything in one batch, sequenced on a shared engine.** All sixteen named
  commands ship in this effort rather than a minimal loop first. The commands
  cluster tightly and most reuse one restack engine, so the work is largely
  sequencing rather than independent builds.

- **Unified conflict-recovery model.** `continue` and `abort` stop being
  sync-only. Any operation that can conflict (`restack`, `modify`, `move`,
  `sync`, `merge`) is resumable with `stacc continue` and cancelable with
  `stacc abort`, sharing one conflict-context contract. The continuation record
  must identify which operation was in flight, not just the remaining branches,
  so `continue` knows how to finish it.

- **`merge` clears the ready downstack to current.** `stacc merge` walks the
  current branch's downstack (trunk up to current) bottom-up, merging each PR
  that is "ready" (mergeable: approved and required checks green), stopping at
  the first PR that is not ready, then runs `sync` to reconcile and restack.
  Squash-merge only, matching repo enforcement.

- **`rename` is the sanctioned rename path.** Renaming through raw `git branch
  -m` silently breaks stack state (keyed by branch name), children's base
  pointers, and the PR head. `stacc rename` owns the operation and updates all
  three; it should also detect and repair branches renamed outside stacc.

- **Flat top-level command surface.** Commands are exposed as flat top-level
  verbs (`stacc create`, `stacc up`, `stacc restack`) mirroring graphite's
  aliases and git-spice, not as grouped subcommands.

## Actors

- A1. **Coding agent** — the primary driver. Runs commands with `--format json`
  and `--no-interactive`; reads structured output and conflict-context files to
  recover without a human.
- A2. **Human operator** — works at a terminal; benefits from interactive
  pickers and the visual stack graph, but can drop to explicit flags at any
  time.
- A3. **GitHub** — the forge. Hosts PRs, reports mergeability (approval +
  required checks), and performs squash merges.

## Requirements

Requirements are grouped by capability. `submit` and `track` already ship; this
batch extends them only where noted.

### Core edit loop

- R1. `create` starts a new stacked branch in one step: it creates the branch on
  top of the current branch, commits the staged changes, sets the new branch's
  base to the current branch, and tracks it.
- R2. `modify` folds work into the current branch and repairs the stack: it
  amends the current branch's commit (or appends a new commit when asked), then
  automatically restacks everything upstack of it.
- R3. `restack` is available as a standalone command that rebases tracked
  branches onto their current bases, reusing the existing restack engine
  (fork-point recovery and conflict-context capture included). It is scoped by
  default to the current branch and its upstack, with an option to restack the
  whole stack.

### Conflict recovery

- R4. `continue` resumes whichever stacc operation is in flight after the user
  resolves a conflict, completing the remaining work for that operation.
- R5. `abort` cancels the in-flight operation, restores the working state that
  preceded it, and clears the continuation and conflict-context artifacts.
- R6. Any operation that can conflict records its identity and remaining work
  when it stops on a conflict, so `continue` and `abort` act on the correct
  operation. The conflict-context contract (conflicted files, base PR context)
  is shared across all such operations.

### Navigation

- R7. `checkout` switches to a tracked branch. With an explicit branch argument
  it is deterministic. With no argument on a TTY it offers an interactive
  picker; with no argument under `--no-interactive` or no TTY it fails with a
  structured error rather than prompting.
- R8. `up`, `down`, `top`, and `bottom` move the checked-out branch through the
  current stack (parent/child/tip/base-most). They are deterministic and need no
  interactivity. `up`/`down` accept a step count.

### Stack manipulation

- R9. `move` re-parents the current branch (and its upstack) onto a different
  base, updates the recorded base, and restacks onto the new base.
- R10. `rename` renames a tracked branch and keeps the stack consistent: it
  updates the state key, re-points any children's base, and updates the PR head
  on GitHub. It also detects and repairs a branch that was renamed outside
  stacc.

### Lifecycle and forge

- R11. `merge` walks the current branch's downstack (trunk up to current)
  bottom-up, merging each PR that is ready (mergeable: approved and required
  checks green) via squash merge, stopping at the first PR that is not ready,
  then runs `sync` to reconcile merged branches and restack survivors.
- R12. `pr` outputs the current branch's PR URL (and opens it in a browser for a
  human on a TTY); under `--format json` it emits the URL as structured data.

### Display

- R13. `log` renders a graphite-style visual stack graph: a vertical graph with
  the current branch marked, each branch's PR state shown, and a needs-restack
  indicator for branches that have drifted from their base.
- R14. `log --short` renders a compact one-line-per-branch list.
- R15. `--format json` output for `log` (and every other command) remains
  structured and unchanged by the visual upgrade; the graph is a pretty-format
  concern only.

### Cross-cutting

- R16. Every new command supports `--format json|pretty`, `--color`, and
  `--no-interactive`, and produces a structured error (never a silent prompt)
  when a required input is missing under `--no-interactive` or off a TTY.
- R17. Built-in short aliases ship for the high-traffic commands (e.g. `co`,
  `u`, `d`) alongside the existing user-defined alias system; the alias surface
  does not require new configuration to be useful.

## Key Flows

- F1. **Daily edit loop**
  - **Trigger:** A1/A2 starts new stacked work.
  - **Steps:** `create` a branch with staged changes → make further changes and
    `modify` to fold them in (auto-restacking upstack) → `submit` to push and
    open/update PRs.
  - **Covered by:** R1, R2, R3.

- F2. **Conflict recovery (any operation)**
  - **Trigger:** `restack`, `modify`, `move`, `sync`, or `merge` hits a rebase
    conflict.
  - **Steps:** The operation stops, records its identity and remaining branches,
    and writes conflict context. The driver resolves the conflict and runs
    `continue` to finish, or `abort` to roll back and clear artifacts.
  - **Covered by:** R4, R5, R6.

- F3. **Merge and reconcile**
  - **Trigger:** A downstack of approved, green PRs is ready to land.
  - **Steps:** `merge` squash-merges ready PRs from trunk up to current,
    bottom-up, stopping at the first not-ready PR, then `sync` reconciles merged
    branches and restacks the survivors.
  - **Covered by:** R11.

- F4. **Navigation**
  - **Trigger:** A2 wants to move around the stack; A1 wants to position HEAD.
  - **Steps:** `checkout <branch>` jumps directly; `up`/`down`/`top`/`bottom`
    step through the stack; bare `checkout` offers a picker on a TTY.
  - **Covered by:** R7, R8.

## Acceptance Examples

- AE1. **Covers R11.** **Given** a stack of three PRs where the bottom is
  approved and green, the middle is approved and green, and the top is not yet
  approved, **when** `merge` runs from the top branch, **then** the bottom and
  middle PRs squash-merge bottom-up, the top PR is left open, and `sync` runs
  afterward to reconcile.
- AE2. **Covers R7, R16.** **Given** no branch argument, **when** `checkout`
  runs on a TTY, **then** an interactive picker appears; **when** it runs under
  `--no-interactive` or with no TTY, **then** it exits with a structured error
  and no prompt.
- AE3. **Covers R10.** **Given** a tracked branch with children and an open PR,
  **when** `rename` runs, **then** the state key, every child's base pointer, and
  the PR head are all updated to the new name.
- AE4. **Covers R2, R4, R6.** **Given** `modify` triggers an upstack restack that
  conflicts on the second branch, **when** the user resolves the conflict and
  runs `continue`, **then** the modify operation resumes and finishes the
  remaining upstack branches.
- AE5. **Covers R5.** **Given** an operation stopped on a conflict, **when**
  `abort` runs, **then** the working tree returns to its pre-operation state and
  the continuation and conflict-context artifacts are removed.

## Scope Boundaries

### Deferred for later

- Other graphite commands not named in this batch: `delete`, `untrack`, `fold`,
  `squash`, `split`, `edit`, `info`, and similar. They are part of eventual
  parity, not this effort.
- Richer `submit` scope variants (upstack-only, whole-stack) and flags such as
  `--draft` beyond what already ships, unless they fall out naturally.

### Outside this batch's identity

- MCP server, TUI, parallel-agent / worktree locking, and additional forges
  (GitLab, Bitbucket) remain v2 per `plans/stacc.md`.

## Dependencies / Assumptions

- The shared restack/operations engine is assumed to move out of
  `crates/stacc/src/commands.rs` into the currently-empty `stacc-core` crate so
  `restack`, `modify`, `move`, `merge`, and `sync` reuse one implementation. The
  exact extraction is a planning decision; this batch assumes it happens.
- The existing conflict-context and continuation mechanism is extended (to carry
  operation identity), not replaced.
- GitHub exposes a usable mergeability signal (approval state + required checks)
  for R11; squash-merge is the only merge method (repo-enforced).
- Visual-log rendering specifics (glyphs, layout) are a planning/design detail,
  not resolved here.

## Outstanding Questions

### Resolve before planning

- None. Product behavior is settled; remaining items are implementation
  choices.

### Deferred to planning

- The exact default scope and flag surface for `restack` (current upstack vs.
  whole stack) and for `modify` (amend vs. append-commit).
- How `move` handles a branch that has its own upstack (move the whole subtree
  vs. just the branch).
- How `rename` detects an externally-renamed branch (reflog, ref scan) and how
  aggressively it repairs.
- Visual `log` glyph and layout design.
