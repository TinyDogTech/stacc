# stacc (AGENTS.md)

Project conventions for coding agents. Inherits global defaults from
`~/.pi/agent/AGENTS.md` (plain `git` + `gh`).

stacc is a stacked-diff CLI for AI coding agents, Rust, Astral/uv aesthetic
(single binary, miette errors, JSON output, zero-config). Model: branch-per-PR;
v1 is git + GitHub only (PAT/OAuth device flow, fully local); state in a hidden
`refs/stacc/` git ref; design inspired by git-spice (reimplementation, not a
fork). Linear: team **STA** (workspace tiny-dog-tech), branches
`jillian/sta-<n>-<slug>`. MVP (STA-1..13) is merged; v1.1 backlog is STA-14..18.
Full decisions in `plans/stacc.md`; deeper notes in the Claude project memory at
`~/.claude/projects/-Users-jilliankozyra-projects-stacked-diff-tooling/memory/`.

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets    # also the citation source for "idiomatic" claims
```

## Explaining Rust

This project doubles as a way to learn Rust. When you make changes:

- **Explain in your response, not in code comments.** Walk through what changed
  and why in the terminal. Do not embed teaching in code comments, keep comments
  minimal and idiomatic (only when behavior isn't obvious from the code itself).
- **Show the code you're explaining.** Quote the specific lines and explain them
  inline in your response.
- **Justify Rust implementation choices.** When you pick a type, trait, pattern,
  or crate, say why it's the idiomatic / Rust-standard choice over the
  alternatives.
- **Cite sources for "idiomatic" claims.** Before calling something idiomatic,
  name a specific source: a clippy lint (e.g. `clippy::redundant_closure_for_method_calls`),
  a Rust Book chapter, an stdlib example, or an API Guidelines entry. If you
  can't cite one, say "I'd prefer" or "I'd lean toward" instead, taste, not
  consensus. When in doubt, run `cargo clippy --workspace --all-targets` and let
  the lint output be the citation.

## Workflow

> Use plain `git` + `gh` for all branch, push, and PR operations (matches the
> global default; do **not** use `gt`). This repo is itself the future
> replacement for Graphite, see `plans/stacc.md`.

- Use plain `git` + `gh` (NOT `gt`).
- One branch per Linear ticket, named `jillian/sta-<n>-<slug>` to match the
  Linear branch name so PRs auto-link to the issue.
- Conventional-commit format for commit messages and PR titles (still applies).
- Squash-merge only (the repo enforces it).

## Project

- Design: `plans/stacc.md`. Core algorithms: `plans/algorithms.md`.
- Go reference implementation (git-spice) lives at `../git-spice/`, a sibling of
  this repo.
