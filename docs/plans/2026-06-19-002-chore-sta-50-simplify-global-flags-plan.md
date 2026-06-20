---
title: "chore(stacc): [STA-50] simplify global flags"
type: chore
date: 2026-06-19
---

# chore(stacc): [STA-50] simplify global flags

## Summary

Two global flags are unnecessarily verbose for the primary agent use-case:
`--format json` (four tokens) and `--no-interactive` (no short form). Replace
`--format {pretty,json}` with a single `--json` boolean, and add `-i` as a
short alias for `--no-interactive`. Clean break; no backwards-compat shim
(pre-1.0).

---

## Problem Frame

Agents invoking stacc must type `stacc <cmd> --no-interactive --format json`
on every call. AGENTS.md documents this pattern extensively. The `--format`
enum was designed for future extensibility that has not materialized; `pretty`
vs `json` is a boolean choice. `--no-interactive` has no short form, making
every agent invocation longer than necessary.

---

## Requirements

- R1. `--json` (boolean, global) selects JSON output; absence selects pretty.
- R2. `--format` is removed entirely; no hidden alias, no deprecation warning.
- R3. `-i` is a short alias for `--no-interactive`; the long form still works.
- R4. All integration tests and AGENTS.md updated to use the new flags.
- R5. `OutputFormat` enum remains as an internal type; only CLI surface changes.

---

## Key Technical Decisions

**Remove `--format`, not alias it.** Pre-1.0, no external users have built
integrations against the flag contract. Adding a hidden alias would keep dead
code in the parser and confuse `--help`. Clean removal per project philosophy
(single binary, zero-config, no backwards-compat shims for unreleased surface).

**`--json` as top-level boolean, not a subcommand modifier.** Matches clap's
convention for simple binary output switches (`--quiet`, `--verbose`). Keeps
`GlobalArgs.json: bool` and lets all downstream dispatch stay clean via a
helper method.

**`GlobalArgs::output_format()` accessor method.** Replaces the 36 call sites
that reference `cli.global.format` with a single function so the boolean-to-enum
translation is in one place, not inlined at every dispatch point.

**`-i` not `-n` for `--no-interactive`.** `-n` conventionally means
"dry run" in many CLIs (`git clean -n`). `-i` is unconventional for
"non-interactive" (most CLIs use `-i` for interactive mode), but it mirrors what
stacc agents actually care about: "run without prompting." The long form
`--no-interactive` remains authoritative; `-i` is a convenience alias only.

---

## Implementation Units

### U1. Replace `--format` with `--json` and add `-i` in `cli.rs`

**Goal:** Change `GlobalArgs` so the CLI surface exposes `--json` bool and `-i`
short for `--no-interactive`. Remove `OutputFormat` from the public CLI derive.

**Requirements:** R1, R2, R3, R5.

**Dependencies:** None.

**Files:**
- `crates/stacc/src/cli.rs`

**Approach:**
Replace the `format: OutputFormat` field in `GlobalArgs` with `json: bool`:
- Remove `#[arg(long, value_enum, default_value = "pretty", global = true)]` /
  `pub format: OutputFormat`
- Add `#[arg(long, global = true)]` / `pub json: bool`

Add `-i` short to `no_interactive`:
- Change `#[arg(long, global = true)]` to `#[arg(long, short = 'i', global = true)]`

`OutputFormat` and `ColorChoice` enums remain in the file; `OutputFormat` loses
its `ValueEnum` derive since it is no longer a CLI arg value. Alternatively,
keep `#[derive(ValueEnum)]` as it costs nothing, but remove it from the `#[arg]`
attribute. Simpler: keep the derive, it does no harm and leaving it avoids
churn if it gets re-exposed later. Either is fine; prefer keeping derive to
minimize diff noise.

**Patterns to follow:** `no_interactive` field for `#[arg(long, global)]`
convention. `ColorChoice` for how an enum stays in the file without being a CLI
surface arg.

**Test scenarios:**
- `stacc --json log` parses correctly (no clap error, `json: true`).
- `stacc log` (no `--json`) has `json: false` / pretty output.
- `stacc -i log` parses as `no_interactive: true`.
- `stacc --no-interactive log` still parses (long form still works).
- `stacc --format json` returns a parse error (flag removed).
- Existing `help_lists_commands_alphabetically` test in `cli.rs` still passes.

**Verification:** `cargo test -p stacc cli` passes; `stacc --help` no longer
shows `--format`.

---

### U2. Add `output_format()` accessor and update all dispatch call sites

**Goal:** Translate `json: bool` to `OutputFormat` in one place; update every
call site that passes `cli.global.format` to commands.

**Requirements:** R1, R5.

**Dependencies:** U1.

**Files:**
- `crates/stacc/src/cli.rs` (add accessor method on `GlobalArgs`)
- `crates/stacc/src/lib.rs` (replace `cli.global.format` with `cli.global.output_format()`)

**Approach:**
Add to `impl GlobalArgs`:
```
pub fn output_format(&self) -> OutputFormat {
    if self.json { OutputFormat::Json } else { OutputFormat::Pretty }
}
```
This is directional guidance; the exact method body is obvious.

In `lib.rs`, replace every `cli.global.format` reference with
`cli.global.output_format()`. There are 36 such sites in `lib.rs`; all are
mechanical substitutions.

