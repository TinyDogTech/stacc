# stacc

[![CI](https://github.com/TinyDogTech/stacc/actions/workflows/ci.yml/badge.svg)](https://github.com/TinyDogTech/stacc/actions/workflows/ci.yml)

A stacked-diff CLI.

stacc makes it easy to break a big change into a stack of one-PR-per-branch commits and rebases
the right branches automatically when a base moves. Every command is
non-interactive by default and can emit JSON, so it scripts cleanly in CI, in your
own tooling, or under an AI agent, and prints readable output when you run it at a
terminal. It installs as `stacc` and the short alias `st`.

## Why stacc

Most stacked-diff tools assume a person at a terminal: prompts, pagers,
interactive pickers. stacc works the same whether you drive it by hand or via an agent. It's non-interactive by default, with machine-readable output on request.

- **Non-interactive by default.** No command blocks on a prompt. Pass
  `--no-interactive` and a would-be prompt becomes a structured error instead of
  a hang.
- **Machine-readable output.** `--format json` on every command emits a JSON
  object a script or agent can parse. The default `pretty` format is for humans.
- **Structured errors.** Failures are JSON objects with a stable `error`
  discriminator (`conflict`, `usage`, `ambiguous`, and so on), so a caller
  branches on the kind instead of scraping text.
- **Branch per PR.** Each branch is one reviewable change stacked on its parent.
  stacc records the stack in a hidden git ref and rebases the right branches when
  a base moves.
- **One binary, zero config.** A single static binary that autodetects your trunk
  and remote. No runtime, no config file to start.

## Install

Install the latest release with the one-line script (macOS and Linux):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/TinyDogTech/stacc/releases/latest/download/stacc-installer.sh | sh
```

On Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/TinyDogTech/stacc/releases/latest/download/stacc-installer.ps1 | iex"
```

Or with Homebrew:

```sh
brew install TinyDogTech/tap/stacc
```

Both the `stacc` and `st` (short alias) binaries are installed. If you would
rather inspect the script before piping it to a shell, download it first and read
it. Every release also ships SHA-256 checksums and signed build provenance, verify
a downloaded binary with `gh attestation verify <binary> --repo TinyDogTech/stacc`.

Building from source needs a Rust toolchain:

```sh
git clone https://github.com/TinyDogTech/stacc
cd stacc
cargo build --release   # binaries: target/release/stacc and target/release/st
```

## Quickstart

The whole lifecycle, from an empty branch to a merged stack. The same commands
apply to a single PR and to a stack of twenty.

Point stacc at your repo. It reads your trunk (`main` or `master`) and remote
from git and initializes its state:

```sh
stacc init
```

Stage a change and create a branch for it. stacc commits the staged work and
stacks the new branch on the current one:

```sh
git add -A
stacc create add-user-api -m "feat: add the user API"
```

Open a pull request:

```sh
stacc submit
```

Revise after review by amending in place. stacc restacks everything above the
branch you changed:

```sh
git add -A
stacc modify
stacc submit
```

Stack a second change on top of the first:

```sh
git add -A
stacc create add-user-ui -m "feat: render the user list"
stacc submit
```

You now have two stacked PRs. Print the stack:

```sh
stacc log
```

```
◉ add-ui (current)
│ 2 minutes ago
│ 95610c6 - feat: render the user list
│
○ add-api
│ 5 minutes ago
│ 95338df - feat: add the user API
│
○ main
  1 hour ago
```

The trunk sits at the bottom, `◉` marks the branch you are on, and each branch
shows its latest commit and (once submitted) its live PR status. `stacc log
short` is the same graph without the per-branch detail, and `stacc log long` is
git's own history for the stack.

When the bottom PR merges, pull trunk, detect the merge, and restack the rest:

```sh
stacc sync
```

Move around the stack without remembering branch names:

```sh
stacc up         # toward the tip
stacc down       # toward trunk
stacc checkout   # pick interactively on a terminal, or pass a branch name
```

When the stack is approved, merge the ready PRs from trunk upward and sync:

```sh
stacc merge
```

Every command takes `--format json` for the same result in machine-readable
form:

```sh
stacc log --format json
```

```json
{"trunk":"main","stack":[{"name":"add-api","base":"main","pr":{"number":123,"url":"https://github.com/you/repo/pull/123","status":"open"},"commit":{"sha":"95338df","subject":"feat: add the user API","age":"5 minutes ago"},"children":[{"name":"add-ui","base":"add-api","pr":{"number":124,"url":"https://github.com/you/repo/pull/124","status":"open"},"commit":{"sha":"95610c6","subject":"feat: render the user list","age":"2 minutes ago"},"children":[]}]}]}
```

Each branch carries its `pr` (an object `{number, url, status}`, or `null`
before submit) and its `commit` (or `null` when the branch adds none of its
own). Pass `--no-status` to skip the live PR lookup, or `--stack` to scope the
output to the current branch's stack.

Reach for `stacc <command> --help` for the full flags of any command.

## Aliases

Common commands have built-in short aliases, so you type less:

| Alias | Expands to |
| --- | --- |
| `co` | `checkout` |
| `u` | `up` |
| `d` | `down` |
| `l` | `log` |
| `ls` | `log short` |
| `ll` | `log long` |
| `st` | `status` |

The binary is also installed as `st`, so `st co`, `st u`, and `st l` all work.

Define your own in an `[aliases]` table, globally in `~/.config/stacc/config.toml`
or per repo in `.stacc.toml`. Each name maps to a command line (multiple tokens
are fine):

```toml
[aliases]
ci = "submit"
ship = "submit --description @PR.md"
s = "sync"
```

Precedence runs built-in defaults, then your global config, then the repo's
`.stacc.toml` (repo wins).

## For agents

Point your agent at this section (or paste it into your `AGENTS.md`). It is the
stable contract; the exact per-command JSON shapes live in `stacc <command>
--help` and `--format json`.

- **Always pass `--format json`.** Every command returns a JSON object on
  success. For example, `stacc status --format json`:

  ```json
  {"branch":"add-ui","base":"add-api","pr":null,"children":[]}
  ```

- **Pass `--no-interactive` to never block.** Any command that would prompt a
  human instead fails with a structured error, so an agent never hangs on a TTY.

- **Errors are JSON objects with an `error` discriminator.** Branch on `error`,
  not on the message text:

  | `error` | Meaning |
  | --- | --- |
  | `conflict` | A rebase stopped on a merge conflict (see recovery below) |
  | `usage` | Bad arguments or a precondition not met |
  | `ambiguous` | The command needs you to disambiguate; `choices` lists the options |
  | `not_in_progress` | `continue`/`abort` with nothing to resume |
  | `git` / `github` / `state` / `config` | A failure from that subsystem |

- **Recover from conflicts with `continue` / `abort`.** When a restack hits a
  conflict, stacc stops and tells you how to proceed:

  ```json
  {"error":"conflict","branch":"add-ui","continue":"stacc continue","abort":"stacc abort"}
  ```

  Resolve the conflict in the working tree, then `stacc continue` to resume the
  operation, or `stacc abort` to unwind it.

Command cheatsheet:

| Purpose | Commands |
| --- | --- |
| Set up | `init`, `track`, `auth` |
| Build the stack | `create`, `modify`, `restack`, `move`, `rename` |
| Ship | `submit`, `sync`, `merge` |
| Navigate | `up`, `down`, `top`, `bottom`, `checkout`, `log`, `status`, `pr` |
| Recover | `continue`, `abort` |

Any subcommand stacc does not recognize is passed through to `git`, so
`stacc commit`, `stacc push`, and friends work as expected.

## Authentication

stacc talks to the GitHub API for everything PR-related. It resolves a token from
three sources, in order:

1. `GITHUB_TOKEN` (or `GH_TOKEN`) environment variable
2. A token stored in your OS keychain by `stacc auth login`
3. Otherwise, an error with a hint to run `stacc auth login`

The env var always wins, so CI and scripted use just work.

For interactive use, run the OAuth device flow once and stacc stores the token in
your platform keychain (macOS Keychain, Windows Credential Manager, or the GNOME
or KDE Secret Service on Linux):

```sh
stacc auth login
stacc auth status   # which source is active, and as whom
stacc auth logout   # clear the keychain token
```

For CI or explicit scope control, use a fine-grained PAT with **Pull requests:
read and write**, **Contents: read**, and **Metadata: read**, scoped to the
repositories you stack against. That is strictly less power than the OAuth flow
requests.

## How it works

stacc stores the stack in a hidden `refs/stacc/` git ref: a JSON tree of branches,
each recording its base and optional PR. Because the state lives in git, it
travels with the repo and needs no external database. Each branch is one PR; when
a base branch moves (a parent is amended, or a downstack PR merges), stacc rebases
exactly the branches that need it and updates their PR bases.

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

The workspace is six crates:

| Crate | Responsibility |
| --- | --- |
| `stacc` | Binary entry point, CLI parsing, command dispatch |
| `stacc-core` | Stack operations: submit, sync, restack, merge |
| `stacc-git` | Typed wrappers over the `git` CLI |
| `stacc-github` | GitHub API client and auth |
| `stacc-state` | State storage in the `refs/stacc/` git ref |
| `stacc-config` | Configuration and trunk/remote autodetection |
