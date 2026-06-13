---
date: 2026-06-11
topic: seamless-local-multi-forge
title: Seamless local-first stacking, and the path to forge-equal
---

# Seamless local-first stacking, and the path to forge-equal

## Summary

Slice 1 makes stacc's forge-less local experience first-class: `sync` can fetch
trunk and restack with zero forge API calls, merges that happen out of band are
caught by a local heuristic that proposes while an explicit `stacc merged`
disposes, and a per-repo local mode removes flag-spam. This is the universal
floor every forge sits on. Slice 2, captured here as a planned direction,
introduces a forge-equal abstraction (GitLab first) behind a capability-interface
model. `submit` and `merge` stay GitHub-only until slice 2 lands.

## Problem Frame

The local stack mechanics are already pure git: track, create, restack, move,
reorder, fold, squash, split, delete/pop, navigation, `log` structure, and
`status` touch no remote. Only six command areas reach a forge: `sync`
(merged-PR detection), `merge`, `submit`, `auth`, and PR enrichment in `log` and
`info`. A developer on GitLab, Bitbucket, plain git, or no remote can already
build and maintain a stack; what they cannot do is the forge half (open PRs,
squash-merge them, auto-detect merges).

STA-94 sharpened the edge. Before it, a non-GitHub remote silently no-opped the
forge parts; now `sync` hard-errors on a non-GitHub remote when tracked branches
exist (`crates/stacc/src/commands/operations.rs`), with `--offline` as the
escape hatch. Loud-beats-silent is correct, but `--offline` skips both the
upstream fetch and detection, so a GitLab user who wants "pull GitLab's main,
restack my stack, skip the forge" has no clean single command; they fetch by
hand, then run `stacc sync --offline`.

The people this hurts are real and varied: engineers whose forge is not GitHub,
and engineers in locked-down shops who could be on GitHub but cannot reach the
API, mint a token, or install a forge app. Today they hand-roll `git rebase`,
because Graphite's requirement to init against a repo and server is a non-starter
in those environments. That hand-rolled rebase is the bar slice 1 has to beat:
stacc already does the hard restack-after-merge math; the gap is a forge-less
path that does not punish them for lacking a usable forge.

## Key Decisions

- **Local floor first, forge trait second.** The cheapest win serves the largest
  slice of the "cannot use the forge" persona, and the floor is the universal
  substrate every forge later sits on, so it is never wasted work. It also
  de-risks the multi-forge bet by proving the agent-first local wedge before
  committing to multi-forge maintenance, and it mirrors git-spice's own build
  order (local stack management shipped first).

- **Heuristic detects, explicit command disposes.** A merged-out-of-band branch
  is found by a local heuristic (patch-id / is-ancestor equivalence) that only
  proposes; an explicit `stacc merged <branch>` (or `--assume-merged`) is what
  drops the branch and restacks its children. This layers the heuristic the user
  wanted on top of an explicit, deterministic signal, rather than choosing
  between them: detect automatically, dispose deliberately. `plans/algorithms.md`
  rejected diff/patch-id detection for the GitHub-API case, where the forge is
  authoritative; the forge-less floor reverses that deliberately: with no
  authoritative API, a propose-only local heuristic is strictly better than the
  hard-error it replaces (see `plans/algorithms.md`).

- **Never auto-drop non-interactively.** stacc is non-interactive by default
  (agent-first), so "confirm before dropping" has no human in an agent loop. The
  heuristic lists likely-merged branches and exits without dropping; only the
  explicit signal removes work. A false negative is safe (the branch lingers
  until signalled); a false positive never costs unmerged work.

- **Agent-first is the differentiator, not the forge matrix.** git-spice already
  ships GitHub, GitLab, and Bitbucket behind one abstraction, so matching the
  forge matrix is catch-up. The justification for forge-equal rests on
  agent-first execution (non-interactive, JSON, no forge app, single static
  binary), and that has to stay explicit so the work is not pure reimplementation.

## Actors

- A1. Developer, interactive. Drives stacc by hand and can answer a confirm
  prompt.
- A2. AI coding agent, non-interactive. Drives stacc in a loop with no human to
  confirm; relies on structured output and explicit signals.
