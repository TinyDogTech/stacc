//! Build script: stamp a git-aware version into the binary so `stacc --version`
//! distinguishes a from-source/dev build from a published release.
//!
//! Emits `STACC_VERSION` for clap to read via `env!`. The string is
//! `<crate-version> (<short-sha>[-dirty])` in a git checkout, and falls back to
//! the plain crate version when `.git` or the `git` binary is absent (a
//! crates.io tarball, a vendored or container build), so it never fails the
//! build and never writes to the source tree (publish-safe).

use std::process::Command;

fn main() {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let version = match git_build() {
        Some(build) => format!("{pkg} ({build})"),
        None => pkg,
    };
    println!("cargo:rustc-env=STACC_VERSION={version}");

    // Re-run only when git state moves, so the hash updates on commit but an
    // unrelated `cargo build` does not rebuild. Emitting any rerun line disables
    // Cargo's default whole-package scan, so build.rs must list itself too.
    // `--git-path` resolves each file inside the real git dir (worktree-safe) and
    // is relative to this package root, where the build script runs. `packed-refs`
    // is included because `git gc` can move a ref's sha out of the loose `refs`
    // tree into it, which would otherwise leave a stale stamped sha.
    println!("cargo:rerun-if-changed=build.rs");
    for name in ["HEAD", "index", "refs", "packed-refs"] {
        if let Some(path) = run_git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// The short HEAD sha plus a `-dirty` marker when the working tree has changes,
/// or `None` when git is unavailable (no `.git`, no `git` binary).
fn git_build() -> Option<String> {
    let sha = run_git(&["rev-parse", "--short", "HEAD"])?;
    let dirty = run_git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    Some(if dirty { format!("{sha}-dirty") } else { sha })
}

/// Run a git command anchored at this package's directory, returning its trimmed
/// stdout, or `None` on any failure (missing binary, non-zero exit, empty
/// output). The `-C $CARGO_MANIFEST_DIR` anchor pins the working directory so a
/// cross-compile or container build that runs the script from elsewhere still
/// queries this checkout (and `--git-path` results stay relative to it).
fn run_git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}
