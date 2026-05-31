# stacc — AGENTS.md

Project conventions for coding agents. Inherits global defaults from
`~/.pi/agent/AGENTS.md` (plain `git` + `gh`).

## Explaining Rust

This project doubles as a way to learn Rust. When you make changes:

- **Explain in your response, not in code comments.** Walk through what changed
  and why in the terminal. Do not embed teaching in code comments — keep comments
  minimal and idiomatic (only when behavior isn't obvious from the code itself).
- **Show the code you're explaining.** Quote the specific lines and explain them
  inline in your response.
- **Justify Rust implementation choices.** When you pick a type, trait, pattern,
  or crate, say why it's the idiomatic / Rust-standard choice over the
  alternatives.

## Workflow

> Use plain `git` + `gh` for all branch, push, and PR operations (matches the
> global default; do **not** use `gt`). This repo is itself the future
> replacement for Graphite — see `plans/stacc.md`.

- Use plain `git` + `gh` (NOT `gt`).
- One branch per Linear ticket, named `jillian/sta-<n>-<slug>` to match the
  Linear branch name so PRs auto-link to the issue.
- Conventional-commit format for commit messages and PR titles (still applies).
- Squash-merge only (the repo enforces it).

## Project

- Design: `plans/stacc.md`. Core algorithms: `plans/algorithms.md`.
- Go reference implementation (git-spice) lives at `../git-spice/`, a sibling of
  this repo.
