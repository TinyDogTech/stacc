# Stacc — current state

Created: 2026-05-30
Status: pre-alpha
Authors: Jillian Kozyra

# **Goal**

Ship a stacked-diff CLI that an AI coding agent can drive end-to-end:
every command is non-interactive, every output is machine-readable on
demand, and every git or GitHub call is recoverable enough that the
agent can keep going after a conflict.

# **Background**

The stacked-diff ecosystem (Graphite, git-spice, the Phabricator-era
`arc`) optimizes for a human at a terminal. Agents need the same
primitives but with a different shape: structured JSON output, no
editors, no prompts, recorded conflict context the agent can read back,
and an auth path that doesn't depend on a browser session belonging to
a specific human.

The MVP shipped the end-to-end happy path against GitHub. v1.1 closed
the safety and ergonomics gaps that surfaced once the tool was used
against real repos.

# **Overview**

stacc is a Rust workspace with six crates. The runtime entry point is
the `stacc` binary; the rest are libraries that group related concerns.

| Crate | Responsibility |
| --- | --- |
| `stacc` | CLI entry point: parsing, dispatch, alias expansion, git proxy |
| `stacc-config` | Config file + trunk/remote autodetection |
| `stacc-core` | Currently empty stub; operations live in `stacc::commands` |
| `stacc-git` | Typed wrappers over the `git` CLI |
| `stacc-github` | GitHub REST client + OAuth device flow + keychain |
| `stacc-state` | Stack state stored as a JSON tree under `refs/stacc/` |

Design intent and core algorithms live in
[`plans/stacc.md`](stacc.md) and
[`plans/algorithms.md`](algorithms.md).

# **Shipped**

## MVP — end-to-end stacked workflow against GitHub

| Ticket | Summary |
| --- | --- |
| STA-1 | Scaffold cargo workspace + 6 crates |
| STA-2 | CLI shell — arg parsing, global flags, miette errors |
| STA-3 | `stacc-git` — git command wrapper |
| STA-4 | `stacc-state` — `refs/stacc/` JSON-tree storage + CAS |
| STA-5 | `stacc-github` — API client (PAT auth) |
| STA-6 | `stacc-config` — config + trunk/remote autodetect |
| STA-7 | `stacc init` |
| STA-8 | `stacc track` |
| STA-9 | `stacc log` |
| STA-10 | `stacc status` |
| STA-11 | `stacc submit` (with `--description`) |
| STA-12 | `stacc sync` — squash-merge detect + restack |
| STA-13 | `stacc sync --continue` + conflict-context file |

## v1.1 — safety, ergonomics, and a real auth story

| Ticket | Summary |
| --- | --- |
| STA-14 | Stack-wide submit — walks the downstack bottom-up |
| STA-15 | Fork-point recovery when a recorded base hash goes stale |
| STA-16 | Strict fetch + `--force-with-lease` push (with `--offline` opt-out) |
| STA-17 | OAuth device flow + OS-keychain token storage |
| STA-19 | Proxy unknown commands to `git` + the `st` short alias |
| STA-20 | Bug fix: state load failing on slashed branch names |
| STA-21 | User-defined command aliases |
| (chore) | Workspace `clippy::pedantic` opt-in |

# **In progress**

Nothing in flight. v1.1 is feature-complete.

# **Upcoming**

## Deferred

* **STA-18 — CI: build, clippy, and test on PRs.** Held back to avoid
  GitHub Actions billing while the v1.1 backlog was open. With v1.1
  shipped, this is the obvious next pick.

## Pre-announcement follow-up for STA-17

The OAuth `client_id` constant in
`crates/stacc-github/src/auth.rs` ships as a placeholder. Register an
OAuth App under TinyDogTech with device flow enabled, then patch
`DEFAULT_OAUTH_CLIENT_ID` with the real client ID before we announce
the auth feature. Until then, env-var PAT auth still works; `stacc
auth login` against real GitHub does not.

## v2 candidates (no tickets yet)

* **GitLab support.** The abstraction would live behind a `Forge` trait
  in a renamed `stacc-github` crate so the rest of the codebase stays
  forge-agnostic.
* **Real quick-start guide in the README.** The current README covers
  workspace layout and authentication but not the typical `init →
  track → submit → sync` loop.
* **Operations layer in `stacc-core`.** Today `stacc::commands` does
  the orchestration; moving it into `stacc-core` is the original
  intent and would let future entry points (a daemon, an MCP server)
  reuse the logic without depending on the CLI crate.
* **Richer `stacc log` / `stacc status`** — show ancestors' PR state,
  not just the recorded number.

# **Caveats**

* **Pre-alpha.** No SemVer guarantees, no published binaries, no CI.
* **GitHub only.** GitLab and Bitbucket would slot in cleanly but
  haven't been built.
* **OAuth device flow grants the coarse `repo` scope** because GitHub
  OAuth Apps don't support fine-grained permissions. Users who want
  least privilege should set `GITHUB_TOKEN` to a fine-grained PAT
  scoped to Pull requests: read+write and Contents: read.
* **The keyring backend is platform-native.** macOS, Windows, and
  Linux with Secret Service over dbus all work; headless Linux without
  a session keyring will fall back to env-var auth only.
* **`refs/stacc/` is not a public format.** Treat it like git's own
  refs — read through stacc, not through ad-hoc tooling.
