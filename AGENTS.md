# stacc (AGENTS.md)

Project conventions for coding agents. Inherits global defaults from
`~/.pi/agent/AGENTS.md` (plain `git` + `gh`), but overrides the git workflow:
this repo dogfoods **stacc itself** for branching, committing, submitting, and
merging (see Workflow).

stacc is a stacked-diff CLI for AI coding agents, Rust, Astral/uv aesthetic
(single binary, miette errors, JSON output, zero-config). Model: branch-per-PR;
v1 is git + GitHub only (PAT/OAuth device flow, fully local); state in a hidden
`refs/stacc/` git ref; design inspired by git-spice (reimplementation, not a
fork). Linear: team **STA** (workspace tiny-dog-tech), branches
`jillian/sta-<n>-<slug>`. MVP (STA-1..13) is merged; v1.1 backlog is STA-14..18.
Full decisions in `plans/stacc.md`; deeper notes in the Claude project memory at
`~/.claude/projects/-Users-jilliankozyra-projects-stacked-diff-tooling/memory/`.

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets    # also the citation source for "idiomatic" claims
```

## Explaining Rust

This project doubles as a way to learn Rust. When you make changes:

- **Explain in your response, not in code comments.** Walk through what changed
  and why in the terminal. Do not embed teaching in code comments, keep comments
  minimal and idiomatic (only when behavior isn't obvious from the code itself).
- **Show the code you're explaining.** Quote the specific lines and explain them
  inline in your response.
- **Justify Rust implementation choices.** When you pick a type, trait, pattern,
  or crate, say why it's the idiomatic / Rust-standard choice over the
  alternatives.
- **Cite sources for "idiomatic" claims.** Before calling something idiomatic,
  name a specific source: a clippy lint (e.g. `clippy::redundant_closure_for_method_calls`),
  a Rust Book chapter, an stdlib example, or an API Guidelines entry. If you
  can't cite one, say "I'd prefer" or "I'd lean toward" instead, taste, not
  consensus. When in doubt, run `cargo clippy --workspace --all-targets` and let
  the lint output be the citation.

## Workflow

> Drive the stacked-diff workflow with **stacc itself** (the binary this repo
> builds): the repo dogfoods its own tool. Reach for plain `git` + `gh` only
> for the primitives stacc has no command for. Never use `gt` (Graphite is
> retired; stacc is its replacement, see `plans/stacc.md`).

- **Branch + commit:** stage with `git add`, then `stacc create
  jillian/sta-<n>-<slug> -m "<type>(<scope>): [STA-N] <desc>"` to create the
  branch, commit the staged changes, and track it in one step. Fold follow-up
  edits in with `stacc modify` (amends the tip) or `stacc modify --commit` (adds
  a commit). The branch name must match the Linear branch so the PR auto-links.
- **Submit:** `stacc submit --no-interactive --json` pushes the current branch's
  full downstack and creates or updates its PRs. Idempotent: re-submitting an
  unchanged branch is a no-op on the remote.
- **Merge:** `stacc merge --no-interactive --json` squash-merges the ready PRs
  from the trunk up, retargets children, and syncs. Squash is enforced.
- **Sync:** `stacc sync --no-interactive --json` after merges (pulls trunk,
  detects merged branches, restacks). Replaces `git fetch && git rebase
  origin/main`.
- **Navigate / inspect:** `stacc checkout <branch>`, `stacc up`/`down`/`top`/
  `bottom`, `stacc log`, `stacc info`, `stacc pr`, `stacc status`.
- **Still plain `git` + `gh`:** `git add` (staging), editing files during a
  restack conflict then `stacc continue` (or `stacc abort` to bail), `gh pr
  checks <N>` (no stacc equivalent), and `gh auth`.
- One branch per Linear ticket; never commit to `main`. Conventional-commit
  format for commit messages and PR titles.

## Project

- Design: `plans/stacc.md`. Core algorithms: `plans/algorithms.md`.
- Go reference implementation (git-spice) lives at `../git-spice/`, a sibling of
  this repo.

## Using stacc as an agent

### 1. Invocation rule

Every command: `stacc <cmd> --no-interactive --json`. One command per Bash
call. Read stdout directly -- no Python, no `jq`, no pipeline post-processing.
JSON output is one compact line. Nulls and empty arrays are stripped automatically;
absent keys mean null/empty.

### 2. Error channel

With `--json`: success output and error JSON both go to stdout; stderr is
silent. An empty stdout means the process exited before writing (panic or misuse).
Never omit `--json` for programmatic consumption -- without it, errors go to
stderr and stdout is empty on failure.

### 3. stacc vs `gh` substitution table

| Want to... | Use stacc, not gh |
|---|---|
| Create or update PRs (current branch + downstack) | `stacc submit --no-interactive --json` |
| List stack PR status | `stacc log --no-interactive --json` |
| Merge ready PRs (trunk-up, squash) | `stacc merge --no-interactive --json` |
| View current branch PR number and state | `stacc pr --no-interactive --json` |
| Per-branch detail (base, diffstat, PR URL) | `stacc info --no-interactive --json` |
| Navigate stack | `stacc up / down / top / bottom / checkout <branch> --no-interactive --json` |
| Sync local state with merged PRs on remote | `stacc sync --no-interactive --json` |
| PR check results for a single PR | No stacc equivalent; use `gh pr checks <number>` |

`stacc submit` always pushes the current branch's full downstack (all ancestors
up to trunk). This is idempotent -- pushing an unchanged branch is a no-op on the
remote.

### 4. JSON output shapes (key commands)

`stacc merge`:
```
{"op":"merge",
 "merged":[{"branch":"...","number":N,"sha":"...","out_of_band":false}],
 "stopped_at":null | {"kind":"...","branch":"...","number":N,"readiness":"...","rejection":{...},"retryable":bool},
 "trunk_protected":bool,
 "synced":{"dropped":[...],"reparented":[{"branch":"...","base":"..."}],"restacked":[...]},
 "cleaned":[...],"cleanup_skipped":[...],"schema_version":2}