- A3. Forge. GitHub today, GitLab and others in slice 2. In forge-less mode it is
  absent or unreachable, and stacc must not depend on it.

## Requirements

**Forge-less sync and restack**

- R1. `sync` supports a mode that fetches trunk from the remote and restacks the
  local stack while making zero forge API calls, distinct from the existing
  `--offline` which also skips the fetch.
- R2. On that path, a non-GitHub remote is not an error; restack proceeds on the
  fetched trunk. An unreachable but real GitHub remote is also non-fatal in local
  mode, but stacc still emits the merged-detection-skipped note (as `--offline`
  does today) rather than reporting a clean sync, so the STA-94 loud-beats-silent
  contract holds for the temporarily-unreachable case.
- R3. The existing `--offline` behavior (skip both fetch and forge, restack local
  refs only) remains available for the genuinely no-network case.

**Merge reconciliation**

- R4. A local heuristic detects branches whose changes already appear in trunk
  (for example patch-id or is-ancestor equivalence) and flags them as
  likely-merged.
- R5. Detection only proposes; an explicit `stacc merged <branch>` is what drops a
  branch from the stack and restacks its children onto trunk. The non-interactive
  `--assume-merged` signal acts only on an explicitly named branch whose tip the
  caller has verified is reachable from trunk, never on the heuristic's flagged set
  wholesale. Before executing the drop, `stacc merged` writes a structured disposal
  record (branch name, tip SHA, stack shape, and the heuristic evidence when
  detection triggered it) to a local log, so a wrong drop stays diagnosable and
  recoverable.
- R6. Slice 1 requires the non-interactive path: stacc lists flagged branches and
  exits without dropping. An interactive confirm-to-drop affordance is deferred
  polish, not slice 1.
- R7. A false negative is safe (a merged branch lingers until the user signals
  it). The no-silent-loss guarantee is scoped to explicit per-branch disposition:
  the heuristic never drops on its own, and `--assume-merged` drops only the named,
  trunk-verified branch, so an agent acting on a coincidental match (AE3) cannot
  silently lose unmerged work.

**Local mode and configuration**

- R8. A per-repo setting (in `.stacc.toml`, alongside `trunk` and `remote`)
  selects forge-less local mode so forge-touching commands default to the
  forge-less path without per-invocation flags. This requires extending the
  currently-closed config key set in `stacc-config` (today `trunk` / `remote` /
  `alias`); the exact key name and accepted values are deferred to planning, while
  its home (`.stacc.toml`) and effect (activates the forge-less path repo-wide) are
  fixed here.
- R9. In local mode, `submit` and `merge` stay GitHub-only in slice 1; on a
  non-GitHub or forge-less repo they are unavailable with a clear, forge-generic
  message that points the user to push their branch and open a change through their
  forge, not a crash. The message constructs no forge-specific URLs and performs no
  forge detection in slice 1.

**Forge-neutral surface**

- R10. Error messages, hints, and help text on the forge-less path do not assume
  GitHub; they name the forge generically or describe the local action.
- R11. Exact flag and mode names for skipping forge work are deferred to planning
  (see Outstanding Questions). Until the canonical name is resolved, no new
  per-flag forge-less override is added to any forge-touching command; today `sync`
  uses `--offline` and `log` uses `--no-status`, and the forge-less surface must
  not multiply that inconsistency.

## Key Flows

- F1. Forge-less sync and restack.
  - **Trigger:** A developer or agent runs `sync` in local mode on a GitLab or
    forge-less repo.
  - **Actors:** A1, A2
  - **Steps:** Fetch trunk from the remote; restack the stack bottom-up; report
    the result. No forge API call is made.
  - **Outcome:** The stack is rebased on the latest trunk with the forge never
    contacted.
  - **Covers:** R1, R2, R10

- F2. Reconciling an out-of-band merge.
  - **Trigger:** A stack branch was merged through the web UI, `glab`, or `gh`.
  - **Actors:** A1, A2
  - **Steps:** `sync`'s heuristic flags the branch as likely-merged and lists it
    without dropping (the slice-1 path for both A1 and A2; an interactive
    confirm-to-drop is deferred polish, per R6). The developer or agent then runs
    `stacc merged <branch>`, and stacc drops it and restacks its children onto
    trunk.
  - **Outcome:** The stack reflects the merge without any forge-side detection.
  - **Covers:** R4, R5, R6, R7

