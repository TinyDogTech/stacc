---
title: "fix: make sync fail loudly without GitHub credentials"
type: fix
status: active
date: 2026-06-10
deepened: 2026-06-10
issue: STA-90
---

# fix: Make sync fail loudly without GitHub credentials (STA-90)

## Summary

Make `stacc sync` strict about merged-PR detection: missing GitHub credentials become a hard error with named remedies instead of a silent skip, `--offline` becomes the explicit fully-local opt-out, token resolution gains a `gh auth token` fallback, `stacc auth login` stops shipping a dead placeholder client ID, and a tree-identical restack guard backstops the phantom-conflict failure mode.

---

## Problem Frame

A `stacc sync` rebased a squash-merged branch into a phantom conflict (STA-90). Root cause, confirmed against the live API and the `refs/stacc/data` history: the tracked branches had no recorded PR (PRs were opened with `gh`), so `detect_merged` in `crates/stacc/src/commands/operations.rs` returned empty without touching GitHub, and the adoption pass (`adopt_prs`) silently no-opped because `GitHub::from_env` found no token (no env vars, no keychain entry; `stacc auth login` cannot populate the keychain because `crates/stacc-github/src/auth.rs` ships the placeholder client ID `stacc-oauth-client-id-placeholder`). With detection a no-op, sync restacked a branch whose squashed changes were already on trunk. Per-branch lookup failures are also swallowed (`let Ok(Some(pr)) = ... else continue` in `adopt_prs_among`), so even a present-but-failing token degrades silently.

The design doc already forbids this: `plans/algorithms.md` (Caveats) says "without the API we cannot detect squash-merges, and we are not shipping a degraded path." This plan realigns the code with that philosophy and closes the credential gap that made it bite.

---

## Requirements

**Strict reconciliation in sync**

- R1. `stacc sync` (without `--offline`) hard-errors when any tracked branch exists and no GitHub token resolves; the message names the remedies: `stacc auth login`, `GITHUB_TOKEN`, logging in to `gh`, and `--offline`.
- R2. `stacc sync` (without `--offline`) hard-errors when any tracked branch exists and the remote is not a GitHub URL (v1 is GitHub-only by design). The error names the remote by name; it never echoes a raw remote URL, which can carry embedded credentials (`user:token@host`, the standard CI pattern).
- R3. `stacc sync --offline` skips adoption and merged-PR detection entirely (in addition to the existing fetch skip), prints a stderr note that detection was skipped, and marks the skip in the JSON output.
- R4. Per-branch adoption lookup errors (transport, auth) are fatal in sync and in merge's adoption pass; "no PR found" still continues silently. Adoptions found before a mid-loop error are persisted before the error is returned.
- R5. `stacc sync --continue` never constructs a GitHub client and never requires credentials.

**Token resolution**

- R6. `GitHub::from_env` falls back to `gh auth token` after env vars and keychain. A missing `gh` binary, non-zero exit, or empty output reads as "no token"; the subprocess is bounded by a short timeout; the fallback is skipped when `GITHUB_API_URL` points anywhere other than github.com.
- R7. `GH_TOKEN` is checked before `GITHUB_TOKEN` (gh-ecosystem order), and empty-string env values are treated as unset.
- R8. `stacc auth status` reports the gh source and the updated precedence (env, keychain, gh).

**Auth login**

- R9. A real OAuth client ID ships embedded once the OAuth App is registered (manual prerequisite, see Risks); `stacc auth login` works end to end via the device flow.
- R10. While the placeholder ID is the effective client ID (no `STACC_OAUTH_CLIENT_ID` override, or an empty one), `stacc auth login` fails fast with an actionable message instead of starting a doomed device flow.

**Defense in depth**

- R11. During sync's restack pass, a branch whose tip tree is identical to its new base tip's tree, and whose tip is not an ancestor of that base, is skipped with a notice (it looks squash-merged) and surfaced in the output instead of being rebased.

**Test integrity**

- R12. The test suite stays hermetic: no test can reach real GitHub or the developer's own `gh` credentials, and the migrated tests cover the new strict behaviors.

**Token hygiene**

- R13. Token values never appear in stderr, error messages, debug output, or JSON output. The gh fallback's captured stdout is consumed into the token and never quoted in any error (errors may name the binary path and exit status only); `auth status` reports sources, never token values or prefixes.

---

## Key Technical Decisions

