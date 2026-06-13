---
title: "GitLab as a first-class forge, the forge-equal boundary (slice 2)"
type: feat
status: active
date: 2026-06-12
origin: docs/brainstorms/2026-06-12-gitlab-forge-equal-requirements.md
---

# GitLab as a first-class forge, the forge-equal boundary (slice 2)

## Summary

Extract a forge trait from today's concrete GitHub client, add a full GitLab
implementation behind it, and move stacc's internals and CLI JSON to a
forge-neutral change model so an agent drives any forge with one schema while
humans still read "PR" or "MR". gitlab.com is the build target; self-managed
GitLab is a designed-in config hook, not exercised. The slice reaches parity:
open merge requests, squash-merge them, enrich `log`/`info`, and detect merged
MRs authoritatively in `sync`.

---

## Problem Frame

Every forge call today lives in a concrete `GitHub` struct
(`crates/stacc-github/src/lib.rs`) with PR-shaped methods, GitHub-only
vocabulary (`PullRequest`, `PrState`), and a host match (`parse_remote`) that
returns `None` for anything but `github.com`. There is no forge trait. Slice 1
shipped the forge-less local floor (`local` mode, the net-diff merge heuristic,
`stacc merged`) and, decisively, separated **detect** from **dispose**:
detection (heuristic OR forge API) produces a branch-name drop set, and one
shared `reconcile_with` path consumes it. Slice 2 adds a second authoritative
detector (GitLab) as a new producer feeding that same seam.

The extraction is structurally clean: research confirms **only the `stacc`
binary crate depends on `stacc-github`** (`crates/stacc-core` is deliberately
kept off it via a documented invariant), so a forge trait is purely additive to
the dependency graph. The reference implementation, git-spice, had its entire
GitLab provider added by an external contributor in essentially one commit that
implemented two interfaces plus one registration line, with zero orchestration
changes (`../git-spice/internal/forge/gitlab/`) -- strong evidence the boundary
is real.

The genuine difficulty is not re-pointing calls at a new host; it is the neutral
status contract. GitHub and GitLab express merge-readiness, review, and CI
through different APIs, and the slice must flatten them into one model an agent
can consume without ever branching on forge.

---

## Requirements

Requirements R1-R15 are carried from the origin requirements doc
(`docs/brainstorms/2026-06-12-gitlab-forge-equal-requirements.md`); R16-R19 are
introduced by this plan to capture decisions resolved during planning (a
multi-model review and a flow/edge-case pass). IDs are stable. Within the
carried set, R3 additionally states a plan-time clarification (the
host/override-conflict hard error), and R8/R10/R11 are refined to carry a coarse
readiness signal (see KTD3); these refinements were validated in review.

**Forge boundary and selection**

- R1. A forge boundary (a trait extracted from the current `GitHub` struct)
  defines the contract every forge implements: open a change, read its state,
  merge it, enrich it, and detect merges. GitHub and GitLab both implement it
  fully.
- R2. Forge selection is by remote-URL host (for example `gitlab.com` selects
  GitLab), generalizing the GitHub-only host match in `parse_remote`.
- R3. A `.stacc.toml` config key overrides forge selection for hosts that
  host-matching cannot identify. For a self-managed instance the override
  carries both the forge kind and the base URL. A config override that
  contradicts an identifiable host (override says GitLab, remote is github.com)
  is a hard error, not a silent send.
- R4. The forge carries a `base_url` from the outset, so a non-default GitLab
  host is a configuration value rather than a code change. gitlab.com is the
  only host slice 2 must work against.

**Neutral change model and agent-facing contract**

- R5. The internal change vocabulary is forge-neutral (a `Change` with a neutral
  `ChangeState`), replacing the GitHub-specific `PullRequest` / `PrState` types.
- R6. CLI JSON output uses forge-neutral field names through one centralized
  serialization, so an agent consumes one schema regardless of forge. The rename
  away from PR-shaped JSON is a one-time breaking change, accepted in this slice;
  no alias window. The on-disk state format (`refs/stacc/data`) is NOT migrated.
- R7. Human-readable output stays forge-appropriate: "PR" for GitHub, "MR" for
  GitLab.
- R8. The neutral status model reconciles both forges into one display model
  (`ChangeState` + `ChecksState` + `ReviewState` + a coarse, display-only
  `MergeReadiness`) and one coarse merge predicate. `ChecksState` includes a
  `NoChecks` state so the gate can distinguish "no CI configured" from "passed".
  `ReviewState` and `MergeReadiness` are display-only and do not gate the merge;
  the current `ready()` string test (`mergeable_state == "clean"`) is replaced.
- R16. A blocked merge is surfaced as a neutral structured `MergeRejectionReason`
  (conflict / behind / blocked / needs-approval / draft / unknown), derived from
  the forge's **structured** readiness fields read at merge time (GitHub
  `mergeable_state`, GitLab `detailed_merge_status`), i.e. from the same source as
  `MergeReadiness` (R8), not from parsing the forge's free-text rejection body.
  This closes the rejection-path gap R8 leaves: R8 defines the readiness an agent
  reads; R16 defines what an agent sees when a merge is blocked. The agent is
  never left with an opaque rejection.
- R18. CLI JSON carries a `schema_version`. The error envelope uses neutral
  structured type/reason codes (no `forge` discriminator and no per-forge
  branching key); the raw forge HTTP response body is never echoed.
- R19. `ReviewState` is honest about forge capability: a forge that cannot
  express a state (GitLab has no "changes requested") signals that via the
  forge's `capabilities()`, so absence is never misread as "no objections".

**GitLab parity surface**