- F3. Hitting the forge boundary in local mode.
  - **Trigger:** A GitLab or forge-less user runs `submit` in slice 1.
  - **Actors:** A1, A2
  - **Steps:** stacc reports that submit is GitHub-only in this version and points
    to pushing and opening the change via the user's forge.
  - **Outcome:** A clear boundary message, not a crash or a silent no-op.
  - **Covers:** R9, R10

## Acceptance Examples

- AE1. **Covers R6, R7.** Given a stack with branch B whose net diff matches a
  commit on trunk, When `sync` runs non-interactively, Then B is listed as
  likely-merged, nothing is dropped, and the run exits successfully.
- AE2. **Covers R5, R6.** Given the same stack, When `stacc merged B` runs, Then B
  is removed from the stack and its children are restacked onto trunk.
- AE3. **Covers R7.** Given a branch B whose diff coincidentally resembles trunk
  but was not merged, When `sync` runs non-interactively, Then B is flagged but
  never dropped, so no unmerged work is lost.
- AE4. **Covers R1, R3.** Given a GitLab remote, When `sync` runs in local mode,
  Then trunk is fetched and the stack restacks with no forge API call; and When
  `sync --offline` runs, Then no fetch occurs and the restack uses local refs
  only.
- AE5. **Covers R2.** Given a repo with a GitLab remote and tracked branches, When
  `sync` runs in local mode, Then trunk is fetched successfully and the stack
  restacks with no error about the remote not being GitHub.

## Planned Direction: Forge-Equal Abstraction (Slice 2)

This is the direction slice 2 builds toward, captured so planning inherits the
shape. It is not slice 1 scope. Slice 2 is now specified in full at
`docs/brainstorms/2026-06-12-gitlab-forge-equal-requirements.md`; the sketch
below remains as the original framing.

- **A forge boundary with a shared capability set.** Each forge implements the
  set; selection is by remote-URL host with a config override; auth is per forge.
  This mirrors git-spice's proven model (a registry, remote-URL parsing, and
  per-forge tokens) in `../git-spice/internal/forge/forge.go`.
- **Capability interfaces, not lowest-common-denominator.** A core floor every
  forge meets (open a change, read its state, merge it), plus opt-in capabilities
  (labels, assignees, templates, draft, checks) a forge may lack, so weaker
  forges degrade honestly instead of breaking. Bitbucket is the proof case: it
  has no PR labels, assignees, or template enumeration.
- **Neutral change model internally, forge-specific labels in the UI.** A "change"
  internally, rendered as a GitHub PR or a GitLab MR at the surface.
- **GitLab is the first non-GitHub forge.**
- **Each forge can roll out incrementally on the local floor.** Open-changes
  first, merge and detect later, because the floor already reconciles merges
  regardless of forge. The seamless local floor is exactly what makes a partial
  forge a complete experience rather than a broken half.

## Success Criteria

- A GitLab or locked-down developer can run a full local stack loop (track,
  restack, forge-less `sync`, reconcile merges) with no per-command flag-spam and
  no GitHub assumptions surfaced.
- The forge-less path beats hand-rolled `git rebase` specifically on
  restack-after-merge: the user never manually re-parents children after a merge
  lands.
- No path silently drops unmerged work; every drop traces to an explicit signal
  or an interactive confirm.
- A planning agent can break slice 1 into implementation units without inventing
  behavior, scope, or reconciliation semantics.

## Scope Boundaries

**Deferred for later (slice 2 and beyond)**

- The forge trait plus GitLab end-to-end (open, merge, and detect MRs), the
  neutral change vocabulary migration, self-hosted base-URL support, and forges
  past GitLab.
- Replacing the heuristic with forge-authoritative merge detection on non-GitHub
  forges; that arrives per forge as slice 2 fills in.

**Outside what stacc will fix (platform-policy limits, named not solved)**

- Stacking requires push access to the shared repo; a developer who cannot push
  branches gets local stack management only, with no submit on any forge.
- Repos that dismiss approvals when a change's base branch moves are
  fundamentally incompatible with restacking; the only remedy is reconfiguring
  the repo.

