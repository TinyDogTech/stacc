# stacc — core algorithms

Created: 2026-05-20
Status: draft
Authors: Jillian

# Objective

Pin down the three hard algorithms behind `stacc sync` — squash-merge detection, restack, and conflict resume — before we scaffold the Rust workspace. Each is modeled on git-spice's Go implementation, with the stacc-specific deltas called out. File:line citations point at `../../git-spice/` so we can read the reference while implementing.

# Background

The plan (`stacc.md`) names three hard problems and says "reference implementations in git-spice." We read the git-spice source to turn that into a concrete spec. Two findings changed our assumptions:

- **Squash-merge detection is API-based, not diff-based.** We had assumed we'd compare a branch's patch-id or diff against trunk to spot a squash. git-spice doesn't. It asks the forge whether the PR is merged. That is simpler and more reliable, and it works for us because v1 is GitHub-only.
- **Conflict-context fetching has no reference.** git-spice's three "hard problems" are really detection, restack, and resume. It does nothing to explain *why* a conflict happened — it just tells a human to fix it. The context file in our plan is genuinely new surface; we cannot copy it.

The coupling matters: one `stacc sync` detects squash-merges, restacks dependents, and on conflict emits machine-readable context and exits resumably. The three sections below are one pipeline, not three features.

# Overview

`stacc sync` runs in this order:

1. Fetch trunk and the state ref from the remote.
2. **Detect** which tracked branches the forge reports as merged; record what merged below their children, delete them, re-parent children onto trunk.
3. **Restack** the remaining branches bottom-up with `git rebase --onto`, updating each branch's recorded base hash as we go.
4. On a rebase conflict: write the **conflict context** file, append a **continuation** to the state ref, print a structured error, and exit non-zero.
5. `stacc sync --continue` finishes the rebase and replays the remaining work idempotently.

State lives in one git ref (`refs/stacc/`), mirroring git-spice's `refs/spice/data`. It travels with the repo via push/fetch and needs no server.

# Detailed Design

## Squash-merge detection

git-spice has two paths (`../../git-spice/internal/handler/sync/handler.go`):

- **Forge API path** (`findForgeFinishedBranches`, lines 400–743). Bulk-query PR state via GitHub GraphQL (`ChangesStates`). GitHub returns only `Open` / `Closed` / `Merged` with no merge-method field — squash, rebase, and normal merges all report `Merged`. Detection is just `state == Merged`. For PRs git-spice didn't submit itself, it matches by branch name and compares the PR's `HeadHash` to the local branch HEAD.
- **Local fallback** (`findLocalMergedBranches`, lines 374–398). `git merge-base --is-ancestor <branch> <trunk>`. The code comment admits this only catches normal merges and fast-forwards — squash and rebase merges "will need to be handled manually by the user."

After detection, git-spice does the bookkeeping: it propagates a `MergedDownstack` record up to the merged branch's children (so they remember what merged below them), deletes the branch, then re-parents children onto trunk in the next restack pass.

**stacc delta:** v1 is GitHub-only, so we use the API path exclusively. Query PR state, treat `Merged` as done. No patch-id heuristics. We keep the `MergedDownstack` bookkeeping — children need to know their grandparent's PR merged so we can rebase them onto trunk and so conflict context can still resolve a merged base to its PR.

## Restack

The single-branch operation (`../../git-spice/internal/spice/restack.go:25-111`):

```
git rebase --onto <new-base-hash> <old-base-hash> <branch> --autostash --quiet
```

- **`old-base-hash`** is the base hash recorded in the state ref last time. **`new-base-hash`** is the base branch's current tip. The range between them is exactly the commits to replay.
- **Fork-point recovery.** If the recorded old-base is no longer an ancestor of the branch (base was amended or force-pushed), fall back to `git merge-base --fork-point` to find where the branch actually diverged. This is the safety valve against externally-mutated bases.
- **Idempotency.** Before rebasing, `VerifyRestacked` checks `IsAncestor(baseHash, branchHead)`. If the branch already sits on its base, return `ErrAlreadyRestacked` and skip. Safe to re-run.
- **Ordering** (`../../git-spice/internal/handler/restack/handler.go:99-238`). Build the list as reversed downstack (bottom-up) + the branch + upstack BFS (already top-down). Bases always restack before dependents.
- **State update.** After each successful rebase, upsert the new base hash into the state ref and commit it, so the next operation has a fresh baseline.

**stacc delta:** mirror this directly. The branch graph comes from our state ref. We shell out to `git rebase --onto` from `stacc-git`; the topo-sort and the per-branch state write live in `stacc-core`.

## State storage

One ref: git-spice uses `refs/spice/data` (`../../git-spice/repo_init.go:142`). It is a normal git commit whose tree holds JSON blobs:

| path | content |
| --- | --- |
| `repo` | trunk + remote |
| `branches/<name>` | base name + hash, upstream, PR metadata, merged-downstack |
| `version` | schema version |
| `rebase-continue` | continuation queue |

Writes use plumbing: `hash-object` → `update-tree` → `commit-tree` → `update-ref`, with a 5× retry loop on the ref compare-and-swap for concurrent writers. The ref travels with the repo via push/fetch like any other ref.

**stacc delta:** same model under `refs/stacc/`. The retry-on-CAS loop is what later makes parallel-agent support (v2) possible, so we build it in from the start even though v1 is single-writer.

## Conflict resume

git-spice does not checkpoint a loop position. On conflict (`../../git-spice/internal/spice/rebase.go:92-163`) it:

1. Detects the interrupted rebase by reading `.git/rebase-merge/head-name`, wraps it as a `RebaseInterruptError`.
2. Appends a continuation to the state ref — `{command: ["upstack","restack"], branch: "X"}`, the command to re-run, not a position.
3. Prints "resolve and run `gs rebase continue`" and exits.

On `rebase continue`: finish the git rebase, drain the continuation queue, re-run each command. Because each restack command is idempotent, re-running the whole operation skips the already-done branches and resumes at the conflict. If it conflicts again, the remainder is re-appended. `rebase abort` drains the queue and runs `git rebase --abort`.

**stacc delta:** steal the "re-run idempotent work" trick, but change the surface for agents. Instead of `--continue` re-parsing a CLI command, store the operation plus the remaining branch list in the state ref and replay it. Same principle, machine-friendly framing.

## Conflict context (stacc-only)

No git-spice reference exists. We insert this into the same exit path as conflict resume. When a `--onto` rebase returns a conflict we already hold the branch being rebased, its base, and the new-base tip. We:

1. Map the new-base tip to its PR via the PR number stored in `branches/<name>`.
2. Fetch the PR title, body, and relevant diff hunks from the GitHub API.
3. Write `.git/stacc-conflict-context.json` and exit with a structured error.

The agent reads the context, resolves the conflict, and runs `stacc sync --continue`. This is an insertion into the existing flow, not a separate subsystem.

# Caveats

- **GitHub-only, v1.** The local `--is-ancestor` fallback is out of scope; without the API we cannot detect squash-merges, and we are not shipping a degraded path. A non-GitHub remote is a hard error in v1.
- **Conflict context is best-effort.** If the API call fails or the base tip maps to no known PR, we still write the file with whatever we have and exit — we never block the resume on a failed fetch.
- **Single writer in v1.** The CAS retry loop exists, but real parallel-agent locking on the state ref is v2 (`stacc.md` "Later").
- Citations point at git-spice as read on 2026-05-20; line numbers drift as that repo updates.

# Sources

- `../../git-spice/` — Go reference implementation. Key files cited inline.
- `stacc.md` — master plan.