- R9. `submit` opens merge requests for the stack on GitLab, with the same stack
  semantics as GitHub PRs (each change's base is its parent branch).
- R10. `merge` squash-merges a GitLab MR through the API, honoring GitLab's
  merge settings (squash, readiness) and the head-SHA assertion.
- R11. `log` and `info` enrich stack entries with GitLab MR state (open, merged,
  closed, draft, plus review/approval and pipeline status) through the neutral
  model, with no regression to today's GitHub `log`/`info` output. The coarse
  `MergeReadiness` carries today's GitHub `behind`/`dirty`/`blocked` hint so the
  human surface is preserved (R15).
- R12. Forge-authoritative merge detection works for GitLab: `sync` reconciles
  merged MRs from GitLab's API. The slice-1 forge-less heuristic remains the
  fallback for the no-forge case and for an unreachable forge.
- R17. The autonomous merge gate refuses to merge a change whose repo has neither
  CI nor branch protection unless `--allow-no-ci` (or protection) is present,
  preserving today's merge-safety bar for CI-less repos.

**Auth**

- R13. GitLab auth resolves a token through a ladder mirroring GitHub's: an env
  token (`GITLAB_TOKEN`), the OS keychain (`stacc auth login` stores a PAT, in a
  per-host slot that does not clobber the existing GitHub slot), then a
  `glab auth token --hostname gitlab.com` reuse fallback gated to gitlab.com.
- R14. No GitLab OAuth application or forge app is required to complete the
  submit/merge/detect loop. Required PAT scope is `api` (GitLab's broad
  read/write scope); the `auth login` help text notes this, mirroring the
  existing GitHub least-privilege note.

**Migration and compatibility**

- R15. GitHub behavior at the human surface is unchanged: GitHub users still see
  "PR" and equivalent command output, and the `behind`/`dirty`/`blocked` hint is
  preserved via `MergeReadiness` (R11). The merge gate's reported stop reasons
  shift in shape (see KTD3) but remain actionable. One deliberate GitHub-affecting
  change: the new `no_ci_unprotected` refusal (R17) will stop a merge that
  succeeds today on a CI-less, unprotected repo. This is announced in the README
  "For agents" contract and the changelog (U5) so existing GitHub users learn
  about `--allow-no-ci` before their first surprised refusal. Otherwise only JSON
  field names and the error envelope neutralize.

---

## Key Technical Decisions

- **KTD1. A new `stacc-forge` crate holds the trait and neutral types; a new
  `stacc-gitlab` crate holds the GitLab impl.** Research confirmed only the
  `stacc` binary depends on `stacc-github`, and `stacc-core` is deliberately
  kept off the forge client. Putting the trait in `stacc-core` would force a
  `stacc-github -> stacc-core` edge and pull the stack engine into the client's
  build graph, violating the documented "core speaks only git and state"
  invariant. A leaf-ish `stacc-forge` crate keeps each impl lean, gives the CLI
  one neutral-vocabulary home, and adds only additive edges
  (`stacc-github -> stacc-forge`, `stacc-gitlab -> stacc-forge`,
  `stacc -> {stacc-forge, stacc-gitlab}`).

- **KTD2. A single neutral `Forge` operations trait plus a pure host-selection
  free function, not git-spice's `Forge`/`Repository` split.** git-spice splits
  so its `Forge` can (de)serialize opaque provider change IDs from local state
  without auth or network; stacc stores plain `{number, url}`, so it lacks that
  driver. Selection is a pure URL-parsing concern with no auth, so it is a free
  function returning a `ForgeSelection`, separate from construction. The trait
  carries a single `capabilities()` accessor returning a minimal struct (slice 2
  needs only one field, `expresses_changes_requested`; it grows to a richer
  struct when a second capability has a real consumer) rather than
  runtime-downcast capability interfaces. Degraded forges (Bitbucket/Gerrit)
  later return a structured `Unsupported` error and declare reduced capabilities.
  The crates are internal to the workspace (the only external contract is the CLI
  JSON, handled separately), so a later split is cheap and carries no semver
  cost. (Considered and deferred: a minimal `ForgeProvider`/`ForgeRepo` split;
  rejected for slice 2 because the testability win is small once selection is
  already a pure function and construction is a single `from_env` constructor.)

- **KTD3. Status model is "rich display, simple gate" with a coarse readiness
  signal, structured rejection reasons, and a no-CI guard.** The neutral model
  carries `ChangeState`, `ChecksState` (including a `NoChecks` state), and
  `ReviewState` plus a coarse, **display-only** `MergeReadiness`
  {Ready, Conflicted, Behind, Blocked, NeedsApproval, Draft, Unknown}. The first
  three preserve today's GitHub `log`/`info` signal; `MergeReadiness` carries the
  `behind`/`dirty`/`blocked` hint that would otherwise regress (R11/R15) and is
  the single source for `MergeRejectionReason` (read at merge time from GitHub
  `mergeable_state` / GitLab `detailed_merge_status`, not parsed from a free-text
  rejection body). The merge gate stays coarse and consumes **only** state +
  checks: refuse on not-open, checks-failed, checks-pending (after a bounded poll),
  and no-CI-and-unprotected unless `--allow-no-ci`; `MergeReadiness` and review do
  not gate. Distinguishing "no CI configured" from "pipeline not yet created"
  matters: a freshly-pushed MR with `head_pipeline == nil` maps to `Pending` and
  is re-polled; only an exhausted poll yields `NoChecks` (which feeds the no-CI
  guard). The gate then attempts the merge and surfaces the structured
  `MergeRejectionReason` (R16) on a block. This deliberately does **not**
  faithfully gate on every `mergeable_state`/`detailed_merge_status` value (the
  origin's named main risk) -- the coarse readiness is for display and rejection
  reporting, not gating. (Considered and rejected: full faithful readiness gating,
  too much iteration surface; lean git-spice parity with no readiness signal,
  regresses GitHub `log`/`info` and leaves rejections opaque.)

- **KTD4. The neutral error envelope drops the forge discriminator.** Today the
  error JSON is `{"error":"github", ...}` and embeds the raw response body.
  Neutralizing to a forge-named discriminator (`forge:"gitlab"|"github"`) would
  invite `if error.forge == "gitlab"`, contradicting the never-branch-on-forge
  goal. Instead the envelope uses neutral structured codes (e.g.
  `type:"forge_auth" | "forge_rejected" | "conflict" | "not_found" |
  "rate_limited"`) plus `schema_version`; forge identity may appear as optional
  debug context but is never the branching key. The raw body is scrubbed; a safe
  structured reason is extracted first (this is both a security fix and what
  keeps an agent un-blinded after R16). This is a second breaking change folded
  into the same accepted one-time break (R18). Credential stripping in host
  parsing (U3) and forge-response-body scrubbing (U4) are security invariants
  that land with this work, not separable cleanup; both are enforced by tests.

- **KTD5. The GitLab project identifier is an opaque, URL-encoded full path,
  wrapped in a newtype.** GitLab group paths are multi-segment
  (`group/subgroup/repo`), so the current `(owner, repo)` split does not
  generalize. The neutral side carries a single `ProjectPath` newtype
  (URL-encoded full path) to prevent double-encoding bugs; GitHub derives
  `owner/repo` internally.

- **KTD6. Per-host keychain slots, preserving the existing GitHub slot.** Today
  storage is hard-keyed to `user="github.com"`. Generalize to a per-host slot so
  GitLab tokens live beside (not on top of) the GitHub token; keep github.com's
  existing slot value so stored GitHub tokens are not invalidated. `auth login`
  gains `--host`, `logout` clears the named host only, `auth status` reports
  per-host rows.

- **KTD7. stacc-created GitLab MRs set `remove_source_branch=false`, hardcoded.**
  GitLab's default of deleting the source branch on merge would orphan the rest
  of the stack and break source-branch-query merge detection (the squash phantom
  class hardened against in STA-90). stacc owns branch lifecycle (keep-alive
  refs), so it must not request source-branch removal, and this is not
  agent-overridable.

- **KTD8. The `.stacc.toml` forge override follows the slice-1 `local`
  precedent.** A `[forge]` table (`kind`, `base_url`) read per-invocation
  (file-only, like `aliases_from_file`/`local_from_file`), not threaded into
  persisted `RepoConfig`. `stacc-config` depends only on `stacc-git`, so no new
  edge. This is the second deliberate widening of the previously-closed `Key`
  namespace.

- **KTD9. Draft state via the `Draft:` title prefix, create-only, with an
  idempotency guard.** GitLab sets draft through the title prefix (the
  `draft`/`work_in_progress` API field is read-only/derived), matching the
  git-spice reference. `submit --draft` sets it at create only (matching today's
  GitHub `draft`); a regex guard prevents double-prefixing when the title is
  recomputed from the commit subject. (Implementation should confirm the current
  GitLab API behavior; the reference evidence favors the prefix approach.)

---

## High-Level Technical Design

### Crate topology and forge selection

The remote host (or a `.stacc.toml` override) selects a forge through a pure
function; both impls satisfy one trait; the neutral model feeds one JSON schema
plus forge-specific human labels.

```mermaid
flowchart TB
  R[remote URL] --> SEL["select_forge() (pure)"]
  CFG[".stacc.toml [forge] override"] --> SEL
  SEL -->|github.com| GH[stacc-github: impl Forge]
  SEL -->|gitlab.com| GL[stacc-gitlab: impl Forge]
  SEL -->|override + base_url| GL
  SEL -->|unknown host| FL[forge-less floor, slice 1]
  SEL -->|override vs host conflict| ERR[hard error]
  GH --> T[stacc-forge: Forge trait + neutral model]
  GL --> T
  T --> J["neutral JSON (one schema + schema_version)"]
  T --> H["human labels: PR / MR"]
```

Dependency edges are additive: `stacc-github -> stacc-forge`,
`stacc-gitlab -> stacc-forge`, `stacc -> {stacc-forge, stacc-gitlab}`.
`stacc-core` stays off every forge crate.

### Neutral status mapping

Two different forge APIs flatten into one display model. The merge gate consumes
only `ChangeState` + `ChecksState`; `ReviewState` and `MergeReadiness` are
display-only, and `MergeReadiness` doubles as the source for the rejection reason.

```mermaid
flowchart TB
  subgraph GitHub
    A1[PR state: open/closed/merged+merged_at]
    A2[reviewDecision]
    A3[statusCheckRollup]
    A4[mergeable_state]
  end
  subgraph GitLab
    B1[MR state: opened/closed/merged]
    B2[approvals API]
    B3[head_pipeline.status]
    B4[detailed_merge_status]
  end
  A1 --> CS[ChangeState]
  B1 --> CS
  A2 --> RS["ReviewState (+ capabilities)"]
  B2 --> RS
  A3 --> CK["ChecksState (+ NoChecks)"]
  B3 --> CK
  A4 --> MR["MergeReadiness (display-only)"]
  B4 --> MR
  CS --> DISP["display: log / info / JSON"]
  RS --> DISP
  CK --> DISP
  MR --> DISP
  MR --> RJ["MergeRejectionReason (read at merge time)"]
  CS --> GATE[merge gate]
  CK --> GATE
```

### Merge gate

The gate reads only `ChangeState` and `ChecksState`. `NoChecks` (no CI
configured, after the bounded poll) triggers the no-CI guard; review and
`MergeReadiness` are display-only and never gate.

```mermaid
flowchart TB
  S{state == open?} -->|no| X1[stop: not_open]
  S -->|yes| C{ChecksState}
  C -->|Failed| X2[stop: checks_failed]
  C -->|Pending after bounded poll| X3[stop: checks_pending]
  C -->|NoChecks| N{protected OR --allow-no-ci?}
  N -->|no| X4[stop: no_ci_unprotected]
  N -->|yes| M[attempt merge]
  C -->|Passed| M
  M -->|ok| OK[merged]
  M -->|blocked| RR["MergeRejectionReason from readiness\n{conflict|behind|blocked|\nneeds_approval|draft|unknown}"]
```

### Sync detection: the fourth classifier branch

```mermaid
flowchart TB
  A[sync] --> B{offline or no tracked branches?}
  B -->|yes| Z[skip detection, note]
  B -->|no| C{local-mode key?}
  C -->|yes| H[heuristic, no note]
  C -->|no| D["select_forge(remote)"]
  D -->|github.com reachable| E[GitHub authoritative]
  D -->|gitlab.com reachable| G["GitLab authoritative (new)"]
  D -->|unknown / forge-less| H2[heuristic, with note]
  E -->|unreachable| H2
  G -->|unreachable| H2
  E --> DROP[drop set]
  G --> DROP
  H --> DROP
  H2 --> LIKELY[likely-merged, propose-only]
  DROP --> RW["reconcile_with (shared dispose path)"]
```

---

## Output Structure

Two new crates. Per-unit `Files` sections remain authoritative; this tree is the
expected shape, adjustable during implementation.

```text
crates/
  stacc-forge/            # NEW: the boundary (no network, no auth)
    Cargo.toml
    src/
      lib.rs              # Forge trait, re-exports
      model.rs            # Change, ChangeState, ChecksState (+ NoChecks),
                          #   ReviewState, MergeReadiness, ChangeStatus,
                          #   MergeOptions, MergeRejectionReason
      select.rs           # select_forge(), ForgeKind, ForgeSelection, ProjectPath
      capability.rs       # Capabilities struct
      error.rs            # ForgeError + neutral error type/reason codes
  stacc-gitlab/           # NEW: GitLab REST v4 impl
    Cargo.toml
    src/
      lib.rs              # GitLab struct: impl Forge
      client.rs          # transport (PRIVATE-TOKEN, base_url)
      auth.rs             # token ladder, per-host keychain, glab reuse
      submit.rs           # create/update MR, Draft: prefix
      merge.rs            # accept MR, MergeRejectionReason parsing
      state.rs            # state/checks/review mapping
      error.rs            # GitLab error -> ForgeError, body scrub
  stacc-github/           # refactored: impl Forge (was the concrete client)
  stacc-config/           # extended: [forge] override key
  stacc/                  # rewired: select + dyn Forge, neutral JSON, gate
```

---

## Implementation Units

Units are grouped into four phases. Phase A is behavior-preserving; the
agent-facing break lands in Phase B; GitLab parity in Phase C; verification in
Phase D. Dependencies cite U-IDs.

### Phase A: Forge boundary and neutral model (no behavior change)

### U1. The `stacc-forge` crate: neutral model and `Forge` trait

- **Goal:** Define the neutral vocabulary and the trait every forge implements.
  No implementations yet.
- **Requirements:** R1, R5, R8, R16, R18, R19.
- **Dependencies:** none.
- **Files:** `crates/stacc-forge/Cargo.toml`, `crates/stacc-forge/src/lib.rs`,
  `crates/stacc-forge/src/model.rs`, `crates/stacc-forge/src/capability.rs`,
  `crates/stacc-forge/src/error.rs`, `Cargo.toml` (workspace member).
- **Approach:** Define `ChangeState {Open, Merged, Closed}`,
  `ChecksState {Pending, Passed, Failed, NoChecks}` (`NoChecks` = no CI
  configured, distinct from `Passed` so the gate can detect it),
  `ReviewState {Approved, ChangesRequested, ReviewRequired, NoReview}`, a
  display-only `MergeReadiness {Ready, Conflicted, Behind, Blocked, NeedsApproval,
  Draft, Unknown}`, a `Change` / `ChangeStatus` carrying
  `{number, url, state, checks, review, readiness, draft, ...}`, `SubmitChange`
  request/result, `MergeOptions {squash, head_sha}`,
  `MergeRejectionReason {Conflict, Behind, Blocked, NeedsApproval, Draft, Unknown}`,
  a minimal `Capabilities {expresses_changes_requested: bool}` (one field for
  slice 2; grows only when a second capability has a consumer), and `ForgeError`
  with neutral type/reason codes plus a `SCHEMA_VERSION` constant. `ReviewState`
  and `MergeReadiness` are display-only; the gate consumes only state + checks.
  The `Forge` trait method set mirrors today's GitHub surface, neutralized:
  `current_user`, `create_change`, `update_change`, `get_change`,
  `change_for_branch` (+ any-state / budgeted variants), `merge_change`,
  `change_checks`, `branch_protected`, `close_change`, `capabilities`. Methods
  return `Result<_, ForgeError>`; unsupported operations (e.g. branch rename on
  GitLab) return a structured `ForgeError::Unsupported`. The neutral serde field
  names are decided here (used by U5).
- **Patterns to follow:** the existing enum + `thiserror` shape in
  `crates/stacc-github/src/lib.rs` and `crates/stacc-github/src/error.rs`; serde
  derives as on `crates/stacc-state/src/model.rs`.
- **Test scenarios:**
  - `ChangeState`/`ChecksState`/`ReviewState`/`MergeReadiness` serialize to the
    agreed neutral strings (e.g. `open`/`merged`/`closed`,
    `pending`/`passed`/`failed`/`no_checks`).
  - `ChecksState::NoChecks` is a distinct value from `Passed` (the gate's no-CI
    branch depends on this distinction).
  - `MergeRejectionReason` and `ForgeError` round-trip serialize with the neutral
    type/reason codes and a `schema_version` field present.
  - `Capabilities` defaults are explicit (no accidental `true`).
  - `ProjectPath` (added in U3) is referenced by trait signatures, confirm the
    trait compiles against it.
  - Test expectation: type-and-serialization level; no network.
- **Verification:** crate builds; `cargo clippy --workspace --all-targets` clean;
  serialization unit tests pass.

### U2. `.stacc.toml` forge-override config key

- **Goal:** Let `.stacc.toml` name a forge kind and base URL for hosts that
  host-matching cannot identify.
- **Requirements:** R3, R4.
- **Dependencies:** none (can land in parallel with U1).
- **Files:** `crates/stacc-config/src/lib.rs` (and its tests).
- **Approach:** Add a `[forge]` table (`kind`, `base_url`) to the `Overrides`
  parse struct and a per-invocation reader mirroring `aliases_from_file` /
  `local_from_file` (repo-over-global precedence via the `resolve_local` shape).
  Extend the `Key` enum, `Key::parse`, `Display`, `set_in_file`, `unset_in_file`,
  and the namespace-closure test. Forge selection reads this at the command
  boundary, not from persisted `RepoConfig`.
- **Patterns to follow:** the slice-1 `Key::Local` widening and
  `local_from_file` (`crates/stacc-config/src/lib.rs`); `toml_edit`
  format-preserving set/unset arms.
- **Test scenarios:**
  - `[forge] kind = "gitlab"` parses; unknown kind errors with `InvalidValue`.
  - `base_url` parses and round-trips through set/unset preserving file format.
  - Repo-level override beats global config.
  - Missing `[forge]` table yields `None`, no error.
  - Namespace-closure test includes the new key(s).
- **Verification:** config unit tests pass; clippy clean.

### U3. Host-based forge selection (pure function)

- **Goal:** Replace the GitHub-only `parse_remote` with a pure
  `select_forge(remote_url, override) -> ForgeSelection`.
- **Requirements:** R2, R3, R4, R16 (hard-error path), KTD5.
- **Dependencies:** U1 (types), U2 (override).
- **Files:** `crates/stacc-forge/src/select.rs` (and tests). Removes the
  github.com-only `parse_remote` from `crates/stacc-github/src/lib.rs` (or
  re-expresses it as a GitHub-internal helper).
- **Approach:** Classify host -> `ForgeKind {GitHub, GitLab, Unknown}`; extract a
  `ProjectPath` (URL-encoded full path, multi-segment for GitLab groups); carry
  `base_url`. Strip credentials from `https://user:token@host/...` without ever
  echoing them; handle `git@host:` SSH, trailing `.git`/slash, and GitLab `/-/`
  web segments. Apply the override; an override that contradicts an identifiable
  host is `ForgeError`-level hard error naming both. Unknown host returns the
  forge-less signal.
- **Patterns to follow:** the credential-leak gating intent of `is_github_dot_com`
  (`crates/stacc-github/src/lib.rs`); the existing
  `parse_remote_handles_https_and_ssh` fixture-test style.
- **Test scenarios:**
  - github.com https + ssh -> GitHub, correct path.
  - gitlab.com https + ssh -> GitLab, correct path.
  - `git@gitlab.com:group/sub/repo.git` -> GitLab, URL-encoded multi-segment path.
  - `https://oauth2:TOKEN@gitlab.com/g/r.git` -> classifies; assert the token
    never appears in the result or any error string.
  - Unknown host -> forge-less.
  - Override agrees with host -> selects; override contradicts host -> hard error
    naming both.
  - Self-managed base_url present with `kind=gitlab` -> GitLab against that base.
- **Verification:** fixture-table tests pass; a test asserts no token/raw-URL
  leakage; clippy clean.

### U4. GitHub implements `Forge` (internal neutral adapter)

- **Goal:** Make the existing GitHub client satisfy the trait. Behavior-preserving
  for the JSON field names and the gate (those change in U5/U6), **except** the
  R16/R18 error-body scrub, which lands here atomically (see below) rather than
  opening a leak window before U5.
- **Requirements:** R1, R5, R8, R11 (GitHub side), R16 (GitHub rejection source),
  R18 (body scrub, GitHub), R19 (capabilities).
- **Dependencies:** U1.
- **Files:** `crates/stacc-github/src/lib.rs`,
  `crates/stacc-github/src/error.rs`, `crates/stacc/src/error.rs` (the
  `Error::Github` arm of `as_json`, scrubbed atomically with the type change),
  `crates/stacc-github/Cargo.toml` (add `stacc-forge` dep).
- **Approach:** Map `PrState` -> `ChangeState` (preserving the `merged ||
  merged_at` quirk), `ReviewDecision` -> `ReviewState`, `statusCheckRollup` ->
  `ChecksState` (no rollup / no required checks -> `NoChecks`, not `Passed`), and
  `mergeable_state` -> the display-only `MergeReadiness`
  (`clean` -> Ready; `dirty` -> Conflicted; `behind` -> Behind; `blocked` ->
  Blocked; `unknown`/absent -> Unknown). Implement `capabilities()` with
  `expresses_changes_requested = true`. Derive `MergeRejectionReason` from the
  structured `MergeReadiness` at merge time (not from the rejection body). Scrub
  the raw response body from `GitHubError::Status`, extracting a safe structured
  reason first, **and** update the `Error::Github` arm of `as_json` in the same
  unit so no leak window exists. Keep the REST + single-GraphQL internals. Apart
  from the error-body scrub, existing `stacc-github` and CLI tests pass unchanged
  (the CLI still constructs GitHub directly and still emits the old JSON field
  names until U5/U6).
- **Patterns to follow:** existing `From<RawPullRequest>` mapping and the
  `pull_request_checks_within` GraphQL parse; the "token never in error" intent
  of `gh_token`.
- **Test scenarios:**
  - Covers R8. `statusCheckRollup` maps to the right `ChecksState`, with no
    rollup / no required checks mapping to `NoChecks` (not `Passed`);
    `reviewDecision` maps to the right `ReviewState`; `mergeable_state` maps to the
    right `MergeReadiness` (`dirty` -> Conflicted, `behind` -> Behind, `blocked`
    -> Blocked, `clean` -> Ready).
  - The `merged`/`merged_at` dual-signal still resolves `Merged` for both the
    single-PR and list endpoints (regression).
  - A blocked merge derives `MergeRejectionReason::Conflict` (or `Blocked`) from
    `MergeReadiness`, and the raw body is absent from the error.
  - Covers R18. A 403 whose body carries a token-bearing string never surfaces the
    token in either the error `Display` or the `as_json` `message`.
  - Pre-existing tests pass unchanged except error-shape assertions that quoted the
    raw body, which are updated to assert the scrubbed structured reason.
- **Execution note:** Add characterization coverage for the body-scrub and
  rejection-derivation paths before changing the error type.
- **Verification:** full `cargo test --workspace` green (only error-body
  assertions edited); clippy clean.

### Phase B: Agent-facing contract

### U5. Neutral JSON output and error envelope (the one-time break)

- **Goal:** Flip all CLI JSON to neutral field names through shared neutral
  serialization, add `schema_version`, neutralize the error envelope, and
  re-document the agent contract so consumers learn the v2 schema (there is no
  alias window to cushion a stale doc).
- **Requirements:** R6, R7, R15 (no-CI announcement), R18.
- **Dependencies:** U4.
- **Files:** `crates/stacc/src/commands.rs`,
  `crates/stacc/src/commands/operations.rs`,
  `crates/stacc/src/commands/info.rs`, `crates/stacc/src/commands/log.rs`,
  `crates/stacc/src/error.rs`, `README.md` (the "For agents" stable-contract
  section, plus a changelog entry for the break and the new `no_ci_unprotected`
  refusal); integration tests under `crates/stacc/tests/` (`submit.rs`,
  `merge.rs`, `merged.rs`, `sync.rs`, `info.rs`, `status.rs`, `log.rs`, `pr.rs`).
- **Approach:** Route every `serde_json::json!` PR-shaped emit site through shared
  neutral serialization. The emit sites are heterogeneous (each command emits a
  different field subset, several embedding the dropped `mergeable_state`), so
  this is per-site reconciliation onto the neutral `Change`/`ChangeStatus` fields,
  not a single `serde` derive -- but the rename lands in one unit so the break is
  atomic, not dribbled. Add `schema_version`: a present value means the versioned
  v2 schema; an absent field means legacy/untrusted output (pre-versioning), since
  v1 never emitted the field -- consumers treat field-absent as "unrecognized",
  not as a valid version. Complete the `Error::Forge` envelope (the `Error::Github`
  arm was already scrubbed in U4): neutral structured `type`/`reason` codes (KTD4),
  `miette` diagnostic + `as_json`. Human label stays "PR" for GitHub. Re-document
  the README "For agents" section (it currently tells agents to branch on
  `error == "github"` and read a `pr` field) to the neutral schema, the new error
  codes, and `schema_version`. Update the golden assertions in the integration
  tests.
- **Patterns to follow:** the existing `report_*` `json!` builders and
  `error.rs::as_json`; the `--format json|pretty` contract.
- **Test scenarios:**
  - Golden JSON for each command (`submit`, `merge`, `sync`, `log`, `info`,
    `status`, `pr`) shows neutral field names + `schema_version`.
  - Covers R18. The error envelope for an auth failure is
    `{type:"forge_auth", ...}` with no `forge` discriminator and no raw body.
  - A merge stop carries the neutral `MergeRejectionReason`, not GitHub
    `mergeable_state`.
  - Human (`pretty`) output still says "PR" for a GitHub repo (R7/R15).
  - The README "For agents" examples match the emitted v2 schema (no stale `pr` /
    `error == "github"` references remain).
- **Verification:** integration tests updated and green; a reviewer can diff the
  golden files to see the entire break in one place; the README contract matches
  the emitted schema.

### U6. CLI wiring to `dyn Forge`, selection, and human labels

- **Goal:** Stop constructing GitHub directly; route every forge-touching command
  through `select_forge` + a per-forge constructor. (Gate rework is U11.)
- **Requirements:** R2, R7.
- **Dependencies:** U3, U4, U5.
- **Files:** `crates/stacc/src/commands/operations.rs`,
  `crates/stacc/src/commands.rs`, `crates/stacc/src/commands/info.rs`,
  `crates/stacc/src/commands/log.rs`, `crates/stacc/src/cli.rs`.
- **Approach:** Replace `require_github_forge` / `github_client` / `build_client`
  with selection + `Box<dyn Forge>` construction (`from_env` per kind). Thread the
  forge through `submit`, `merge`, `log`, `info`, `sync`. Derive the human label
  ("PR"/"MR") from `ForgeKind`. Surface the host/override-mismatch hard error at
  the command boundary. The merge command still calls the forge as it does today;
  the gate logic is reworked in U11 so wiring and gate-behavior failures stay
  independently reviewable.
- **Patterns to follow:** `reconcile_detection` and `merge_stack` in
  `crates/stacc/src/commands/operations.rs`; the `--offline` flag plumbing in
  `crates/stacc/src/cli.rs`.
- **Test scenarios:**
  - A github.com remote routes to the GitHub forge; a gitlab.com remote routes to
    GitLab (with U7 present); unknown host routes forge-less.
  - Human output uses "PR" for GitHub, "MR" for GitLab.
  - `--offline` skips fetch/detect regardless of forge (regression).
  - A config override that contradicts an identifiable host hard-errors at the
    command boundary (naming both).
- **Verification:** submit/log/info/sync integration tests green; clippy clean.

### U11. Merge-gate rework (checks gate, no-CI guard, rejection reasons)

- **Goal:** Replace the `mergeable_state` gate with the coarse checks gate, the
  no-CI guard, and structured rejection reasons.
- **Requirements:** R8, R16, R17, R15 (gate-shape delta).
- **Dependencies:** U6.
- **Files:** `crates/stacc/src/commands/operations.rs` (`merge_stack`,
  `poll_pr_ready`), `crates/stacc/src/cli.rs` (`--allow-no-ci`).
- **Approach:** Rework the gate per KTD3. A bounded checks-poll (replacing
  `poll_pr_ready`) re-polls `Pending` -- including a freshly-pushed MR whose
  pipeline has not yet been created (`head_pipeline == nil` reads as `Pending`,
  not `NoChecks`, until the poll is exhausted) -- and yields `stop: checks_pending`
  on timeout. `stop: not_open` / `stop: checks_failed` as shown in the gate
  diagram. The `no_ci_unprotected` guard fires on `NoChecks` AND not protected AND
  not `--allow-no-ci` (a new flag). Otherwise attempt the merge and, on a block,
  surface a `MergeRejectionReason` derived from `MergeReadiness`. Keep the existing
  unprotected-trunk warning and `--require-protected`.
- **Patterns to follow:** `merge_stack` and `poll_pr_ready` in
  `crates/stacc/src/commands/operations.rs`.
- **Test scenarios:**
  - Covers R17. A repo whose checks read `NoChecks` with no protection stops with
    `no_ci_unprotected` unless `--allow-no-ci` (or protected).
  - A freshly-pushed MR with no pipeline yet re-polls as `Pending`, not `NoChecks`
    (it does not falsely trip the no-CI guard).
  - Checks pending past the bound -> `checks_pending`; checks failed ->
    `checks_failed`.
  - A blocked merge surfaces a neutral `MergeRejectionReason` derived from
    readiness (GitHub path).
- **Execution note:** Start with a failing integration test for the gate
  truth-table before reworking `merge_stack`.
- **Verification:** merge integration tests green; the gate truth-table is covered.

### Phase C: GitLab parity

### U7. `stacc-gitlab` forge implementation

- **Goal:** Implement `Forge` against GitLab REST v4 for the full parity surface.
- **Requirements:** R8, R9, R10, R11, R12 (detection primitive), R16, R19, KTD5,
  KTD7, KTD9.
- **Dependencies:** U1 (trait); integrates via U6 routing.
- **Files:** `crates/stacc-gitlab/Cargo.toml`, `crates/stacc-gitlab/src/lib.rs`,
  `crates/stacc-gitlab/src/client.rs`, `crates/stacc-gitlab/src/submit.rs`,
  `crates/stacc-gitlab/src/merge.rs`, `crates/stacc-gitlab/src/state.rs`,
  `crates/stacc-gitlab/src/error.rs`, `Cargo.toml` (workspace member).
- **Approach:** A thin REST v4 client (PRIVATE-TOKEN header, `base_url`),
  mirroring the git-spice gateway endpoint map:
  - project resolution: at construction, resolve the `ProjectPath` to the project
    resource (`GET projects/{encoded_path}`), caching the numeric id and the
    `current_user` (`GET /user`); the cross-project out-of-scope guard reads
    `source_project_id`/`target_project_id` from this resource;
  - create MR: `POST projects/{id}/merge_requests` with
    `target_branch = parent`, `target_project_id`, `remove_source_branch = false`
    (hardcoded), `Draft:` title prefix when drafting (idempotent strip on update);
  - merge: `PUT projects/{id}/merge_requests/{iid}/merge` with `squash = true`
    and the head `sha` assertion;
  - state: `opened`/`closed`/`merged` -> `ChangeState`;
  - checks: `head_pipeline.status` -> `ChecksState` (`success`/`skipped` ->
    Passed; `failed`/`canceled` -> Failed; other non-terminal -> Pending; absent
    pipeline -> `NoChecks`; the transient "pushed, pipeline not yet created" case
    is handled by U11's poll, which treats a fresh `nil` as `Pending`);
  - readiness: `detailed_merge_status` -> the display-only `MergeReadiness` and the
    source for `MergeRejectionReason` (read at merge time): e.g.
    `discussions_not_resolved`/`not_approved` -> Blocked/NeedsApproval,
    `conflict`/`broken_status` -> Conflicted, `need_rebase` -> Behind,
    `draft_status` -> Draft, `mergeable` -> Ready, else Unknown;
  - review: `GET projects/{id}/merge_requests/{iid}/approvals` ->
    `Approved`/`ReviewRequired`/`NoReview`; `capabilities().expresses_changes_requested
    = false`;
  - merge detection primitive: `GET .../merge_requests?source_branch=<b>`
    filtered by source project, `state == merged`; read back the created MR's
    `force_remove_source_branch` and, if the project forces source-branch deletion
    despite our `remove_source_branch=false`, fall back to IID-based detection and
    warn that the project setting is incompatible with keep-alive (KTD7);
  - `branch_protected`: `GET projects/{id}/protected_branches/{name}` (404 ->
    false);
  - `MergeRejectionReason` is derived from `detailed_merge_status` (above), not
    parsed from the rejection body; the body is scrubbed;
  - `rename_branch` -> `ForgeError::Unsupported` (not in slice-2 parity R-set).
- **Patterns to follow:** `../git-spice/internal/forge/gitlab/` and
  `../git-spice/internal/gateway/gitlab/{api,client}.go` for the exact endpoints
  and wire shapes; `crates/stacc-github/src/lib.rs` for the `ureq` client shape.
- **Test scenarios (against an httpmock GitLab server, mirroring the GitHub test
  style):**
  - Covers R9. `submit` creates one MR per branch with `target_branch` = parent;
    `--draft` prepends `Draft:`; a re-submit does not double-prefix.
  - Covers R10. `merge` sends `squash=true` + `sha`; a moved head is rejected and
    surfaces `MergeRejectionReason`.
  - Covers R8/R11. `head_pipeline.status` maps to each `ChecksState`; absent
    pipeline -> `NoChecks`; `detailed_merge_status` maps to `MergeReadiness`
    (conflict -> Conflicted, not_approved -> NeedsApproval, etc.); approvals map to
    `Approved`/`ReviewRequired`/`NoReview`.
  - State mapping for `opened`/`closed`/`merged`.
  - Merge detection: a merged MR is found by source-branch query as `state ==
    merged`; with `remove_source_branch=false`, the source branch still resolves
    (the STA-90 phantom class).
  - A project that forces source-branch deletion (`force_remove_source_branch`
    true despite our `false`) falls back to IID-based detection and warns.
  - The project-resolution step caches the numeric id + `current_user`; a
    cross-project push remote vs MR target is rejected as out of scope.
  - `rename_branch` returns `Unsupported`, surfaced cleanly.
  - A multi-segment group project path is URL-encoded correctly in every endpoint.
- **Verification:** `stacc-gitlab` unit/integration tests green; clippy clean.

### U8. GitLab auth ladder and per-host keychain

- **Goal:** Resolve and store GitLab tokens without clobbering GitHub, gated to
  gitlab.com, with no credential leakage.
- **Requirements:** R13, R14, KTD6.
- **Dependencies:** U7.
- **Files:** `crates/stacc-gitlab/src/auth.rs`,
  `crates/stacc/src/commands/auth.rs` (the `login`/`logout`/`status` command
  surface), and any shared keychain helper.
- **Approach:** Ladder: `GITLAB_TOKEN` env -> per-host keychain slot -> a `glab`
  reuse fallback gated to gitlab.com. Note the `glab` mechanism differs from
  `gh`: git-spice runs `glab auth status --hostname gitlab.com --show-token` and
  regex-extracts the token from **stderr** (glab prints `✓ Token: <tok>` there),
  not `glab auth token` on stdout -- mirror the git-spice command, not the
  `run_gh_token` stdout shape, or the rung silently no-ops. Keep the bounded,
  killed-on-timeout subprocess and the token-never-in-error contract. The
  `--hostname` argument MUST be the compile-time literal `"gitlab.com"`, never
  derived from `base_url`/`ForgeSelection` (mirrors `run_gh_token`). Required
  scope `api`; PRIVATE-TOKEN header. Generalize the keychain `user` to be
  host-keyed, preserving the existing `github.com` slot value. `auth login
  --host`; `auth logout --host <h>` clears the named host only, while bare `auth
  logout` clears all stored per-host slots and reports each cleared host (so a
  GitLab token is never silently left behind); `auth status` reports per-host
  rows. Add a `STACC_GLAB_BIN` test-hook/kill-switch mirroring `STACC_GH_BIN`. A
  read-only token's write-path 403 is normalized to an actionable "token lacks
  `api` scope" message (no raw body).
- **Patterns to follow:** `crates/stacc-github/src/auth.rs` (`gh_token`,
  `run_gh_token`, keychain helpers) and `crates/stacc/src/commands/auth.rs`; the
  STA-90/94 credential-hygiene contracts.
- **Test scenarios:**
  - Env `GITLAB_TOKEN` beats keychain beats `glab`.
  - `glab` not installed / non-zero exit / no token in stderr / timeout each fall
    through cleanly; the captured token never appears in any error.
  - The token is read from `glab auth status --show-token` stderr (not assumed on
    stdout); the `--hostname` argument is the literal `"gitlab.com"`, so a
    `glab` session for a different host is not reused.
  - `glab` is gated to gitlab.com (a custom base_url never invokes it).
  - Storing a GitLab token does not overwrite an existing github.com token;
    `logout --host gitlab.com` leaves the GitHub token intact.
  - Bare `auth logout` clears all per-host slots and reports each cleared host (no
    token is silently left behind).
  - `auth status` shows per-host rows.
  - A read-only token: reads succeed, `submit`/`merge` 403 normalized to the
    scope message.
- **Verification:** auth unit tests green (hermetic via `STACC_GLAB_BIN` +
  `STACC_KEYCHAIN` disable); clippy clean.

### U9. GitLab authoritative merge detection in `sync`

- **Goal:** Add GitLab as the fourth classifier branch feeding the shared dispose
  path.
- **Requirements:** R12.
- **Dependencies:** U6 (selection routing), U7 (MR state read), U8 (an
  authenticated, reachable GitLab client is required to detect authoritatively).
- **Files:** `crates/stacc/src/commands/operations.rs` (the `reconcile_detection`
  classifier and `detect_*` producers).
- **Approach:** Extend `reconcile_detection`: a recognized, reachable gitlab.com
  forge routes into a new authoritative producer that resolves merged MRs (by
  source-branch query, `state == merged`) into the branch-name drop set consumed
  by the existing `reconcile_with`. An unreachable GitLab forge falls back to the
  slice-1 heuristic with a note (the `is_forge_unreachable` path). No new dispose
  logic -- this is a producer at the existing detect/dispose seam.
- **Patterns to follow:** the existing GitHub `detect_and_adopt` /
  `detect_merged` producer and the `Detection` enum in
  `crates/stacc/src/commands/operations.rs`; the slice-1 `reconcile_with` seam.
- **Test scenarios:**
  - Covers R12. A GitLab MR merged via the web UI is detected authoritatively;
    the branch's children restack via `reconcile_with`.
  - The same repo in forge-less mode falls back to the heuristic + `stacc merged`.
  - A merged-then-source-branch-deleted MR still resolves as authoritative-merged,
    not heuristic-fallback (STA-90 regression; relies on KTD7).
  - GitLab unreachable -> heuristic fallback with a note (not a hard error).
  - `--offline` skips detection regardless of forge.
- **Verification:** `sync` integration tests green against the GitLab mock.

### Phase D: Verification

### U10. End-to-end dogfood and cross-forge contract tests

- **Goal:** Prove the full loop on a gitlab.com mock and lock the neutral
  contract.
- **Requirements:** R6, R7, R11, R15, R18, plus the success criteria.
- **Dependencies:** U7, U8, U9.
- **Files:** `crates/stacc/tests/` (a new GitLab end-to-end test module and a
  cross-forge JSON parity test).
- **Approach:** Drive track -> submit MRs -> merge -> sync-reconcile against a
  GitLab httpmock server. Add a parity test asserting the GitHub and GitLab JSON
  outputs of the same command share field names (only the human label differs).
  Assert the host-mismatch hard error and the scrubbed-body invariant end to end.
- **Patterns to follow:** existing `crates/stacc/tests/` integration harness and
  the GitHub httpmock setup.
- **Test scenarios:**
  - Covers R6, R7. The dogfood loop completes; JSON field names are identical
    across forges; human text shows "MR" on GitLab, "PR" on GitHub.
  - Covers R18. No command leaks a raw forge body or a token in any output path.
  - The host/override-mismatch case hard-errors end to end.
  - A read-only token completes reads but fails writes with the scope message.
- **Verification:** the gitlab.com dogfood loop runs end to end on the mock; the
  cross-forge parity test passes; `cargo test --workspace` and
  `cargo clippy --workspace --all-targets` clean.

---

## Scope Boundaries

**Deferred for later (ticketed in Linear, team STA; carried from origin)**

- Self-managed GitLab base-URL support beyond the designed-in config hook
  (honored best-effort but untested in slice 2; gated so a glab/gitlab.com token
  is never sent to a custom host).
- GitLab OAuth device flow (auth parity beyond PAT plus `glab`).
- Bitbucket forge, and the opt-in capability interfaces its degradations require.
- Gerrit forge (a changeset model, a genuinely different shape).
- The forge-order decision: which non-GitHub forge follows GitLab.

**Deferred to follow-up work (plan-local)**

- Synthesizing a GitLab `ChangesRequested` from unresolved blocking discussions
  (slice 2 signals capability instead; KTD3/R19).
- A strict-predictive merge gate as an opt-in `merge.gate = strict|simple` (slice
  2 ships the simple gate + no-CI guard only).
- Branch rename on GitLab (returns `Unsupported`; not in the parity R-set).
- Cross-project / fork MRs (`source_project_id != target_project_id`): out of
  scope; a divergent push remote and MR target is a hard error.
- Transitioning an MR out of draft on update (draft is create-only; KTD9).

**Outside this product's identity (carried from origin / slice 1)**

- Push access to the shared repo is required to stack and submit; a developer who
  cannot push gets local stack management only.
- Repos that dismiss approvals when a base branch moves are fundamentally
  incompatible with restacking; the remedy is repo reconfiguration, not a stacc
  feature.

---

## Risks & Dependencies

- **Dependency: slice 1** (the forge-less floor, `local` mode, `stacc merged`,
  and the detect/dispose seam) is shipped and is the fallback when no forge is
  selected or a forge is unreachable.
- **The readiness mapping is the new iteration surface.** "Rich display, simple
  gate" moves the risk off proactive gate flattening and onto mapping each forge's
  structured readiness field (GitHub `mergeable_state`, GitLab
  `detailed_merge_status`) into `MergeReadiness` / `MergeRejectionReason` (R8/R16).
  Deriving the reason from these structured fields (read at merge time) is far more
  reliable than parsing free-text rejection bodies, but git-spice ignores these
  fields entirely, so there is no reference mapping for GitLab's
  `detailed_merge_status` values; expect iteration on the `Unknown` fallback.
- **GitLab Merge Trains and tiering.** Free vs Premium API behavior differs;
  Merge Trains create detached pipeline refs that can break the
  `head_pipeline.status` -> `ChecksState` derivation, and "pipelines must
  succeed" settings change merge acceptance. Slice 2 targets the standard MR flow
  on gitlab.com; Merge Trains are untested and out of scope. (Open question
  below.)
- **The breaking JSON change has a wide blast radius** (5+ emit sites across four
  command modules plus their integration tests). Mitigated by centralizing the
  serialization (U5) and golden-snapshot tests, so the break lands atomically.
- **Per-forge keychain migration.** Generalizing the storage `user` must preserve
  the existing `github.com` slot value or stored GitHub tokens are invalidated
  (KTD6).
- **GitLab review mapping is beyond the reference.** git-spice ignores approvals
  entirely; stacc's `ReviewState` for GitLab (approvals API) is new territory and
  may need iteration.
- **A GitLab project may force source-branch deletion.** A project with "remove
  source branch after merge" enabled deletes the source branch on merge regardless
  of our `remove_source_branch=false`, orphaning the stack and breaking
  source-branch-query detection (the STA-90 phantom class). Mitigated by reading
  back `force_remove_source_branch` and falling back to IID-based detection with a
  warning (U7/U9); a repo configured this way is partially incompatible with
  keep-alive and the warning says so.

---

## Acceptance Examples

Carried from origin (AE1-AE4, AE6) and extended (AE5, AE7-AE9) for
plan-introduced contracts. Each maps to the units that satisfy it.

- AE1. Covers R2, R9 (U6, U7). Given a gitlab.com remote and a tracked stack,
  When `submit` runs, Then one MR per branch is opened with its base set to the
  parent branch, reported as "MR" in text and under the neutral schema in JSON.
- AE2. Covers R6, R7 (U5, U10). Given the same `submit` on a GitHub repo and a
  GitLab repo, When the JSON outputs are compared, Then field names are identical
  and only the human label differs (PR vs MR).
- AE3. Covers R3, R4 (U2, U3). Given a self-managed GitLab host not identifiable
  by name, When `.stacc.toml` names the forge kind and base URL, Then stacc
  selects GitLab against that base URL with no code change.
- AE4. Covers R12 (U9). Given a GitLab MR merged through the web UI, When `sync`
  runs with GitLab reachable, Then the merge is detected authoritatively and
  children restack; and When the repo is forge-less, Then the slice-1 heuristic
  flags it and `stacc merged` disposes.
- AE5. Covers R8, R11 (U7, U11). Given a GitLab MR whose pipeline is pending, When
  stacc reads readiness, Then the neutral status reports checks-pending and the
  merge gate stops with `checks_pending`, the same predicate GitHub's pending
  rollup produces.
- AE6. Covers R14 (U8). Given only a GitLab PAT and no OAuth app, When the full
  submit/merge/detect loop runs, Then it completes with no forge-app
  registration.
- AE7. Covers R16 (U7, U11). Given a GitLab MR with a merge conflict, When `merge`
  attempts it, Then stacc surfaces a neutral `MergeRejectionReason` of `conflict`
  derived from `detailed_merge_status` (not an opaque error, and with no raw forge
  body in the output).
- AE8. Covers R17 (U11). Given a repo with no CI and no branch protection, When
  `merge` runs without `--allow-no-ci`, Then it stops with `no_ci_unprotected`;
  When `--allow-no-ci` is passed, Then it proceeds.
- AE9. Covers R18 (U5). Given any forge auth failure, When the command emits JSON,
  Then the error envelope carries a neutral structured `type` and `schema_version`
  with no `forge` discriminator and no raw response body.

---

## Open Questions

- **GitLab Merge Trains / "pipelines must succeed".** Does any dogfood target use
  Merge Trains? If so, `head_pipeline.status` may not reflect the mergeable
  pipeline. Resolve by confirming the dogfood repo's settings before relying on
  the simple checks mapping; treat Merge Trains as a follow-up.
- **GitLab draft API confirmation.** KTD9 assumes draft is set via the `Draft:`
  title prefix (the reference's approach) and that the API `draft` field is
  read-only. Confirm against the current GitLab API at implementation time; if a
  writable boolean exists, prefer it (it removes the double-prefix hazard).
- **Exact neutral field names, the `MergeRejectionReason` variant set, and the
  GitLab `detailed_merge_status` -> `MergeReadiness` mapping.** Pinned at
  implementation time in U1/U5/U7 from the real emit sites and live GitLab
  responses; the values here are directional, and git-spice provides no reference
  for the `detailed_merge_status` mapping.

---

## Sources / Research

- stacc GitHub client and the PR-shaped surface to neutralize (`GitHub` struct,
  `parse_remote`, `ready()`, the GraphQL checks query, the `merged`/`merged_at`
  quirk): `crates/stacc-github/src/lib.rs`,
  `crates/stacc-github/src/error.rs`, `crates/stacc-github/src/auth.rs`.
- stacc config surface and the closed `Key` namespace a forge override extends,
  plus the slice-1 `local` precedent: `crates/stacc-config/src/lib.rs`.
- The forge boundary, `--offline`, host handling, the merge gate, and the
  `reconcile_detection` classifier (the detect/dispose seam):
  `crates/stacc/src/commands/operations.rs`, `crates/stacc/src/cli.rs`,
  `crates/stacc/src/commands.rs`, `crates/stacc/src/commands/log.rs`,
  `crates/stacc/src/commands/info.rs`, `crates/stacc/src/error.rs`.
- Crate layering and the "only the binary depends on stacc-github" /
  "core stays off the forge client" invariants: each crate's `Cargo.toml`,
  `crates/stacc-core/src/ops.rs`.
- Slice 1 (the forge-less floor this builds on):
  `docs/plans/2026-06-11-001-feat-forge-less-local-floor-plan.md`,
  `docs/brainstorms/2026-06-11-seamless-local-multi-forge-requirements.md`.
- The credential-hygiene contracts (host-gated CLI fallback, never echo token or
  raw remote URL, `STACC_GH_BIN`-style hook):
  `docs/plans/2026-06-10-001-fix-sync-credential-loud-failure-plan.md`.
- git-spice forge abstraction (the trait shape, registry, neutral Change model,
  per-forge auth, the opt-in capability interfaces stacc defers):
  `../git-spice/internal/forge/forge.go`.
- git-spice GitLab provider and REST v4 endpoint map (create/merge/state/checks/
  detection/auth, the `Draft:` prefix, `remove_source_branch`):
  `../git-spice/internal/forge/gitlab/`,
  `../git-spice/internal/gateway/gitlab/api.go`,
  `../git-spice/internal/gateway/gitlab/client.go`.
- The neutral-model consumer / readiness orchestration for contrast:
  `../git-spice/internal/handler/merge/handler.go`;
  `../git-spice/internal/forge/github/{states,merge,checks,find}.go`.