## Dependencies / Assumptions

- The crate layering already supports this: `stacc-core` is forge-agnostic (pure
  git topology), `stacc-github` isolates every API call, and the `.stacc.toml`
  config surface (`crates/stacc-config/src/lib.rs`) is the natural home for a
  mode setting.
- The merge heuristic flags only the top-of-stack or single-branch squash via
  net-diff equivalence. When a downstack branch merges and advances trunk, the
  upstack branches no longer match their original patch and are not flagged; in a
  multi-branch stack that is the common path, not an edge case, so the explicit
  `stacc merged` signal, not the heuristic, is the primary reconciliation route for
  stacks. R7's "false negative is safe" therefore means upstack merged branches
  frequently linger until explicitly signalled.
- Only the is-ancestor portion of the heuristic mirrors git-spice's
  `findLocalMergedBranches`; git-spice does not catch squash or rebase merges and
  defers them to the user. The patch-id / net-diff squash detection is net-new
  surface with no reference implementation, so its false-positive behavior and
  confidence threshold carry implementation risk the git-spice precedent does not
  cover.
- R4 extends, rather than replaces, the existing `same_tree` tree_guard skip in
  `restack_forced` (STA-90, `crates/stacc-core/src/ops.rs`), which already proposes
  without dropping and points at `stacc delete`. Whether `stacc merged` replaces or
  supplements `stacc delete`, and how the tree-identical and net-diff signals
  reconcile, is deferred to planning.
- git-spice (sibling repo at `../git-spice/`) is the working reference for both
  the forge abstraction and the platform limits; it ships GitHub, GitLab, and
  Bitbucket behind one abstraction, which validates feasibility (an external
  contributor added GitLab behind the same abstraction with no business-logic
  change).

## Outstanding Questions

**Resolve before planning slice 2**

- Forge order after GitLab, and whether Bitbucket (degraded) and Gerrit (a
  changeset model, a genuinely different shape) are in or out.
- Self-hosted instances (GitHub Enterprise, GitLab self-managed, Bitbucket
  Server) in slice 2 or later. The locked-down-enterprise persona most likely
  runs self-hosted, so this may rank above Bitbucket Cloud rather than below it.
- User-facing vocabulary migration: keep forge-specific labels with neutral
  internals (git-spice's approach) versus neutralizing the existing GitHub "PR"
  surface, which touches current users.
- Whether the agent-first wedge needs anything forge-specific beyond parity (for
  example structured change-state output) to be a real differentiator rather than
  catch-up.

**Deferred to planning (slice 1)**

- Exact names for the forge-less `sync` mode and the reconcile command or flag
  (a `--no-detect` style flag versus a mode-implied path; `stacc merged` versus
  `sync --merged`).
- The precise heuristic (patch-id of the net diff, `git cherry`, is-ancestor, or
  a combination) and its confidence threshold.
- Whether `stacc merged` replaces or supplements the existing `stacc delete`
  disposition, and how the `same_tree` tree_guard skip and the net-diff signal
  reconcile.
- The exact `--assume-merged` contract (named branch plus trunk-reachable tip
  verification) and the disposal-record format.

## Sources / Research

- stacc forge boundary, `--offline`, and the non-GitHub hard-error logic:
  `crates/stacc/src/commands/operations.rs`, `crates/stacc/src/cli.rs`.
- stacc config surface (trunk, remote, aliases; the home for a mode setting):
  `crates/stacc-config/src/lib.rs`.
- Design intent ("a non-GitHub remote is a hard error in v1"; squash-merge
  detection is API-based, not diff-based): `plans/algorithms.md`, `plans/stacc.md`.
- git-spice forge abstraction (the Forge, Repository, and Change interfaces, the
  registry, per-forge auth, and the opt-in capability interfaces for Bitbucket):
  `../git-spice/internal/forge/forge.go`.
- git-spice platform limits (write-access requirement, squash-merge restack,
  Bitbucket degradations, approval dismissal on base change):
  https://abhinav.github.io/git-spice/guide/limits/
- git-spice forge-abstraction validation (external contributor added GitLab
  behind the same abstraction, no business-logic change):
  https://www.rippling.com/blog/boosting-workflow-velocity-gitspice
