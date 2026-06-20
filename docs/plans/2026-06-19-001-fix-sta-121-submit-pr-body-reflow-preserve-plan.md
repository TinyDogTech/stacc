---
title: "fix(stacc): [STA-121] reflow hard-wrapped commit bodies and preserve manual PR description edits"
type: fix
date: 2026-06-19
---

# fix(stacc): [STA-121] reflow hard-wrapped commit bodies and preserve manual PR description edits

## Summary

`stacc submit` has two bugs in how it handles PR descriptions. First, commit bodies hard-wrapped at ~72 columns (conventional-commit style) are sent verbatim to GitHub, where GFM renders each single newline as a line break, producing mid-sentence breaks in PR descriptions. Second, `stacc submit` sends `body: Some(body)` on every PR update unconditionally, silently overwriting any manual PR body edits the user made via `gh pr edit`.

Two focused fixes: (1) reflow commit body paragraphs before using them as PR descriptions, and (2) skip the body field in `PullRequestUpdate` when no explicit description source exists (no `--description` flag and no previously-stored description).

---

## Problem Frame

**Bug 1: Wrapped bodies render broken GFM.**

`commit_body()` calls `git log -1 --format=%b`, which returns the raw commit body with hard wraps at ~72 columns. GitHub renders single newlines inside a paragraph as hard line breaks (`<br>`), so a commit body like:

```
Adds support for stacked PRs by walking the downstack chain
from tip to trunk before pushing.
```

renders in the PR description as two separate lines rather than a single sentence.

**Bug 2: Manual PR body edits are overwritten.**

`submit` builds a `body` value via the cascade `--description > stored_desc > commit_body()` and always passes it as `body: Some(body)` in `PullRequestUpdate` (commands.rs:739). When the user later edits the PR body via `gh pr edit`, that edit lives only on GitHub. Stacc has no record of it. The next `stacc submit` re-derives the body from the commit (since `stored_desc` is None if `--description` was never passed) and overwrites the user's edit.

`PullRequestUpdate.body` is already `Option<String>` with `#[serde(skip_serializing_if = "Option::is_none")]` and a doc comment "Unset fields are left as-is." The infrastructure for fix 2 already exists.

---

## Requirements

- R1. When PR description falls back to `commit_body()`, reflow the text: join lines within a paragraph (blank-line-separated) into a single line before sending to GitHub.
- R2. Reflow applies only to commit-body-derived descriptions. `--description` text and `stored_desc` values pass through unchanged.
- R3. PR CREATE always sets the body (no behavior change on first submit).
- R4. PR UPDATE only includes `body` in the PATCH payload when an explicit description source exists: `--description` was provided on this run, OR `stored_desc` is Some (user previously used `--description`).
- R5. When no explicit description source exists on UPDATE, `body: None` is passed, and the GitHub PATCH request omits the `body` field, leaving GitHub's current value intact.

---

## Key Technical Decisions

**Reflow applies at the call site in commands.rs, not inside `commit_body()`.**
`commit_body()` is a raw git operation that other callers may use. Reflow is a PR-description concern. A private `reflow_body(text: &str) -> String` helper in commands.rs (or a shared text utility) is applied only at the point where `commit_body()` output enters the body cascade.

**Reflow algorithm: paragraph-aware line joining.**
Split input on blank lines (one or more consecutive empty lines) to identify paragraph boundaries. Within each paragraph, join all lines with a single space (stripping trailing whitespace per line). Rejoin paragraphs with `\n\n`. This handles the common case of conventional-commit bodies without attempting to preserve list markers, code fences, or other markdown structures, which are out of scope (see Scope Boundaries).

**No-overwrite implemented by making `update_body` a separate `Option<String>`.**
The existing `body` variable (lines 719-730) continues to hold the full cascade result for CREATE. For UPDATE, introduce an `update_body: Option<String>` derived from the explicit-source check: `desc_update.as_ref().map(|(_, d)| d.clone()).or_else(|| stored_desc.clone())`. Pass `body: update_body` to `PullRequestUpdate` and `body: body` to `NewPullRequest`. No restructuring of the cascade is needed; only the UPDATE dispatch site changes.

