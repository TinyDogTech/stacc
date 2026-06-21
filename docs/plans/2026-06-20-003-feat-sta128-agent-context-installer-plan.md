---
title: "feat(stacc): [STA-128] harness-agnostic agent context installer"
type: feat
status: ready
date: 2026-06-20
issues: [STA-128]
origin: docs/brainstorms/2026-06-20-sta128-agent-context-installer-requirements.md
---

# feat: [STA-128] Harness-agnostic agent context installer

## Problem frame

STA-124 shipped `~/.claude/commands/stacc.md` (Claude Code skill) and `AGENTS.md`.
Neither helps agents in any other harness. Agents in pi, Cursor, Codex, Gemini CLI fall
back to `gh pr create`, reintroducing compound pipelines and permission prompts.
Additionally, the STA-124 skill file shipped with `--format json` (wrong flag; actual
flag is `--json`). No automated path to fix it exists.

(see origin: `docs/brainstorms/2026-06-20-sta128-agent-context-installer-requirements.md`)

## Scope

### In scope

- `stacc agent install` subcommand: installs agent context files, interactive checklist
  with two items, none pre-selected.
- Two install targets:
  1. Universal skill (`~/.agents/skills/stacc/SKILL.md`): covers all agentskills.io-compatible
     clients (Gemini CLI, OpenCode, Codex, Cursor, pi, Goose, Firebender, Warp, Zed, ~40 others).
  2. Claude Code slash command (`~/.claude/commands/stacc.md`): user-invocable `/stacc`
     skill; separate because Claude Code reads from `~/.claude/commands/`, not `~/.agents/skills/`.
- Idempotent: re-running overwrites installed files with current binary's content.
- `stacc init` integration: interactive-only prompt at the end of init flow.
- Single source of truth: one canonical template embedded via `include_str!()`.
- Fixes `--format json` bug in any existing `~/.claude/commands/stacc.md` on install.

### Out of scope (see origin)

`stacc agent status`, `stacc agent uninstall`, guard hooks, MCP server, per-repo install.

## Key decisions

**Universal path over per-harness files.** `~/.agents/skills/stacc/SKILL.md` covers the
entire agentskills.io ecosystem; only Claude Code needs a second file (slash-command UX).

**Binary-embedded templates.** `include_str!()` from `crates/stacc/assets/agent/skill-content.md`
eliminates version skew between installed files and binary behavior.

**Single content body.** Both install targets get identical body content. Only the thin
wrapper differs: SKILL.md gets agentskills.io frontmatter; `stacc.md` gets a `# stacc`
heading. No per-harness content maintenance.

**None pre-selected in checklist.** Explicit opt-in; no silent writes to user-global
paths without understanding what is happening.

**`stacc agent` namespace.** Reserves `stacc agent status`, `stacc agent uninstall` as
natural follow-ons without rename.

**`AgentAction::Install(AgentInstallArgs)` pattern.** Mirrors `AuthArgs`/`AuthAction`
exactly; clap dispatches the sub-action, `agent.rs` dispatches internally.

**Empty `--harness` vec = interactive checklist.** Non-interactive with empty harness
returns a usage error. `--harness all` is a shortcut for all targets.

**`init` calls into agent install's checklist directly.** After successful init in an
interactive session, show the same multi-select (none pre-selected); Esc/empty-select =
skip cleanly. Non-interactive init silently omits the prompt.

## Existing patterns to follow

| Pattern | Location |
|---|---|
| Two-level subcommand (`auth` / `AuthArgs` / `AuthAction`) | `crates/stacc/src/cli.rs:163-179`, `commands/auth.rs` |
| `interactive::allowed()` TTY gate | `crates/stacc/src/interactive.rs:13` |
| `interactive::prompt_multi_select()` checkbox prompt | `crates/stacc/src/interactive.rs:45` |
| Dispatch arm in `dispatch()` | `crates/stacc/src/lib.rs:97-143` |
| `mod agent;` / `pub use agent::agent;` in commands module | `crates/stacc/src/commands.rs:17-41` |
| `include_str!()` with `CARGO_MANIFEST_DIR` | Rust std |
| `init()` signature adding `no_interactive` | `commands::sync()` adds same param |

## File map

| File | Change |
|---|---|
| `crates/stacc/assets/agent/skill-content.md` | CREATE: canonical body content |
| `crates/stacc/src/cli.rs` | MODIFY: add `Agent`, `AgentArgs`, `AgentAction`, `AgentInstallArgs`, `AgentHarness` |
| `crates/stacc/src/commands/agent.rs` | CREATE: `agent()`, `agent_install()` |
| `crates/stacc/src/commands.rs` | MODIFY: `mod agent;`, `pub use agent::agent;`, `init()` no_interactive param, init prompt |
| `crates/stacc/src/lib.rs` | MODIFY: BUILTINS, dispatch arm, init call |

