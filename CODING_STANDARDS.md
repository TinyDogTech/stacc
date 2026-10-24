# Coding standards

These standards apply to stacc's Rust code, tests, crate boundaries, dependencies, comments, and repository documentation. Read `AGENTS.md` for the stacc workflow and agent invocation rules.

## Toolchain

- Use the workspace toolchain: Rust 2021, Cargo, `rustfmt`, and Clippy. Do not add a parallel formatter, linter, or build system.
- Keep shared dependency versions and workspace lints in the root `Cargo.toml`; member crates opt in with `workspace = true` where appropriate. Commit `Cargo.lock` when dependencies change.
- Keep Clippy's pedantic group at `warn` with specific exceptions in `Cargo.toml`. CI denies remaining build warnings with `CARGO_BUILD_WARNINGS=deny`; do not use `-D clippy::pedantic`, which overrides the workspace exceptions. Review new warnings on their merits before changing an exception.

## Repository responsibilities

```text
crates/stacc          CLI parsing, command composition, output, and the st/stacc binaries
crates/stacc-core     Stack operations and recovery across git and state
crates/stacc-git      Git operations and repository interaction
crates/stacc-state    Stack state stored in refs/stacc/
crates/stacc-forge    Forge-neutral change models and Forge trait
crates/stacc-github   GitHub implementation, auth, and transport
crates/stacc-config   Configuration and aliases
plans                Design decisions and algorithms
```

- Keep GitHub API and authentication details in `stacc-github`. Keep the `Forge` contract free of GitHub-specific types and network code. The CLI chooses and composes implementations; lower-level crates do not depend on the CLI.
- Keep state invariants in `stacc-state`, git mechanics in `stacc-git`, and multi-step stack operations in `stacc-core`. Avoid duplicating an invariant in command handlers and backend crates.
- Put CLI arguments, interactive prompts, miette reporting, and JSON presentation in `stacc`. Do not let presentation formats become core domain models.
- Add a crate only for a coherent capability with a useful boundary. Do not create placeholder crates or one crate per concept. Expose only the supported public interface from each crate's `lib.rs`.

## Change scope and modules

- Keep a branch and its pull request focused and reviewable. Use one branch per Linear issue, as described in `AGENTS.md`. Split independent changes when they can be reviewed and verified separately; do not split changes solely to meet a file count.
- Give a module one cohesive responsibility, not one function per file. Prefer a small interface that hides ordering, validation, and integration work.
- Place an abstraction where behavior varies or a boundary needs to be protected. Avoid pass-through wrappers and generic `utils` or `helpers` modules.
- Keep implementation details private. Do not make a private function public solely to test it.
- Search for existing patterns before adding a new helper, trait, error type, or crate.

## State, boundaries, and errors

- Treat Git repositories, GitHub responses, configuration, and command arguments as untrusted at their boundaries. Validate before using them to change branches, refs, credentials, or remote state.
- Keep `refs/stacc/` as the authoritative local stack metadata. Preserve branch identity and parent relationships across restacks, syncs, and merges. Do not report success before a durable git or forge operation succeeds.
- Make no-op, conflict, retry, and partial-success states explicit. Do not silently discard a restack conflict or conceal a partial merge behind a success response.
- Use `Result` and typed errors for expected failures; reserve panics for broken internal invariants. Translate errors to miette for human output and structured JSON at the CLI boundary. Preserve useful context without exposing credentials or tokens.
- Keep JSON output compatible with the documented `schema_version` contract. Missing optional keys are meaningful to agents; change output shapes deliberately and cover them with tests.
- Name limits, timeouts, and retry policies where they are owned. Include units in names when the type alone does not make the unit clear.
- Prefer pure transformations for stack calculations. Isolate git commands, network calls, keyring access, and ref writes behind the existing boundaries so failure and recovery can be tested.

## Rust interfaces and comments

- Use types and names to express ownership and invariants. Borrow when the caller retains ownership; move values when the callee needs to own them. Do not add a trait or generic type when a concrete type communicates the contract better.
- Use `Option` for absence and `Result` for failure. Handle errors at the boundary that can add context or decide recovery; do not unwrap expected external failures.
- Add `//!` module docs or `///` API docs when they clarify purpose, caller obligations, ordering, or error behavior. Add inline comments only for non-obvious invariants and decisions. Do not narrate visible implementation steps or add teaching comments; explain Rust choices in the response, per `AGENTS.md`.
- Keep terms consistent: branch, parent, trunk, change, forge, restack, and state should not acquire new synonyms without a domain reason.
- Use short, direct sentences in docs and diagnostics. Define uncommon abbreviations. Show an ASCII relationship diagram only when it makes a non-obvious state or dependency relationship clearer, and update it with the code.

## Tests

- Protect behavior through the interface callers use. Use unit tests beside modules for focused invariants; use `crates/stacc/tests/` for CLI and git-repository behavior; use HTTP mocks in `stacc-github` tests for GitHub contracts. Do not duplicate the same case at every layer.
- Test success, no-op, invalid input, conflict, retry, and partial failure when the interface promises those states. For mutating commands, assert both JSON output and resulting repository or remote state.
- Use isolated temporary repositories, explicit fixtures, and mock HTTP responses. Do not rely on a developer's git config, credentials, network access, or mutable real repositories.
- Add regression tests for changes to ref state, command output, merge/sync behavior, and error mapping. Do not expose private internals just to make a test convenient.

## Completion checklist

1. Confirm the change is in the crate that owns the behavior and dependency direction remains intact.
2. Confirm state and JSON contracts, including failure and no-op behavior, remain truthful.
3. Confirm comments explain decisions rather than code mechanics, and tests protect distinct behavior.
4. Run `cargo fmt --all -- --check`, `cargo build --workspace`, `cargo test --workspace`, and `CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --all-features --locked`. Report any checks that could not run.
5. Follow the stacc branch, commit, submit, and merge rules in `AGENTS.md`; do not substitute `gh` for a stacc operation.