- **Hard error, not a warning, when detection cannot run.** A warning followed by a phantom conflict is the incident with extra steps. This matches the stated v1 philosophy in `plans/algorithms.md` ("not shipping a degraded path"). `--offline` is the single explicit opt-out.
- **Extend `--offline` (sync only) to mean fully local.** Today it only skips the git fetch; API detection still runs. Extending it is a semantic change, but the flag name already promises "offline", and `stacc log --no-status` (alias `offline`) sets precedent for opting out of API calls. `merge --offline` keeps its current meaning: merge cannot work without the API.
- **No new `--no-detect` flag now.** The "fetch but skip detection" persona (ssh-only, token-less) is hypothetical for v1; the gh fallback makes credentials nearly universal on dev machines. Deferred, not rejected (see Scope Boundaries).
- **Err vs Ok(None) split in adoption.** "No PR anywhere" is the common WIP case and stays non-fatal; a failed lookup is a broken environment and aborts (strict-by-default, mirroring the existing fetch behavior with its `--offline` hint).
- **Upfront client construction in sync, after the `--continue` early return.** Compute "needs API" as "any tracked branch exists" (every tracked branch either has a recorded PR or is an adoption candidate), construct the client once, and pass it down. Reuse `GitHubError::MissingToken` through the transparent `Error::Github` wrapping; extend its message to mention gh; sync adds a `hint:` stderr line about `--offline`, mirroring the existing fetch-failure hint.
- **gh fallback lives in `stacc-github`.** That crate already owns token resolution (env, keychain). The subprocess invocation mirrors `stacc-git`'s `Git::command` shape: piped output, null stdin, spawn failure and non-zero exit mapped to "no token", trimmed stdout, bounded wait. A `STACC_GH_BIN` env override doubles as the test hook (point at a fake script) and the kill switch (point at an empty value to disable), keeping the integration harness hermetic. `STACC_GH_BIN` is a program path passed directly to the process spawn (never through a shell) with a fixed argument vector; that keeps it in the same trust domain as resolving `gh` via PATH, not an escalation.
- **The non-github.com `GITHUB_API_URL` skip is a security boundary, not just test hygiene.** An ambient gh credential must never be sent as a bearer token to a non-GitHub host (a mock server, a stale dotenv value, or a hostile URL); explicitly exported env tokens do follow `GITHUB_API_URL` because the user chose both. This pairing must survive refactors.
- **Resolution order: `GH_TOKEN`, `GITHUB_TOKEN`, keychain, gh.** Explicit env vars win (gh's own docs and Copilot CLI both document env-first); stacc's own keychain entry beats gh's ambient credential; gh is last, exactly how Copilot CLI frames its gh fallback ("activates only when no other credentials are found").
- **Client ID stays an embedded constant with the existing `STACC_OAUTH_CLIENT_ID` env override.** Device-flow client IDs are safe to embed (GitHub docs; gh hardcodes its own in public source). No config-file key: the `stacc-config` `Key` namespace is deliberately closed and auth is not repo-scoped.
- **Tree-identical guard is narrow and sync-scoped.** Check: `tip^{tree}` equals base tip's `^{tree}` and tip is not an ancestor of the base (the ancestor case is already skipped as "already on base" in `restack_forced`). Zero false positives by construction; it intentionally misses squash-merges with later trunk commits on top, which API detection (now guaranteed to run or loudly absent) handles. Skip with a notice, never auto-drop state. No patch-id heuristics (explicitly out of scope in `plans/algorithms.md`). The guard applies only to sync's restack pass, not to explicit user operations (`move`, `reorder`, `restack`, `modify`).

---

## High-Level Technical Design

Sync's new gate, replacing the silent-skip paths:

```mermaid
flowchart TB
  A[stacc sync] --> B{--continue?}
  B -->|yes| C[resume rebase queue<br/>no API, no client]
  B -->|no| D{--offline?}
  D -->|yes| E[note: detection skipped<br/>prune + restack local refs only]
  D -->|no| F{any tracked branches?}
  F -->|no| G[fetch trunk, done]
  F -->|yes| H{remote parses as GitHub?}
  H -->|no| X1[hard error: v1 is GitHub-only<br/>hint: --offline]
  H -->|yes| I{token resolves?<br/>GH_TOKEN > GITHUB_TOKEN > keychain > gh}
  I -->|no| X2[hard error: MissingToken<br/>remedies: auth login, env, gh, --offline]
  I -->|yes| J[adopt PRs by head branch<br/>lookup Err = fatal, persist partial]
  J --> K[detect merged by PR number]
  K --> L[drop merged + reparent children]
  L --> M[fetch trunk]
  M --> N[restack remainder<br/>tree-identical guard skips look-merged branches]
```

Token resolution chain (`GitHub::from_env`):

```mermaid
flowchart TB
  T1{GH_TOKEN set, non-empty?} -->|yes| U[use it]
  T1 -->|no| T2{GITHUB_TOKEN set, non-empty?}
  T2 -->|yes| U
  T2 -->|no| T3{keychain entry stacc/github.com?}
  T3 -->|yes| U
  T3 -->|no| T4{github.com target AND gh resolves a token?<br/>STACC_GH_BIN override, bounded wait}
  T4 -->|yes| U
  T4 -->|no| X[Err MissingToken]
```

---

## Implementation Units

### U1. Migrate sync tests off the offline-plus-mock pattern

- **Goal:** Free `--offline` from its accidental "API yes, fetch no" test role so U2 can change its semantics without breaking a dozen tests in the same commit.
- **Requirements:** R12 (groundwork for R3).
- **Dependencies:** none.
- **Files:** `crates/stacc/tests/sync.rs` (helpers and the affected tests; the analyzer flagged the adoption/detection tests around lines 226, 294, 397, 540, 568, 598, 624, 694, 727, 763, 795, 820).
- **Approach:** Add an `online_repo`-style helper to sync.rs mirroring `crates/stacc/tests/merge.rs` (`git config url.<file://bare-path>.insteadOf <origin URL>` so the remote still parses as GitHub but fetch/push hit a local bare repo). Migrate tests that pair `--offline` with a live `GITHUB_API_URL` mock to run without `--offline` against the local-bare remote. Pure test refactor; behavior under test is unchanged and the suite stays green.
- **Test scenarios:** the migrated tests themselves are the coverage; each must assert the same adoption/merged/restack outcomes as before, now via online-mode sync. Verify no remaining sync test depends on `--offline` still reaching the API.
- **Verification:** `cargo test --workspace` green with zero behavior changes in src.

### U2. Strict reconciliation gate in sync

- **Goal:** No silent degradation: sync either runs detection, or the user explicitly opted out, or it errors with remedies.
- **Requirements:** R1, R2, R3, R4, R5, R13.
- **Dependencies:** U1.
- **Files:** `crates/stacc/src/commands/operations.rs` (sync, `adopt_prs`, `adopt_prs_among`, `detect_merged`, `report_sync`), `crates/stacc/src/cli.rs` (`SyncArgs` `--offline` doc comment), `crates/stacc-github/src/error.rs` (MissingToken message), `crates/stacc/tests/sync.rs`, `crates/stacc/tests/merge.rs`.
- **Approach:** After the `--continue` early return: if not offline and state has tracked branches, parse the remote (hard error when non-GitHub, reusing the existing Usage error shape from `detect_merged`, naming the remote but never echoing its URL, which can carry `user:token@` credentials) and construct `GitHub::from_env()?` once; thread the client into adoption and detection instead of their internal silent constructions. In `adopt_prs_among`, replace the swallowing `let Ok(Some(pr))` with an error-propagating match that distinguishes Err (persist adoptions found so far, then return the error) from Ok(None) (continue). When offline, skip adoption and detection, print `note: --offline skipped merged-PR detection; run stacc sync online to reconcile merged PRs.`, and add a detection-skipped marker to the JSON object emitted by `report_sync`. Extend the MissingToken message to name gh; sync prints a `hint:` line naming `--offline` before returning the error (mirror the existing fetch hint). Audit remaining non-offline sync invocations under the hermetic test default (dead `GITHUB_API_URL`) since lookup failures are now fatal.
- **Test scenarios:**
  - Tracked branch, no token anywhere (harness disables the gh fallback): sync exits with the MissingToken error, JSON `error` discriminator is stable, stderr carries the `--offline` hint, and no rebase or state mutation happened.
  - Tracked branch, non-GitHub remote URL: sync errors naming GitHub-only v1; `--offline` succeeds on the same repo.
  - Tracked branch, remote URL with embedded credentials (`https://x-access-token:SECRET@github.com/o/r`, which fails the strict remote parse today): the error output (text and JSON) contains the remote name but not the string SECRET.
  - `sync --offline` with tracked branches and no token: succeeds, restacks local refs, prints the skipped-detection note, JSON contains the skip marker.
  - Adoption lookup returns 500 (mock): sync fails with the API error; an adoption that succeeded earlier in the loop is persisted in state despite the failure.
  - Merge's adoption pass with a failing lookup (mock): merge fails fast rather than silently skipping (pins the shared-core change for merge).
  - Conflict-interrupted sync, then `sync --continue` with no token and a dead API URL: resume succeeds (no client constructed on the continue path); offline note is not printed on the continue path.
  - Empty state (no tracked branches), no token: sync succeeds (fetch only, no API needed).
  - Regression: the full STA-90 shape, squash-merged PR adopted by head branch on an online sync, branch dropped and ref cleaned, no phantom conflict (largely existing coverage, re-run post-change).
- **Verification:** all new scenarios green; no test reaches real GitHub; the incident repro (tracked branch with merged PR, no credentials) can no longer rebase.

### U3. gh auth token fallback and auth status reporting

- **Goal:** Machines authenticated via `gh` (the documented PR workflow for this repo) work out of the box, removing the most common cause of the R1 error.
- **Requirements:** R6, R7, R8, R13, R12 (kill switch).
- **Dependencies:** none (lands independently; U2's error becomes rarer once this ships).
- **Files:** `crates/stacc-github/src/lib.rs` (`from_env`), `crates/stacc-github/src/auth.rs` or a new small module for the subprocess, `crates/stacc/src/commands/auth.rs` (`status`), `crates/stacc-github` unit tests, `crates/stacc/tests/sync.rs` (harness kill switch via `STACC_GH_BIN`).
- **Approach:** Flip env order to `GH_TOKEN` then `GITHUB_TOKEN`; treat empty values as unset. After keychain, invoke `gh auth token --hostname github.com` (binary name from `STACC_GH_BIN` when set; empty value disables the fallback): null stdin, piped output, bounded wait (about 5s; thread-based wait or the `wait-timeout` crate, implementer's choice), trim stdout, map spawn failure, non-zero exit, or empty output to "no token" and continue to MissingToken. Subprocess errors may name the binary path and exit status, never captured output (the stdout IS the token). Skip the fallback when `GITHUB_API_URL` is set to a non-github.com base (tests, GHES). Update `auth status` to report the gh source and precedence; keep its JSON shape additive. Set `STACC_GH_BIN=` (empty) in the integration-test harness defaults alongside the existing hermetic env so missing-credential tests stay deterministic on dev machines with gh logged in.
- **Test scenarios:**
  - Fake gh script printing a token: `from_env` returns it when env and keychain are empty.
  - Fake gh exiting 1 with stderr noise: resolution falls through to MissingToken.
  - Fake gh printing empty stdout: treated as no token.
  - `STACC_GH_BIN` empty: fallback disabled, MissingToken without spawning anything.
  - `GH_TOKEN=x` and `GITHUB_TOKEN=y` both set: x wins; `GITHUB_TOKEN=""` alone: treated as unset and the chain continues.
  - `GITHUB_API_URL` pointing at a mock server: fallback not attempted (no fake-gh invocation recorded).
  - `auth status` with only a fake gh token available: reports source gh; with env set: reports env and notes precedence.
  - Token hygiene: with a fake gh printing a known sentinel token, force a downstream API failure and assert the sentinel never appears in stderr, the error message, or JSON output (pins R13).
- **Verification:** unit tests cover the chain order and edge cases without real gh; integration suite remains hermetic (no test output changes on a machine with gh logged in vs without).

### U4. Working OAuth client ID path for auth login

- **Goal:** `stacc auth login` either works (registered app) or says exactly why it cannot, instead of failing into a confusing device-flow error.
- **Requirements:** R9, R10.
- **Dependencies:** none (code side); R9 additionally depends on the manual app registration (Risks).
- **Files:** `crates/stacc-github/src/auth.rs` (`DEFAULT_OAUTH_CLIENT_ID`, `DeviceFlow::default`), `crates/stacc/src/commands/auth.rs` (login fail-fast), `crates/stacc-github/tests/auth.rs`.
- **Approach:** Add a fail-fast in login when the effective client ID equals the placeholder (treat an empty `STACC_OAUTH_CLIENT_ID` as unset, per the existing `env_or` helper's gap): error message states this build has no registered OAuth app and points at the working alternatives (gh login, `GITHUB_TOKEN`). Once the OAuth App is registered under TinyDogTech (device flow enabled, no client secret needed; the registration steps live in the STA-17 PR description), replace the constant with the real ID and confirm the requested scope set (classic `repo` at minimum) supports read, create, and merge on private repos. Login output should tell the user to confirm the app name on GitHub's authorization page before approving (device flow is phishable by design; codes should only come from a login the user started).
- **Test scenarios:**
  - Placeholder ID, no override: login exits with the fail-fast message; no device-code request is issued (assert via mock server receiving zero requests).
  - Placeholder ID, `STACC_OAUTH_CLIENT_ID=""`: same fail-fast (empty treated as unset).
  - `STACC_OAUTH_CLIENT_ID=real-ish`: device flow proceeds against the mock (existing auth.rs test pattern).
- **Verification:** with a registered ID embedded, a manual end-to-end `stacc auth login` on a clean machine stores a keychain token and `auth status` verifies the user.

### U5. Tree-identical pre-restack guard in sync

- **Goal:** Even with detection unavailable or wrong, sync never rebases a branch that is byte-for-byte already contained in its base (the exact STA-90 shape).
- **Requirements:** R11.
- **Dependencies:** U2 (reuses the report surface; conceptually independent).
- **Files:** `crates/stacc-git/src/lib.rs` (a `trees_identical` style helper using the existing `rev_parse("{rev}^{tree}")` idiom from `crates/stacc/src/commands/removal.rs`), `crates/stacc-core/src/ops.rs` (`restack_forced` skip, gated by an option so only sync enables it), `crates/stacc/src/commands/operations.rs` (enable for sync's restack pass; surface the skip), `crates/stacc/tests/sync.rs`, `stacc-core` unit tests.
- **Approach:** In `restack_forced`'s per-branch decision ladder (after the existing "already on base" ancestor skip), when the guard is enabled: if the branch tip's tree equals the base tip's tree and the tip is not an ancestor of the base, skip the rebase, record a skip reason ("tree-identical to base; looks squash-merged"), and continue. Sync prints a notice naming remedies (run sync online to confirm and clean up, or `stacc delete`), and the skip appears in sync's JSON output. Explicit operations (`move`, `reorder`, `restack`, `modify`) do not enable the guard.
- **Test scenarios:**
  - Offline sync where trunk was fast-forwarded to a squash of the branch (local setup, no API): branch is skipped with the notice, not rebased, no conflict; state still tracks it.
  - Same branch on a subsequent online sync with the PR mocked merged: adoption/detection drops it (guard does not mask the real reconcile path).
  - Fresh empty branch created on the trunk tip (tree-identical AND ancestor): not flagged by the guard (already covered by the ancestor skip; pin with a test).
  - Branch with real pending changes (trees differ): rebased normally.
  - Explicit `stacc restack` of a tree-identical branch: not skipped (guard off outside sync).
- **Verification:** the STA-90 incident repro under `--offline` ends in a skip notice instead of a rebase conflict.

---

## Scope Boundaries

**Non-goals**

- No patch-id or commit-comparison heuristics for squash detection (explicitly rejected for v1 in `plans/algorithms.md`; the API is the source of truth).
- No GHES / non-github.com host support for the gh fallback (v1 is github.com-only; `GITHUB_API_URL` remains a test/GHES escape hatch that disables the fallback).
- No config-file key for the OAuth client ID or token (the `stacc-config` namespace stays closed; env override only).
- `merge --offline` semantics unchanged.

**Deferred to follow-up work**

- A "fetch and restack but skip detection" mode (`--no-detect` or similar) if a real token-less persona shows up.
- Distinguishing a locked/unavailable keychain from an absent entry inside `from_env` (U3 may surface keyring errors in `auth status` cheaply; the full distinction is follow-up).
- Mapping 401/403 lookup failures to a tailored re-auth message (the generic API error with status is acceptable for now).
- Rejecting or warning on ignored flag combinations with `sync --continue` (existing behavior, unchanged).

---

## Acceptance Examples

- AE1. Given a tracked branch whose PR was squash-merged out of band and no credentials anywhere, when `stacc sync` runs, then it exits with the MissingToken error naming `stacc auth login`, `GITHUB_TOKEN`, gh, and `--offline`, and performs no rebase (this is the STA-90 incident, now loud).
- AE2. Given the same repo state but `gh` is installed and logged in, when `stacc sync` runs, then the gh fallback supplies the token, adoption finds the merged PR by head branch, the branch is dropped and its ref cleaned, and no conflict occurs.
- AE3. Given tracked branches and no credentials, when `stacc sync --offline` runs, then it restacks local refs, prints the skipped-detection note, and the JSON output carries the skip marker.
- AE4. Given a conflict-interrupted sync and no credentials, when `stacc sync --continue` runs, then it resumes and completes without constructing a GitHub client.
- AE5. Given the placeholder client ID and no override, when `stacc auth login` runs, then it fails fast with the actionable message and never starts the device flow.
- AE6. Given `--offline` and a branch tree-identical to the freshly fast-forwarded trunk tip (tip not an ancestor), when sync restacks, then the branch is skipped with the looks-squash-merged notice instead of conflicting.

---

## Risks & Dependencies

- **Manual prerequisite for R9:** someone with TinyDogTech org access must register the GitHub OAuth App (device flow enabled; client ID is safe to embed; no verification required for a public OAuth App). Until then, U4 ships only the fail-fast, and the gh fallback (U3) is the practical auth path. Registration steps are in the STA-17 PR description.
- **`--offline` semantic change:** scripts or agents that relied on `sync --offline` still detecting merged PRs will see detection skipped. Mitigated by the stderr note, the JSON marker, and the flag's doc update; flagged for the changelog.
- **Strictness surfaces flaky networks:** lookup failures now abort sync by design. `--offline` is the escape; the error carries the API failure detail.
- **Test migration breadth:** about a dozen tests change harness shape in U1; isolating it as a no-behavior-change commit keeps the risk reviewable.
- **gh fallback hermeticity:** without the `STACC_GH_BIN` kill switch in the harness, missing-credential tests would pass or fail depending on the developer's gh login state; the kill switch is part of U3's definition of done.
- **Scope breadth of the reused gh token:** the gh OAuth token carries `repo`, `read:org`, and `gist` (sometimes `workflow`), more than stacc needs, and the fallback activates without explicit user action. Mitigations: `auth status` names gh as the source, docs recommend a fine-grained PAT or `stacc auth login` (repo-only) for least privilege, `STACC_GH_BIN=` (empty) is the opt-out, and the gh-sourced token lives in process memory only (never written to stacc's keychain, state refs, or any file).
- **Credentialed remote URLs:** `https://user:token@github.com/...` remotes currently fail the strict remote parse and will route to the new GitHub-only error; the message must strip or omit the URL (R13/R2). Making `parse_remote` accept credentialed github.com URLs is a follow-up.
- **Device flow abuse:** the embedded client ID is a shared public identifier; third-party abuse could get the OAuth App rate-limited or suspended, disabling `stacc auth login` for everyone. Contingency paths: `GITHUB_TOKEN` and the gh fallback.

---

## Sources & Research

- Root-cause evidence: `refs/stacc/data` state history (branch records with no `pr` field; content-identical state commits seconds after the merge), live replay of the adoption query returning PR #90 as merged. Linear: STA-90.
- Design intent: `plans/algorithms.md` ("no degraded path", "no patch-id heuristics"; non-GitHub remote is a v1 hard error) and `plans/stacc.md` hard problem 1 (squash-merge detection and phantom-conflict avoidance).
- `gh auth token` contract (stdout token, exit 1 when logged out, no network call, env vars win): gh manual and cli/cli source; verified empirically on gh 2.93.0.
- Precedence prior art: GitHub Copilot CLI documents env > own keychain > gh-fallback-last; git-spice offers gh-based auth alongside its own device flow; jjpr env-first vs stakk gh-first shows the order diverges in the wild, so following gh's own docs (env wins) is the least-surprise choice.
- Device flow requirements: OAuth App (not GitHub App) for classic `repo` scope; device flow must be enabled in app settings; client ID safe to embed (gh hardcodes its own); 50 user-code submissions/hour rate limit; no app verification needed for public OAuth Apps.
- Existing in-repo patterns: `Git::command` subprocess hardening (`crates/stacc-git/src/lib.rs`), `^{tree}` comparison (`crates/stacc/src/commands/removal.rs`), `online_repo` insteadOf trick (`crates/stacc/tests/merge.rs`), hermetic env harness (`crates/stacc/tests/sync.rs`).