No new crate dependencies needed. `inquire` (for `MultiSelect`) and `serde_json` (for
JSON output) are already in scope.

## Implementation units

### U1: Skill content asset

**File:** `crates/stacc/assets/agent/skill-content.md`

Create the directory and file. Content is the agent invocation reference extracted from
`AGENTS.md` sections "Using stacc as an agent" (numbered 1-8). Key correctness requirement:
every CLI invocation example must use `--json` (not `--format json`). This is the single
source of truth; both install targets write this body.

The file is embedded at compile time via:
```rust
const SKILL_CONTENT: &str = include_str!(
    concat!(env!("CARGO_MANIFEST_DIR"), "/assets/agent/skill-content.md")
);
```

**SKILL.md wrapper** (written to `~/.agents/skills/stacc/SKILL.md`):
```
---
name: stacc
description: Stacked-diff CLI for AI coding agents -- usage reference
version: "<binary version via env!(STACC_VERSION)>"
---

<SKILL_CONTENT>
```

**Claude Code slash command wrapper** (written to `~/.claude/commands/stacc.md`):
```
# stacc

<SKILL_CONTENT>
```

**Test scenarios:**
- Content includes `--json` (not `--format json`) in all invocation examples.
- SKILL.md wrapper is valid YAML frontmatter followed by body.
- Claude Code wrapper starts with `# stacc` heading, no frontmatter.

---

### U2: CLI scaffolding

**File:** `crates/stacc/src/cli.rs`

Add `Agent(AgentArgs)` to `Command` enum. Alphabetical position: between `Absorb` and
`Auth` (currently `Auth` is at index 2; `Agent` sorts before it).

```
/// Manage agent context files (install skill files for coding agents).
Agent(AgentArgs),
```

Add structs/enums below the `AuthArgs`/`AuthAction` block (or near it, keeping
non-`Command` types clustered):

```rust
pub struct AgentArgs {
    #[command(subcommand)]
    pub action: AgentAction,
}

pub enum AgentAction {
    /// Install agent context files for one or more harnesses.
    Install(AgentInstallArgs),
}

pub struct AgentInstallArgs {
    /// Harness(es) to install context for. Repeat for multiple.
    /// Omit to select interactively.
    #[arg(long, value_enum)]
    pub harness: Vec<AgentHarness>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentHarness {
    /// Universal skill (~/.agents/skills/stacc/SKILL.md)
    Universal,
    /// Claude Code slash command (~/.claude/commands/stacc.md)
    #[value(name = "claude-command")]
    ClaudeCommand,
    /// All of the above
    All,
}
```

**Test scenarios:**
- `stacc agent install --help` describes both items and the `--harness` flag.
- `stacc agent install --harness universal --harness claude-command` parses to `vec![Universal, ClaudeCommand]`.
- `stacc agent install --harness all` parses to `vec![All]`.
- Bare `stacc agent install` (no flags) parses to `harness: vec![]`.
- `stacc agent` (no sub-action) prints help, exits non-zero.

---

### U3: `agent install` command

**File:** `crates/stacc/src/commands/agent.rs`

Public surface:
```rust
pub fn agent(args: &AgentArgs, format: OutputFormat, no_interactive: bool) -> Result<(), Error>
```

Dispatch:
```rust
match args.action {
    AgentAction::Install(install_args) => agent_install(install_args, format, no_interactive),
}
```

`agent_install` logic:

1. **Resolve targets.** Expand `AgentHarness::All` to `[Universal, ClaudeCommand]`.
   Deduplicate. If targets list is empty after expansion:
   - Interactive allowed (`interactive::allowed(stdout.is_terminal(), no_interactive, format)`):
     present `prompt_multi_select` with items `["Universal skill (~/.agents/skills/stacc/SKILL.md)",
     "Claude Code slash command (~/.claude/commands/stacc.md)"]`, none pre-selected.
     Map indices back to `AgentHarness` variants. Empty selection = no-op, return success.
   - Not interactive: return `Error::Usage("no harnesses specified; pass --harness or run interactively")`.

2. **Install each target.** For each resolved target:
   - Expand `~` via `home::home_dir()` (already a transitive dep via stacc-config or similar;
     if unavailable, use `std::env::var("HOME")` with an error on failure).
   - Create parent directory with `fs::create_dir_all`.
   - Write file content with `fs::write` (overwrites existing; idempotent).
   - Record `installed` path.

3. **Report.** JSON: `{"op":"agent-install","installed":[{"target":"...","path":"..."}],"skipped":[],"schema_version":2}`.
   Pretty: one line per installed path.

**Target paths:**
- `Universal` -> `~/.agents/skills/stacc/SKILL.md`
- `ClaudeCommand` -> `~/.claude/commands/stacc.md`

**Content per target:**
- `Universal`: SKILL.md wrapper (frontmatter + SKILL_CONTENT)
- `ClaudeCommand`: Claude Code wrapper (`# stacc\n\n` + SKILL_CONTENT)