**Reflow applies before `desc_update` is set.**
In the `--description` branch (lines 721-724), `resolved` comes from user input and is stored as-is. Reflow is only applied in the `commit_body()` fallback branch (the `None` arms of the `is_current` and `else` cases on lines 726 and 729).

---

## Scope Boundaries

### In scope
- Paragraph reflow for commit-body-derived PR descriptions
- No-overwrite protection for PR body on re-submit

### Deferred to Follow-Up Work
- Preserving markdown list markers, code fences, or other block-level elements in reflowed bodies. The reflow algorithm handles plain prose; structured markdown in commit bodies is uncommon and can be addressed when reported.
- A `--reset-description` flag to clear `stored_desc` and revert to commit-body-derived mode on the next submit.
- Applying reflow retroactively to already-stored `stored_desc` values (backward compatibility with pre-fix state).
- **Title field overwrite**: `title` has the same overwrite behavior as `body` (always sent on UPDATE regardless of whether the user renamed the PR on GitHub). The same no-overwrite fix could apply to `title`; explicitly out of scope here to keep the diff small.
- **Adopted-PR body initialization**: when `stacc submit` adopts an existing PR (created outside stacc), U2 leaves the existing GitHub body unchanged on first adoption because `stored_desc = None`. An optional "initialize body on first adoption" carve-out would make adoption behave like CREATE for body; deferred.
- **Non-current downstack branches**: branches that are not the current branch at submit time cannot receive a new `--description` in the same run. A downstack branch whose initial CREATE sent a hard-wrapped body (before this fix shipped) can only be corrected by checking it out and resubmitting with `--description`.

### Outside scope
- Documentation-only changes (suggestion (c) from the issue).
- Changes to CREATE behavior (always sets body, reflow applied to commit_body fallback).

---

## Implementation Units

### U1. Add `reflow_body` helper and apply to commit-body fallback

**Goal:** When `commit_body()` output is used as PR description, reflow paragraphs so single-newline wraps are collapsed before reaching GitHub.

**Requirements:** R1, R2.

**Dependencies:** None.

**Files:**
- `crates/stacc/src/commands.rs` (add helper, apply in body cascade)
- `crates/stacc/tests/submit.rs` (new tests for reflow behavior)

**Approach:**
Add a private `fn reflow_body(text: &str) -> String` helper. Split on runs of blank lines using a simple line-by-line scan: accumulate non-blank lines into a paragraph buffer, flush to output as a space-joined line when a blank line or end-of-input is encountered. Rejoin paragraphs with `\n\n`. Strip trailing whitespace from each input line before joining.

Apply `reflow_body()` at the two `commit_body()` call sites in the body cascade:
- Line 726 (is_current, no --description): `stored_desc.unwrap_or_else(|| reflow_body(&git.commit_body(branch).unwrap_or_default()))`
- Line 729 (non-current, no stored): same pattern

Do not call `reflow_body()` on the `resolved` value from `--description` (lines 721-725) or on `stored_desc` values.

**Patterns to follow:** The `resolve_description()` helper at commands.rs:844 as the pattern for a private string-processing function adjacent to submit.

**Test scenarios:**
- Single-paragraph commit body wrapped at 72 columns: submit creates PR with body joined into one line, no mid-sentence breaks.
- Multi-paragraph body (blank-line-separated): each paragraph joined to a single line, paragraphs separated by `\n\n` in the PR body.
- Body with trailing whitespace on some lines: trailing whitespace stripped before joining.
- Empty commit body: PR body is empty string; no panic.
- Commit body that is already a single long line per paragraph: body is unchanged (idempotent).
- `--description "My custom text\nwrapped"` provided: text passed as-is to GitHub, no reflow applied.
- `stored_desc` present from prior `--description` run: stored value used as-is, no reflow.

