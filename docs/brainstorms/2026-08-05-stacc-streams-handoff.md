# Handoff: stacc "streams" brainstorm (worktree-aware stacking)

> **Superseded as the resume point.** This handoff was charted into the wayfinder map
> [STA-145](https://linear.app/tiny-dog-tech/issue/STA-145/stacc-streams-worktree-aware-stacking)
> on 2026-08-05. Work the map's tickets rather than resuming `/b-brainstorm` from the
> "How to resume" section below. This document is preserved because STA-145 cites it for
> the six locked decisions and the codebase grounding each was verified against.

Date: 2026-08-05
Repo: `/Users/jilliankozyra/projects/stacc`, branch `main`, clean, at v0.4.1 (44ae735)
Session type: background job, `/b-brainstorm`. Read-only. No code changed, no worktree entered, no commits.

## Status

Mid-brainstorm, stopped at **Phase 2.5 (scoping synthesis), awaiting user confirmation**. No requirements
document has been written yet. Six design decisions are locked via blocking questions; everything else
is either a stated default or an open call-out listed below.

There is no artifact to hand off, which is why this doc exists rather than a `b-brainstorm` to `b-plan`
reset-and-resume. The decisions below live only in the conversation.

## Input material

- Prior session's investigation: `/var/folders/yj/pxgxy4hx5hxf_9yw5m_5slgw0000gn/T/handoff-worktree-investigation.md`
  Root-caused why every background session spawns a fresh worktree (`worktree.bgIsolation` default),
  documented the git worktree mental model, and rejected the bare-clone layout from
  `https://dev.to/metal3d/git-worktree-like-a-boss-2j1b`. Read it; this brainstorm assumes its findings.
- User's two goals, verbatim:
  1. Each Claude session gets a new worktree, possibly with a "root" branch, all session work stacks
     on that root, and at the end the root branch and worktree are deleted.
  2. `stacc log` should visualize parallel in-progress workstreams by worktree, showing each worktree's
     root relative to `main` so active worktrees and the active branches within them are both visible.
- Backlog anchor: `plans/stacc.md:116` already lists "Parallel agent / worktree support, locking on
  state ref for concurrent writes".

## Grounding verified against the codebase

Each of these was read, not assumed. They constrain the design.

| Fact | Evidence |
|---|---|
| `refs/stacc/data` is one ref per repo, and git refs are shared across worktrees, so `stacc log` in any worktree already sees every tracked branch everywhere | `crates/stacc-state/src/store.rs:10` |
| stacc already parses `git worktree list --porcelain` into `{path, branch}`; today it only feeds the `WorktreeConflict` guard | `crates/stacc-git/src/lib.rs:598`, `crates/stacc/src/error.rs:56` |
| `submit` skips a branch only under `--update-only`; every other tracked branch is pushed and PR'd. A zero-commit branch would hit GitHub's "No commits between" error | `crates/stacc/src/commands.rs:709` |
| Closing a PR is already a supported primitive | `crates/stacc-forge/src/lib.rs:98`, `crates/stacc-github/src/forge.rs:347` |
| **No reopen exists.** Only `close_pull_request` writing `{"state":"closed"}`. PR closure is one-way | `crates/stacc-github/src/lib.rs:273-280` |
| Dropped branch tips are preserved under `refs/stacc/dropped/<branch>-<tip>`, 50-drop retention, so `stacc undo` restores local commits | `crates/stacc-state/src/store.rs:15,24` |
| stacc already fetches PR review decision and check rollup per branch | `crates/stacc/src/commands/log.rs:734,752` |
| `RepoConfig.declined_tracking` records branches the user told stacc to leave alone | `crates/stacc-state/src/model.rs:15` |
| `stacc agent install` already ships skill content to harnesses and takes a `--harness` flag | `crates/stacc/src/commands/agent.rs`, `crates/stacc/assets/agent/skill-content.md` |

Naming collisions checked and ruled out: `lane` (log.rs already uses it for graph columns,
`crates/stacc/src/commands/log.rs:461-492`), `workspace` (cargo workspace), `track` (stacc's
tracked/untracked branch vocabulary).

## Locked decisions

Each was chosen by the user from a blocking multiple-choice question.

**1. stacc owns the worktree lifecycle end to end**, with the grouped visualization from the
"record origin" option. New commands, not just observation.

**2. The grouping is a session tag in stacc state, with no root branch.** Rejected: a synthetic empty
anchor branch at trunk (would force permanent special cases in `submit`, `merge`, `sync`, and `log`, and
every trunk advance would restack the whole session), and using the first real branch as the anchor
(merging it destroys the group). The tag adds one optional field on branch state plus a worktree table.
Zero changes to `submit`, `merge`, `sync`, `restack`.

**3. Named `stream`, not `worktree`.** `stacc stream new / list / end`. Rationale: `stacc worktree rm`
would read as a synonym for `git worktree remove`, which deletes only a directory, while this closes PRs
and deletes branches. The user's own phrasing was "parallel in progress workstreams". The user got
confused mid-session by tree vs branch vs grouping, which is direct evidence the name matters.

**4. `stream end` is a full destroy**: removes the worktree directory, drops the grouping, closes the
PRs, deletes the branches. The user explicitly rejected all three softer options (keep the grouping,
refuse until merged, drop the grouping but keep branches).

**5. A stream is closed under descent.** If branch A is in stream `pickups`, every branch based on A is
in `pickups`, transitively. Consequences the user accepted:
   - The tag is derived, not chosen. A branch's stream is its base's stream. Only trunk-based branches
     are unconstrained, and those take the stream of the worktree they were created in.
   - A stream may hold several independent stacks (several trunk-based roots). That is fine.
   - `move`, `reorder`, and `fold` become stream-affecting: re-deriving from the new base falls out of
     the same rule.
   - Teardown never has to reparent, refuse, or absorb dependents. The closure is the subtree.
   - **Cross-stream creation is refused.** Running `stacc create` inside stream T's worktree on top of a
     branch belonging to stream S is an error, with `--stream <name>` as the explicit escape.

**6. Two gates on the destroy, both blocking rather than forcing:**
   - **Review gate, tiered by human involvement.** Branches with no PR, and PRs that are draft, merged,
     or already closed, are destroyed silently. Anything with review activity (an approval, requested
     changes, a human review comment) blocks unless `--force`. Chosen over always-confirm specifically
     because always-confirm trains agents to pass `--force` reflexively, which defeats the gate.
   - **Repair pass, block on ambiguity.** Before destroying, untracked branches are repaired into the
     stream: one whose base is a stream branch is tracked and inherits by descent; one checked out in
     the stream's worktree and based on trunk is tracked as a new root. Both are warned about. Anything
     stacc cannot confidently place (previously declined tracking, detached HEAD work, a base rewritten
     outside stacc) blocks the same way an approved PR does.

**7. Harness integration: skill content plus an opt-in hook.** `stacc agent install` gains stream
instructions for agents, and `--hooks` proposes Claude Code `SessionStart` / `SessionEnd` entries that
run `stream new` and `stream end`. The hook is opt-in, never written without being asked for.

## Agreed output shape

`stacc log` renders each stream as a band under trunk. Real stack topology is preserved inside a band.

```
main
│
├─ ▸ stream: pickups   .claude/worktrees/pickups   HEAD: sta-141-render
│    ◯ jillian/sta-140-parsing      #140 open
│    └─◉ jillian/sta-141-render     #141 open
│
├─ ▸ stream: sen-auth  .claude/worktrees/sen-auth  HEAD: sta-150-auth
│    ◯ jillian/sta-150-auth         #150 approved
│    └─◯ jillian/sta-151-tokens     #151 draft
│
└─ ▸ (main checkout)
     ◯ jillian/sta-160-docs         #160 open
```

Blocked teardown, agent-facing:

```
$ stacc stream end pickups --no-interactive --json
{"op":"stream_end","stream":"pickups",
 "repaired":[{"branch":"jillian/sta-142-tmp","base":"jillian/sta-141-render",
              "reason":"untracked, inherited by descent"}],
 "blocked":[{"branch":"jillian/sta-141-render","number":141,"reason":"approved by @alex"},
            {"branch":"jillian/scratch","reason":"tracking previously declined"}],
 "schema_version":3}
```

## Open items

The user has **not** yet responded to the Phase 2.5 synthesis. These two call-outs were put to them and
are unanswered:

1. **The SessionEnd hook will usually block**, because real work ends with open reviewed PRs. In
   practice it cleans up throwaway sessions and no-ops on ones that matter. Safe, but it is not the
   literal "delete at the end" the user described for real work. Needs affirming or redesigning.
2. **Remote branch deletion.** Closing a PR leaves its branch on `origin`. The synthesis assumes
   `stream end` deletes remote branches too. Cheap to change now, awkward after planning.

Defaults the agent adopted without asking. Surface them if the user reopens scope:

- The main checkout renders as an implicit unnamed stream in `stacc log`.
- Bands collapse when only one stream exists, so single-worktree users see no change to `stacc log`.
- Stream name derives from the worktree directory name.
- `$CLAUDE_SESSION_ID` was used as the stream name in the hook preview. It is unstable and ugly;
  deriving from the worktree name is likely better and was not decided.

Explicitly out of scope, per the synthesis:

- The harness's own worktree settings (`bgIsolation`, `baseRef`, `symlinkDirectories`) and worktree
  location on disk. That stays user config, per the prior investigation doc.
- The bare-clone layout from the dev.to article, already rejected in the prior session.
- The three outstanding config actions from the prior session (gitignore `.claude/worktrees/` in four
  repos, `symlinkDirectories` for large `node_modules`, the `integralhq` `bgIsolation: "none"`
  question). Unrelated to this feature, still unapplied.

## How to resume

The user's next message is a confirmation, a revision, or a redirect on the Phase 2.5 synthesis.

- **If they confirm:** write the requirements document to
  `docs/brainstorms/2026-08-05-stacc-streams-requirements.md`, following
  `~/.claude/skills/b-brainstorm/references/brainstorm-sections.md` and
  `references/markdown-rendering.md`. Output format is markdown (no `.bindle/config.local.yaml`
  exists in this repo, so the default applies). Then offer the Phase 4 handoff menu from
  `references/handoff.md`.
- **If they revise:** integrate, re-present the revised synthesis, and wait for explicit confirmation.
  A revision is not a confirmation.
- Existing brainstorm docs in `docs/brainstorms/` show the house format; the most recent is
  `2026-06-20-sta128-agent-context-installer-requirements.md`.

Per `AGENTS.md`, this repo dogfoods stacc for its own git workflow. Any implementation work goes on a
branch named `jillian/sta-<n>-<slug>` matching a Linear STA ticket, created with `stacc create`, and
submitted with `stacc submit --no-interactive --json`. Never commit to `main`. No Linear ticket has been
filed for this feature yet.

## Suggested skills

- **`b-brainstorm`** to resume. It is the skill that was running; re-invoking it with this doc as
  context picks up at Phase 2.5.
- **`b-plan`** once the requirements doc lands. This feature touches state model, `log` rendering, three
  new commands, `create` validation, `move` / `reorder` / `fold` re-derivation, and `agent install`, so
  it needs a real implementation plan rather than going straight to work.
- **`b-doc-review`** on the requirements doc before planning. The closed-under-descent invariant and the
  two-gate destroy are the kind of decisions worth adversarial review; `b-scope-guardian-reviewer` and
  `b-feasibility-reviewer` are the relevant personas.
- **`b-linear-cli`** to file the STA ticket and get the branch name, since the branch must match the
  Linear branch for PR auto-linking.
- **`stacc`** for driving the branch stack during implementation.

Do not reach for `b-work` yet. Nothing is planned.

## State to be aware of

- Nothing was committed, pushed, or edited. Working tree clean on `main`.
- No `EnterWorktree` call was made this session.
- The `AskUserQuestion` tool was rejected three times mid-session, each time because the user wanted the
  options explained further, not because they disagreed. Expect to justify option trade-offs concretely
  rather than assuming shorthand lands.