The version string in SKILL.md frontmatter comes from `env!("STACC_VERSION")` (already
used in `cli.rs:13`).

**Test scenarios:**
- `--harness universal` creates `~/.agents/skills/stacc/SKILL.md` with valid frontmatter
  and body containing `--json`.
- `--harness claude-command` creates `~/.claude/commands/stacc.md` starting with `# stacc`.
- `--harness all` creates both files; `--harness universal --harness claude-command`
  equivalent.
- Running twice: second run overwrites first; no duplicate content, no error.
- `--no-interactive` with no `--harness`: returns usage error.
- `--no-interactive --harness all`: silently installs both, returns JSON output.
- `--json` output: valid JSON with `op`, `installed` array, `schema_version: 2`.
- Interactive Esc/empty-select: returns success with `installed: []`.
- Parent directories created when `~/.agents/skills/stacc/` does not exist.
- `home_dir()` failure: returns descriptive error (not panic).

---

### U4: Wire dispatch

**File:** `crates/stacc/src/lib.rs`

Add `"agent"` to `BUILTINS` const (maintain alphabetical order; before `"auth"`).

Add dispatch arm (between `Absorb` and `Auth`):
```rust
Command::Agent(args) => commands::agent(args, cli.global.output_format(), cli.global.no_interactive),
```

**File:** `crates/stacc/src/commands.rs`

Add `mod agent;` to the module declarations block (alphabetical, before `mod auth;`).
Add `pub use agent::agent;` to the re-export block.

**Test scenarios:**
- `stacc agent install --no-interactive --harness all --json` dispatches through and
  produces JSON output (integration-level sanity).
- `stacc agent` alone prints help text (clap default for missing subcommand).
- Alias expansion: "agent" is in BUILTINS so it is never proxied to git.

---

### U5: `stacc init` integration

**File:** `crates/stacc/src/commands.rs`

Add `no_interactive: bool` to `init()` signature:
```rust
pub fn init(args: &InitArgs, format: OutputFormat, no_interactive: bool, work_dir: &Path) -> Result<(), Error>
```

After the `report(format, "initialized", &repo)` call (not on `"already_initialized"`),
if `interactive::allowed(std::io::stdout().is_terminal(), no_interactive, format)`:
- Call `crate::commands::agent::agent_install_interactive(format)`.
- This is a `pub(crate)` helper in `agent.rs` that runs the multi-select checklist
  (same as `agent_install` with empty harness vec in interactive mode).
- Any error from install is reported as a non-fatal warning (don't fail `init` if
  the optional install step fails).

**File:** `crates/stacc/src/lib.rs`

Update dispatch arm for `Command::Init`:
```rust
Command::Init(args) => commands::init(args, cli.global.output_format(), cli.global.no_interactive, work_dir),
```

**Test scenarios:**
- `stacc init` on a fresh repo (interactive): after trunk/remote detection, checklist
  appears; Esc skips install cleanly; init still reports success.
- `stacc init --no-interactive`: init completes normally, no agent prompt.
- `stacc init --json`: init completes with JSON output, no agent prompt.
- `stacc init` when already initialized: no agent prompt (already_initialized path).
- Install error during init: printed as warning; init exits successfully.

---

## Sequencing

U1 and U2 are independent; implement concurrently or in either order.
U3 depends on U1 (needs the embedded asset) and U2 (needs the CLI arg types).
U4 depends on U3 (needs the `agent()` function).
U5 depends on U3 (needs `agent_install_interactive`) and U4 (needs init dispatch updated).

Recommended order: U1 + U2 together, then U3, then U4 + U5 together.

## Risk and mitigations

| Risk | Mitigation |
|---|---|
| `home_dir()` unavailable on some env | Error::Usage with clear message; never panic |
| Write fails (permissions, disk full) | Surface `io::Error` with path in message |
| SKILL.md frontmatter rejected by some clients | Test with agentskills.io spec; frontmatter is the documented format |
| Version string in frontmatter requires format match | `env!("STACC_VERSION")` is already used in cli.rs; same value |
| `init` interactive path double-prompts on re-run | Guard with `"already_initialized"` check (already in init) |
| `--harness all` vs individual flags: dedup needed | Resolve All to variants first, then deduplicate with a `HashSet` |

## Acceptance criteria

From requirements doc (abridged):
- `stacc agent install` exists; `--help` describes both checklist items.
- Bare `stacc agent install` in a terminal: interactive checklist, nothing pre-selected.
- `--no-interactive --harness all`: installs both files silently.
- Idempotent: re-running overwrites with current content, no errors.
- Installed `~/.agents/skills/stacc/SKILL.md` uses `--json` (not `--format json`).
- Installed `~/.claude/commands/stacc.md` uses `--json`, fixes any existing file.
- `stacc init` in interactive mode offers agent install as final step; skips cleanly if declined.
- Command reports what it installed (paths written or updated).