**Verification:** `cargo test -p stacc submit` passes; a PR created from a hard-wrapped commit body has its paragraphs joined in the GitHub API request body.

---

### U2. Skip PR body field on UPDATE when no explicit description source

**Goal:** When re-submitting an existing PR without `--description` and without a stored description, omit `body` from the PATCH payload so GitHub's current PR body (including manual edits) is preserved.

**Requirements:** R3, R4, R5.

**Dependencies:** None (independent of U1 at the code level, though both touch the body resolution block).

**Files:**
- `crates/stacc/src/commands.rs` (restructure UPDATE dispatch)
- `crates/stacc/tests/submit.rs` (new tests for no-overwrite behavior)

**Approach:**
Introduce `update_body: Option<String>` computed after the `body` variable is resolved:

```
// Directional sketch only -- not implementation specification
update_body = desc_update.map(body from explicit desc)
           OR stored_desc.clone()
           OR None
```

Change the UPDATE dispatch from `body: Some(body)` to `body: update_body`. Leave the CREATE dispatch as `body: body` (unchanged).

This relies on `PullRequestUpdate.body`'s existing `skip_serializing_if = "Option::is_none"` to ensure `None` means "field not sent" rather than "set body to null." The GitHub PATCH API ignores absent fields.

For the non-current-branch case in a stack, `stored_desc` is per-branch state; the same logic applies: if a non-current branch has no stored description, its body is not updated on re-submit.

**Patterns to follow:** The existing `title_update` / `desc_update` optional-pair pattern (lines 780-789) for tracking explicit overrides. The `PullRequestUpdate` "Unset fields are left as-is" invariant (stacc-github/src/lib.rs:82).

**Test scenarios:**
- Re-submit existing PR, no `--description`, `stored_desc = None`: PATCH request body does not contain a `"body"` key.
- Re-submit existing PR with `--description "New body"`: PATCH request includes `"body":"New body"`.
- Re-submit existing PR with `stored_desc = Some("Stored body")`, no `--description`: PATCH request includes `"body":"Stored body"` (stored description is maintained).
- Submit new PR (no existing), no `--description`: POST always includes `"body"` (CREATE unchanged).
- Submit new PR with hard-wrapped commit body (U1 applied): POST body is reflowed.
- Re-submit after user ran `gh pr edit` to set a custom body, no stacc `--description` ever used: PATCH omits `"body"`, manual edit survives.
- Stack with two branches: top branch has `stored_desc = None`, bottom has `stored_desc = Some("...")`. Re-submit both: top branch PATCH omits body, bottom branch PATCH includes stored body.
- Adopted PR (stacc adopts an existing GitHub PR by head-branch match, no prior `stacc submit`): `stored_desc = None`, `desc_update = None` → `update_body = None` → PATCH omits body. The existing PR body (whatever GitHub holds) is preserved. This is the same behavior as any other re-submit with no explicit source; the adoption case does not get commit-body initialization. See Scope Boundaries for the deferred carve-out.

**Verification:** `cargo test -p stacc submit` passes; httpmock assertions confirm the PATCH request body field presence/absence matches the explicit-source check.

---

## Open Questions

- **Deferred to implementation:** What is the right behavior when `stored_desc` was set from a commit body that has since changed? The current fix means the old stored description persists indefinitely. A future `--reset-description` flag or a warning on commit-body drift would address this, but is out of scope here.
- **Deferred to implementation:** `\r\n` line endings from Windows Git clients break the paragraph-split algorithm: a blank line is `"\r"` not `""`, so `"\r"` is treated as a non-blank line and joined into the paragraph, producing embedded `\r` bytes in the PR body. Should normalize `\r\n` → `\n` (or strip `\r` per line) before splitting. Known broken on Windows without this; treat as a near-term add-on, not a theoretical concern.