```
`stopped_at.retryable` is nested under `stopped_at`, not top-level.

`stacc submit`:
```
{"submitted":[{"status":"created"|"updated","branch":"...","number":N,"url":"https://..."}],
 "skipped":[],"schema_version":2}
```

`stacc log` (short or bare):
```
{"trunk":"main","stack":[{"name":"...","base":"...","change":{...},"commit":{...},...}],"schema_version":2}
```
`stack` is a recursive tree: each node may have a `children` array of the same
shape. Leaf nodes have no `children` key (stripped by `print_compact`; do not
expect `"children":[]`). `stacc log long --json` returns only
`{"trunk":"...","form":"long","schema_version":N}` -- it does not return tree
data. Use `stacc log`, `stacc log short`, or bare `stacc log` for tree data.

`stacc sync`:
```
{"op":"sync","merged":[...],"pruned":[],"adopted":[],"reparented":[{"branch":"...","base":"..."}],
 "restacked":[],"cleaned":[],"cleanup_skipped":[],"detection_skipped":false,
 "likely_merged":[...],"schema_version":2}
```

`stacc info` (per-branch detail, includes PR URL):
```
{"branch":"...","parent":"...","children":[...],"needs_restack":bool,"commits":N,
 "commit":{"sha":"...","subject":"...","age":"..."},
 "diffstat":{"files":N,"insertions":N,"deletions":N},
 "change":{"number":N,"url":"https://..."},"schema_version":2}
```

`stacc up / down / top / bottom / checkout`:
```
{"op":"top"|"up"|"down"|"bottom"|"checkout","branch":"...","moved":bool,"schema_version":N}
```

### 5. Conflict resolution flow

When a restack conflict occurs mid-chain, stacc writes context to
`<git-dir>/stacc-conflict-context.json` (in a worktree, `<git-dir>` is not
`.git/`; use `git rev-parse --git-dir` for the robust path):

```
{"branch":"...","base":"...","conflicted_files":[...],"base_pr":{"number":N,"title":"...","body":"..."}}
```

Resolution steps:
1. Read the context file for the conflicting branch and its base PR.
2. Resolve conflicts in the listed files with your editor or git tooling.
3. Stage resolved files and run `stacc continue`.
4. Re-run `stacc merge --no-interactive --json` to finish the remaining
   chain. Do not re-run merge before `stacc continue`.

To abandon the in-progress restack: `stacc abort --no-interactive --json`.

### 6. Three-way merge stop states

- `stopped_at: null` -- clean finish; all PRs merged.
- `stopped_at.retryable: true` -- CI pending on the next branch in chain; wait
  for CI, then re-run `stacc merge`. Or pass `--watch` on the original call to
  poll automatically.
- `stopped_at.retryable: false` -- hard block (needs approval, merge conflict on
  GitHub, protected branch, etc.); stop and inform the user.
- **Conflict during restack mid-chain (distinct from the above):** if a restack
  conflict occurs after PRs have already merged, JSON output contains both a
  partial `merged` array and an error `{"type":"conflict",...}`. Follow the
  conflict resolution flow in section 5. Do not re-run `stacc merge` before
  `stacc continue`.

### 7. Edge cases

- `stacc checkout --no-interactive` without an explicit branch name returns
  `{"type":"usage",...}`, not an interactive prompt. Always pass a branch name
  or `--trunk`.
- `stacc top --no-interactive` on a stack fork returns
  `{"type":"ambiguous","choices":[...],...}`. Pick from `choices` and run
  `stacc checkout <choice> --no-interactive --json`.
- `stacc log long --json` returns only a stub; use `stacc log` or
  `stacc log short` for tree data.
- `moved: false` in navigation output means already at the destination. It is
  not an error.
- `stacc submit` pushes the full downstack, not just the current branch.
- `st` is a built-in alias for `stacc`. Both work; use `stacc` in agent contexts
  for clarity.

### 8. Binary not found

If `stacc` is not in PATH, stop immediately. Do not fall back to `gh` or attempt
compound pipelines. Tell the user: "`stacc` is not installed. Install with
`cargo install stacc` or download from the GitHub releases page."
