# stacc

A stacked-diff CLI for AI coding agents, written in Rust.

Existing stacked-diff tools assume a human operator. stacc assumes an agent:
every command is non-interactive by default and can emit machine-readable JSON.

> Status: pre-alpha. See [`plans/stacc.md`](plans/stacc.md) for the design and
> [`plans/algorithms.md`](plans/algorithms.md) for the core algorithms.

## Workspace layout

| Crate | Responsibility |
| --- | --- |
| `stacc` | Binary entry point, CLI parsing and command dispatch |
| `stacc-core` | Stack operations: submit, sync, restack |
| `stacc-git` | Typed wrappers over the `git` CLI |
| `stacc-github` | GitHub API client and auth |
| `stacc-state` | State storage in the `refs/stacc/` git ref |
| `stacc-config` | Configuration and trunk/remote autodetection |

## Build

```
cargo build
```

## Authentication

stacc talks to the GitHub API for everything related to PRs. We accept three
auth sources and resolve them in this order:

1. `GITHUB_TOKEN` (or `GH_TOKEN`) environment variable
2. A token stored in the OS keychain by `stacc auth login`
3. Otherwise: error with a hint to run `stacc auth login`

Env-var auth always wins so CI runs and one-shot scripted use just work.

### Option A: `stacc auth login` (recommended for interactive use)

```
stacc auth login
```

We print a short user code and a verification URL. Open the URL in a browser,
paste the code, approve the request, and stacc stores the resulting token in
your platform's keychain (macOS Keychain / Windows Credential Manager /
GNOME or KDE Secret Service on Linux). Subsequent `stacc submit`/`sync` calls
use it automatically.

To check or clear:

```
stacc auth status
stacc auth logout
```

The OAuth scope GitHub grants here is `repo`, the narrowest scope GitHub
OAuth Apps can request that still allows PR read/write. If you want tighter
permissions, use a fine-grained PAT (option B).

### Option B: Personal access token via env var

For CI, container deploys, or anyone who prefers explicit scope control,
export a PAT before running stacc:

```
export GITHUB_TOKEN=ghp_xxx     # or GH_TOKEN
```

We recommend a **fine-grained** PAT scoped to the repositories you stack
against, with the following permissions:

| Permission | Access |
| --- | --- |
| Pull requests | Read and write |
| Contents | Read |
| Metadata | Read (mandatory) |

That's strictly less power than what `stacc auth login` requests. Fine-grained
PATs also force expiration (max 1 year), which we consider a feature.

Classic PATs work too, set `repo` scope, but they grant write access to
every repo your account can reach, so we don't recommend them.

### Status

`stacc auth status` reports which source is active and which user we're
authenticated as. If both env-var and keyring tokens are set, it notes that
the env var wins.
