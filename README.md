# stacc

A stacked-diff CLI for AI coding agents, written in Rust.

Existing stacked-diff tools assume a human operator. stacc assumes an agent:
every command is non-interactive by default and can emit machine-readable JSON.

> Status: pre-alpha. See [`plans/stacc.md`](plans/stacc.md) for the design and
> [`plans/algorithms.md`](plans/algorithms.md) for the core algorithms.

## Workspace layout

| Crate | Responsibility |
| --- | --- |
| `stacc` | Binary entry point — CLI parsing and command dispatch |
| `stacc-core` | Stack operations: submit, sync, restack |
| `stacc-git` | Typed wrappers over the `git` CLI |
| `stacc-github` | GitHub API client and auth |
| `stacc-state` | State storage in the `refs/stacc/` git ref |
| `stacc-config` | Configuration and trunk/remote autodetection |

## Build

```
cargo build
```