**Patterns to follow:** `LogArgs::form()` accessor method at `cli.rs` line 346
for the pattern of a derived accessor on a `clap::Args` struct.

Note: `interactive::allowed(is_terminal, no_interactive, format: OutputFormat)`
retains its current signature unchanged. The `output_format()` accessor translates
`json: bool` to `OutputFormat` before reaching the call site; the function body
needs no modification. The redundancy (json mode implies no prompts) is an existing
design that this plan does not restructure.

**Test scenarios:**
- `GlobalArgs { json: true, .. }.output_format()` returns `OutputFormat::Json`.
- `GlobalArgs { json: false, .. }.output_format()` returns `OutputFormat::Pretty`.
- These are unit tests in `cli.rs`'s `#[cfg(test)]` block.

**Verification:** `cargo build --workspace` succeeds with zero type errors.
`cargo test --workspace` passes (command dispatch still receives correct format).

---

### U3. Update integration tests

**Goal:** Replace `--format json` with `--json` (and optionally `-i` for
`--no-interactive`) in integration test invocations.

**Requirements:** R4.

**Dependencies:** U1, U2.

**Files:**
- `crates/stacc/tests/absorb.rs`
- `crates/stacc/tests/aliases.rs`
- `crates/stacc/tests/auth.rs`
- `crates/stacc/tests/checkout.rs`
- `crates/stacc/tests/config.rs`
- `crates/stacc/tests/create.rs`
- `crates/stacc/tests/delete.rs`
- `crates/stacc/tests/fold.rs`
- `crates/stacc/tests/info.rs`
- `crates/stacc/tests/init.rs`
- `crates/stacc/tests/log.rs`
- `crates/stacc/tests/merge.rs`
- `crates/stacc/tests/merged.rs`
- `crates/stacc/tests/modify.rs`
- `crates/stacc/tests/move.rs`
- `crates/stacc/tests/navigation.rs`
- `crates/stacc/tests/parent_children.rs`
- `crates/stacc/tests/pop.rs`
- `crates/stacc/tests/pr.rs`
- `crates/stacc/tests/proxy.rs`
- `crates/stacc/tests/recovery.rs`
- `crates/stacc/tests/rename.rs`
- `crates/stacc/tests/reorder.rs`
- `crates/stacc/tests/restack.rs`
- `crates/stacc/tests/split.rs`
- `crates/stacc/tests/squash.rs`
- `crates/stacc/tests/status.rs`
- `crates/stacc/tests/submit.rs`
- `crates/stacc/tests/sync.rs`
- `crates/stacc/tests/sync_forge_less.rs`
- `crates/stacc/tests/track.rs`
- `crates/stacc/tests/undo.rs`
- `crates/stacc/tests/untrack.rs`
- `crates/stacc/tests/worktree_safety.rs`

**Approach:**
Mechanical replacement in each file: the two-element pattern `"--format", "json"`
becomes `"--json"` (one element). Similarly `"--format", "pretty"` (if any) becomes
nothing (remove the two elements since pretty is the default). Do not replace
`--no-interactive` with `-i` in tests; tests should remain legible, not compressed.

In `proxy.rs`, also update the comment near the test that passes `"--format", "json"` --
it describes the test's discriminating intent vs. git's `--format=...`; after the
rename the comment should reference `--json`.

**Patterns to follow:** Existing test invocation style in each file.

**Test scenarios:**
- No new test scenarios; this is a mechanical flag-name update.
- All existing tests in the 6 files pass unchanged after the flag rename.

**Verification:** `cargo test --workspace` passes; no test invocations contain
`--format json`.

---

### U4. Update AGENTS.md

**Goal:** Replace all `--format json` occurrences in AGENTS.md with `--json`;
document the `-i` shorthand.

**Requirements:** R3, R4.

**Dependencies:** U1.

**Files:**
- `AGENTS.md`

**Approach:**
- Section 1 invocation rule: `stacc <cmd> --no-interactive --format json` →
  `stacc <cmd> --no-interactive --json` (keep `--no-interactive` spelled out in
  the canonical example; add a parenthetical noting `-i` is the short form).
- Section 2 error channel: update `--format json` mention.
- Section 3 substitution table: update all `--format json` in example commands.
- Section 6/7 edge cases: update flag references.
- Do not update `docs/plans/` files; those are immutable decision records.

**Patterns to follow:** Existing AGENTS.md formatting conventions.

**Test scenarios:**
- Test expectation: none -- documentation change only; no code behavior tested.

**Verification:** `rg -n '\-\-format' AGENTS.md` returns zero results.

---

## Scope Boundaries

### In scope
- `--format` → `--json` global flag replacement
- `-i` short alias for `--no-interactive`
- Test and AGENTS.md updates

### Deferred to Follow-Up Work
- `--no-interactive` / `--json` combined as a single `--agent` flag (STA-50
  implies simplification; a combined flag could come later if the two always
  travel together)
- Tab-completion script regeneration (completion output will update automatically
  when built from the new `Cli` definition; no manual step needed)

### Outside scope
- Changes to output JSON shape
- Per-command flag simplifications beyond the two global flags
- `docs/plans/` historical documents (immutable decision records)
