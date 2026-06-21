---
title: "feat: [STA-128] harness-agnostic agent context installer"
type: feat
status: draft
date: 2026-06-20
issues: [STA-128]
---

# feat: [STA-128] Harness-agnostic agent context installer

## Problem

STA-124 shipped a Claude Code skill (`~/.claude/commands/stacc.md`) and `AGENTS.md`
agent guide. These help agents operating in Claude Code but do nothing for agents
operating in any other harness: pi, Cursor, Codex, Gemini CLI, OpenCode, and the
~40 other clients now in the agentskills.io ecosystem.

The result is that an agent using pi or Cursor in a foreign repo falls back to
`gh pr create` for stack operations, reintroducing compound pipelines and permission
prompts. There is also no mechanism to install this context automatically after
upgrading stacc; users must track file updates manually.

A secondary problem: the existing `~/.claude/commands/stacc.md` shipped with a
stale flag (`--format json` instead of `--json`). There is no automated path to
fix it.

## What we are building

A `stacc agent install` subcommand that installs agent context files using the
[Agent Skills standard](https://agentskills.io/specification) (cross-client
`~/.agents/skills/` convention defined by Anthropic) plus Claude Code-specific
extras. The command is interactive by default and idempotent; running it again
after a stacc upgrade brings installed files up to date.

`stacc init` gains a prompted step at the end of its interactive flow that offers
to run agent context installation for the current session, so users get the context
installed during normal onboarding without needing to discover a separate command.

## Checklist items

When run interactively with no flags, `stacc agent install` presents a checklist
with all items unchecked. The user selects what to install. Two items:

1. **Universal skill** (`~/.agents/skills/stacc/SKILL.md`): covers all
   agentskills.io-compatible clients simultaneously: Gemini CLI, OpenCode, Codex,
   Cursor, pi, Goose, Firebender, Warp, Zed, and ~40 others. A single file;
   no per-harness selection needed.

2. **Claude Code slash command** (`~/.claude/commands/stacc.md`): the
   user-invocable `/stacc` skill for Claude Code. Separate from the universal
   skill because Claude Code reads slash commands from `~/.claude/commands/`, not
   from `~/.agents/skills/`. Installing this also fixes the `--format json` bug
   in any existing file at that path.

Non-interactive equivalent: `stacc agent install --harness universal`,
`--harness claude-command`, or `--harness all`. Multiple `--harness` flags may
be combined.

## Source of truth

One canonical skill content file, embedded in the binary via `include_str!()`
from a single source under `assets/agent/skill-content.md`. Every install target
(`~/.agents/skills/stacc/SKILL.md`, `~/.claude/commands/stacc.md`) receives
identical body content. Only the thin wrapper differs per format: SKILL.md gets
agentskills.io-standard frontmatter; the Claude Code slash command gets a
`# stacc agent guide` heading with no frontmatter. No separate content is
maintained per harness.

Re-running `stacc agent install` after `brew upgrade stacc` overwrites installed
files with the current version's content.

## Acceptance criteria

- `stacc agent install` exists as a subcommand; `--help` describes the three
  checklist items.
- Running with no flags in a terminal presents the interactive checklist, nothing
  pre-selected.
- Running with `--no-interactive --harness all` installs all three items silently.
- Each installed item is idempotent: re-running overwrites with the current
  version's content, no duplicate entries, no errors.
- The installed `~/.agents/skills/stacc/SKILL.md` uses `--json` (not `--format
  json`).
- The installed `~/.claude/commands/stacc.md` uses `--json` and fixes any
  existing file at that path.
- `stacc init` in interactive mode offers to run agent context installation as a
  final step and skips it cleanly if declined.
- The command prints what it installed (paths written or updated) upon completion.

## Scope boundaries

### In scope

- `stacc agent install` subcommand with the three items above.
- `stacc init` integration (interactive prompt only; `--no-interactive` init skips
  agent context installation).
- Idempotent overwrite of all installed files.
- Fixing the `--format json` -> `--json` bug in any existing Claude Code skill
  file on install.

### Deferred

- `stacc agent status`: show what is installed and whether files are current.
  Natural follow-on but not MVP.
- `stacc agent uninstall`: remove installed files. Deferred; users can delete
  manually.
- `gh` guard hooks: skipped. No cross-client hook mechanism exists in the
  agentskills.io spec, and the value does not justify Claude Code-only
  implementation.
- Codex, Cursor native-path extras beyond the universal install: the universal
  `~/.agents/skills/stacc/` covers them; no additional files needed.

### Out of scope

- MCP server for stacc (closed decision).
- Changes to `gh` behavior.
- Retraining or fine-tuning models.
- Per-repo installation (all installs are user-global).

## Key decisions

- **Universal path over per-harness files**: writing once to `~/.agents/skills/stacc/`
  covers the entire agentskills.io ecosystem rather than maintaining N copies.
  Claude Code is the only harness requiring a separate file, and only because its
  slash-command UX reads from `~/.claude/commands/` rather than the universal path.
- **No guard hook**: no cross-client hook mechanism exists in the agentskills.io
  spec; Claude Code-only coverage does not justify the complexity.
- **Binary-embedded templates**: content embedded via `include_str!()` eliminates
  version skew between the installed file and the binary's documented behavior.
- **None pre-selected in checklist**: explicit opt-in prevents silent writes to
  `~/.claude/settings.json` (which modifies user-global Claude Code behavior) and
  `~/.agents/skills/` without the user understanding what is happening.
- **Single content source**: one `include_str!()` template drives all install
  targets. Wrappers (frontmatter, heading) differ per format; body content is
  identical. No per-harness content maintenance.
- **`stacc agent` namespace**: leaves room for `stacc agent status` and
  `stacc agent uninstall` as natural follow-ons without requiring a rename.

## Assumptions

- `~/.agents/skills/` is the user-global path for the agentskills.io spec; project-local
  `.agents/skills/` is also valid per the spec but user-global is the right scope
  for a tool-wide skill.
- pi reads from `~/.pi/agent/skills/<skill>/SKILL.md` natively and also honors
  `~/.agents/skills/` as the universal path; the universal install covers pi without
  a separate file.
- Claude Code's `preToolUse` hook in `~/.claude/settings.json` can inspect the
  Bash command string and emit output visible to the agent.

## Outstanding questions

- **Q1: SKILL.md content for universal vs Claude Code slash command.** Resolved:
  single source of truth. Both files get identical body content from one
  canonical template; wrappers differ by format only.
- **Q2, Q3: Guard hook questions.** Moot; guard hook skipped.
