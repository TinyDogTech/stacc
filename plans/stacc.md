# stacc

A stacked diff CLI tool designed for AI coding agents, written in Rust.

## Background

Explored the landscape of stacked diff tooling (Graphite, git-spice, spr, Sapling, jj) and
identified a gap: every existing tool was designed for human operators. AI coding agents
(Claude Code, Cursor, Aider) are increasingly the primary authors of code, and none of the
current tools accommodate them well.

## Name

**`stacc`** — a deliberate misspelling of "stack" in the internet-slang tradition. Clean
namespace: not in Homebrew, not on crates.io, not on npm (only a trivial LIFO stack package
that won't conflict). Binary command: `stacc`.

Rejected names and why:
- `stax` — taken on crates.io as a Rust stacked PR tool in the same domain
- `flume` — Apache Flume (Homebrew), well-known Rust channel crate (111M+ downloads)
- `braid` — git vendor branch tool in Homebrew
- `stak` — Scheme interpreter on crates.io
- `loom` — well-known video recording brand (Atlassian)
- `st` — statistics CLI in Homebrew, conflicts with schemathesis

## Design decisions

### Language & aesthetic

Rust. Inspired by Astral's tooling (uv, ruff):
- Single binary, `curl | sh` install + Homebrew formula
- Rich error messages via `miette`
- Multiple output formats: `--format json|pretty` on all commands, `--color auto/always/never`
- Zero-config defaults (auto-detect trunk, auto-generate PR descriptions)
- Cargo workspace, flat crate structure

### Model

**Branch-per-PR** (not commit-per-PR like spr). Each branch in the stack becomes one PR.
Branches are named and managed by the tool; the stack structure is tracked in state.

### State storage

Hidden git ref: `refs/stacc/` — stores JSON trees as commits (same pattern as git-spice's
`refs/spice/`). State travels with the repo via push/pull. No files in the working tree.
No external server required.

### Forge support

**v1: GitHub only.** Auth via PAT or OAuth device flow (like `gh auth login`). No GitHub App —
all code runs locally. GitHub API calls for: creating/updating PRs, fetching upstream PR
context during conflict resolution, detecting squash-merges.

### Agent-first design

The core design constraint: every operation must work without a human in the loop.

- Non-interactive by default — fail with a structured error rather than open a prompt
- `--format json` on all commands for machine-readable output
- `--description <file-or-string>` on submit so agents can pass PR descriptions
- Conflict context: on rebase conflict, fetch the upstream PR (title, body, relevant diff)
  from GitHub API and write to `.git/stacc-conflict-context.json`, then exit with a
  structured error. Agent reads context, resolves conflict, runs `stacc sync --continue`.

### PR descriptions

Written by the agent. The tool outputs per-branch context (commit messages, diffs, base
branch) for the agent to summarize. Agent passes the result back via `--description`. Default
fallback: template populated from commit messages.

## v1 scope

Minimum viable. No MCP server, no TUI, no parallel agent support.

| Command | Description |
|---------|-------------|
| `stacc init` | Detect/confirm trunk + remote, initialize state ref |
| `stacc track` | Start tracking current branch as part of a stack |
| `stacc submit` | Push branches, create/update PRs on GitHub |
| `stacc sync` | Pull upstream changes, rebase stack, handle squash-merges |
| `stacc sync --continue` | Resume after manual conflict resolution |
| `stacc log` | Print stack state (`--format json\|pretty`) |
| `stacc status` | Current branch position in stack + PR status |

Global flags: `--format json|pretty`, `--no-color`, `--no-interactive`

## Crate structure (planned)

```
stacc/
├── Cargo.toml               # workspace
└── crates/
    ├── stacc/               # binary entry point (CLI parsing, dispatch)
    ├── stacc-git/           # git operations (shell out to git)
    ├── stacc-github/        # GitHub API client
    ├── stacc-core/          # stack operations: submit, sync, rebase
    ├── stacc-state/         # state storage in refs/stacc/
    └── stacc-config/        # configuration parsing
```

## Key hard problems (reference implementations in git-spice)

1. **Squash-merge detection** — after sync, query GitHub API to detect if any stack branches
   were squash-merged; compare squashed commit to original to avoid phantom rebase conflicts.
2. **Rebase-after-merge** — when upstream branches merge, automatically rebase dependents
   onto the new trunk.
3. **Conflict context fetching** — on rebase conflict, identify the upstream commit/PR that
   caused it, fetch its PR body and relevant diff hunks from GitHub API.

See [`algorithms.md`](./algorithms.md) for the concrete spec of all three (squash-merge
detection, restack, state ref, conflict resume + context) with git-spice file:line citations
and the stacc-specific deltas. Reference implementations in Go live in `../../git-spice/`.

## Later (v2+)

- MCP server (`stacc mcp serve`) — exposes tools for agent-in-loop conflict resolution
- Parallel agent / worktree support — locking on state ref for concurrent writes
- Additional forges (GitLab, Bitbucket)
- AGENTS.md / CLAUDE.md shipped with the tool for repo-level agent guidance
