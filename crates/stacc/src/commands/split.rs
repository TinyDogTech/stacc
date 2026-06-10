//! `stacc split`: divide the current branch into stacked branches (KTD-3).
//!
//! Two non-interactive modes:
//!
//! - **by-commit** (default, positional names): each own commit except the tip
//!   becomes a new branch, created AT the existing commit hash; the tip keeps
//!   the original branch name. No cherry-pick, no re-authoring: every commit id
//!   is unchanged, so downstream PR references survive. The original ref and
//!   its upstack never move, which is why this mode needs no `guard_worktree`
//!   and no restack: there is nothing another worktree could desync.
//!
//! - **by-file** (`--by-file <pathspec>=<name>`, repeatable): the branch's own
//!   diff (base tree vs tip tree) is flattened and every changed path is
//!   partitioned into the first group whose pathspec matches it (literal path
//!   or directory prefix, deliberately not gitignore globbing). One re-authored
//!   commit per group is chained off the base, each becoming a new branch; the
//!   original branch moves to the final commit (identical tree, new ids; the
//!   documented cost: original commit boundaries and authorship do not
//!   survive). A path matching no group is a structured error, never a silent
//!   drop. The children then restack onto the moved tip.
//!
//! Both modes validate the whole spec before mutating anything (two-phase),
//! and the state delta is one transactional `StateStore::update` write. An
//! interactive picker is future TTY-only work; a missing spec is a structured
//! error, never a prompt.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use stacc_core::{ops, recovery};
use stacc_git::{Git, TreeEntry};
use stacc_state::{Base, BranchState, RepoConfig, State, StateStore};

use super::operations::{clear_conflict_artifacts, guard_worktree, restack_with_recovery};
use crate::cli::{OutputFormat, SplitArgs};
use crate::error::Error;

/// `stacc split`: see the module docs. Shared preconditions (tracked, not the
/// trunk, restacked onto its base), then mode dispatch.
pub fn split(args: &SplitArgs, format: OutputFormat) -> Result<(), Error> {
    if !args.names.is_empty() && !args.by_file.is_empty() {
        return Err(Error::Usage(
            "pass positional names (split by commit) or --by-file groups (split by file), not both"
                .into(),
        ));
    }

    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    if git.rebase_in_progress() {
        return Err(Error::Usage(
            "a rebase is already in progress; run `stacc continue` to resume it or `stacc abort` to undo".into(),
        ));
    }

    let branch = git.current_branch().map_err(|_| {
        Error::Usage("cannot split a detached HEAD; check out a branch first".into())
    })?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot split the trunk branch `{}`",
            repo.trunk
        )));
    }
    let base = state
        .branches
        .get(&branch)
        .map(|b| b.base.clone())
        .ok_or_else(|| {
            Error::Usage(format!(
                "branch `{branch}` is not tracked; run `stacc track` first"
            ))
        })?;

    // Precondition (git-spice `VerifyRestacked`, mirroring `squash`/`fold`):
    // the branch must sit on its base's live tip, so `base_tip..tip` is exactly
    // its own commits and the chain we carve forks exactly at `base_tip`.
    let tip = git.rev_parse(&branch)?;
    let base_tip = git.rev_parse(&base.name)?;
    if !git.is_ancestor(&base_tip, &tip)? {
        return Err(Error::Usage(format!(
            "`{branch}` is not restacked onto `{}`; run `stacc restack` first, then split",
            base.name
        )));
    }

    if args.by_file.is_empty() {
        split_by_commit(
            &git, &store, &state, &branch, &base.name, &base_tip, &tip, &args.names, format,
        )
    } else {
        split_by_file(
            &git,
            &store,
            &mut state,
            &repo,
            &branch,
            &base.name,
            &base_tip,
            &tip,
            &args.by_file,
            format,
        )
    }
}

/// By-commit mode: create a new branch ref at each own commit except the tip
/// (oldest first), chain the tracking state, and leave every existing ref and
/// the working tree untouched.
#[allow(clippy::too_many_arguments)]
fn split_by_commit(
    git: &Git,
    store: &StateStore,
    state: &State,
    branch: &str,
    base_name: &str,
    base_tip: &str,
    tip: &str,
    names: &[String],
    format: OutputFormat,
) -> Result<(), Error> {
    // The branch's own commits, oldest-first. Fewer than two: there is nothing
    // to split a branch point into, a clear no-op report rather than an error.
    let own_commits = git.rev_list(base_tip, tip)?;
    if own_commits.len() < 2 {
        report_split_commit(format, branch, &[]);
        return Ok(());
    }

    // No names: the interactive picker is a TTY-only convenience that does not
    // exist yet, so this is a structured error on and off a terminal, never a
    // prompt.
    let required = own_commits.len() - 1;
    if names.is_empty() {
        return Err(Error::Usage(format!(
            "`{branch}` has {} own commits; pass {required} branch name(s), one per commit except the tip, oldest first (the tip keeps `{branch}`). Interactive selection is not available yet.",
            own_commits.len()
        )));
    }
    if names.len() != required {
        return Err(Error::Usage(format!(
            "splitting `{branch}` ({} own commits) takes exactly {required} name(s), one per commit except the tip, oldest first; got {}",
            own_commits.len(),
            names.len()
        )));
    }

    // Phase one: validate EVERYTHING before any mutation.
    validate_new_names(git, state, branch, names)?;

    // Phase two: create each new branch ref AT the existing commit hash. The
    // empty expected-old is git's "must not exist" assertion (see the
    // update_ref contract test in stacc-git), so a concurrent creation loses
    // the race instead of being clobbered. No cherry-pick: ids are unchanged.
    let mut created: Vec<(String, String)> = Vec::new();
    for (name, sha) in names.iter().zip(&own_commits) {
        if let Err(err) = git.update_ref(&format!("refs/heads/{name}"), sha, Some("")) {
            cleanup_created_refs(git, &created);
            return Err(Error::Usage(format!(
                "could not create branch `{name}` at {sha} ({err}); the refs created so far were removed, nothing was split"
            )));
        }
        created.push((name.clone(), sha.clone()));
    }

    // One transactional state write: chain each new branch on its predecessor
    // (the first on the original base, refreshed to its live tip) and re-point
    // the original branch onto the last new one. The original ref and its
    // upstack never move (the tip is untouched, children sat on it and still
    // do), so there is no restack and no `guard_worktree`: no existing ref is
    // rewritten, so no other worktree can desync.
    let saved = store.update(|s| {
        apply_split_chain(s, branch, base_name, base_tip, &created);
        Ok(())
    });
    if let Err(err) = saved {
        // Atomic rollback: the only mutation so far is the new refs; remove
        // them so a failed state write leaves no half-split.
        cleanup_created_refs(git, &created);
        eprintln!("note: the created branch refs were removed; re-run the split");
        return Err(err.into());
    }

    report_split_commit(format, branch, &created);
    Ok(())
}

/// By-file mode: flatten the branch's own diff, partition the changed paths by
/// pathspec group, re-author one commit per group chained off the base, move
/// the original branch to the final commit (identical tree), commit the state,
/// THEN restack the children onto the moved tip.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn split_by_file(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    branch: &str,
    base_name: &str,
    base_tip: &str,
    tip: &str,
    specs: &[String],
    format: OutputFormat,
) -> Result<(), Error> {
    let own_commits = git.rev_list(base_tip, tip)?;
    if own_commits.is_empty() {
        return Err(Error::Usage(format!(
            "`{branch}` has no commits of its own above `{base_name}`; nothing to split"
        )));
    }

    let mut groups = parse_by_file_specs(specs)?;
    let names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
    validate_new_names(git, state, branch, &names)?;

    // The branch ref and every upstack child's ref rewrite here, so fail fast
    // if any of them is checked out in another worktree, BEFORE mutating.
    let upstack = ops::upstack_order(&state.branches, branch);
    guard_worktree(git, &upstack)?;

    // The branch ref moves under HEAD; refuse a dirty working tree rather than
    // mixing uncommitted edits into the moved tip's status.
    if git.has_uncommitted_changes()? {
        return Err(Error::Usage(
            "the working tree has uncommitted changes; commit or stash them, then split".into(),
        ));
    }

    // Flatten: the branch's full own diff is the base tip tree vs the branch
    // tip tree. Partition every changed path (added, modified, or deleted vs
    // the base) into the first group whose pathspec matches, in spec order.
    let base_entries = entry_map(git.tree_entries(base_tip)?);
    let tip_entries = entry_map(git.tree_entries(tip)?);
    let changed = changed_paths(&base_entries, &tip_entries);
    if changed.is_empty() {
        return Err(Error::Usage(format!(
            "`{branch}` has no file changes against `{base_name}`; nothing to split"
        )));
    }
    let mut orphans: Vec<String> = Vec::new();
    for path in &changed {
        match groups.iter_mut().find(|g| spec_matches(&g.pathspec, path)) {
            Some(group) => group.paths.push(path.clone()),
            None => orphans.push(path.clone()),
        }
    }
    if !orphans.is_empty() {
        return Err(Error::Usage(format!(
            "{} changed path(s) match no --by-file group: {}; add a group for them (a silent drop would lose changes)",
            orphans.len(),
            orphans.join(", ")
        )));
    }
    if let Some(empty) = groups.iter().find(|g| g.paths.is_empty()) {
        return Err(Error::Usage(format!(
            "--by-file group `{}={}` matches no changed path; drop it or fix the pathspec",
            empty.pathspec, empty.name
        )));
    }

    // Build the new chain bottom-up from the base: each group's commit is its
    // predecessor's tree with the group's paths swapped to (or removed per)
    // the branch tip. Object writes only; nothing is reachable until the ref
    // moves below. The last commit's tree equals the tip's by construction
    // (every changed path landed in some group), verified before any ref move.
    let mut current = base_entries;
    let mut parent = base_tip.to_string();
    let mut created: Vec<(String, String, Vec<String>)> = Vec::new();
    for group in &groups {
        for path in &group.paths {
            match tip_entries.get(path) {
                Some(entry) => {
                    current.insert(path.clone(), entry.clone());
                }
                None => {
                    current.remove(path);
                }
            }
        }
        let entries: Vec<TreeEntry> = current.values().cloned().collect();
        let tree = git.mktree(&entries)?;
        let commit = git.commit_tree(&tree, Some(&parent), &format!("split: {}", group.name))?;
        created.push((group.name.clone(), commit.clone(), group.paths.clone()));
        parent = commit;
    }
    let final_tip = parent;
    let tip_tree = git.rev_parse(&format!("{tip}^{{tree}}"))?;
    let final_tree = git.rev_parse(&format!("{final_tip}^{{tree}}"))?;
    if final_tree != tip_tree {
        return Err(Error::Usage(format!(
            "internal error: the rebuilt split chain's tree {final_tree} does not reproduce `{branch}`'s tree {tip_tree}; nothing was mutated"
        )));
    }

    // Ref surgery, all validated above: create the group refs (empty
    // expected-old: must not exist), then move the original branch to the
    // final commit with its old tip as the CAS lease.
    let mut created_refs: Vec<(String, String)> = Vec::new();
    for (name, sha, _) in &created {
        if let Err(err) = git.update_ref(&format!("refs/heads/{name}"), sha, Some("")) {
            cleanup_created_refs(git, &created_refs);
            return Err(Error::Usage(format!(
                "could not create branch `{name}` at {sha} ({err}); the refs created so far were removed, nothing was split"
            )));
        }
        created_refs.push((name.clone(), sha.clone()));
    }
    if let Err(err) = git.update_ref(&format!("refs/heads/{branch}"), &final_tip, Some(tip)) {
        cleanup_created_refs(git, &created_refs);
        return Err(Error::Usage(format!(
            "could not move `{branch}` to the split tip ({err}); the branch tip moved under split, re-run it"
        )));
    }
    // HEAD names the moved branch; the new tip's tree is identical to the old
    // one, so a mixed reset re-syncs the index and the status stays clean.
    if let Err(err) = git.reset_mixed(&final_tip) {
        eprintln!("warning: could not reset the index to the split tip: {err}");
    }

    // The state delta, committed in ONE transactional write BEFORE the
    // children restack, so a restack conflict interrupts an already-complete
    // split (see the continuation note below).
    let created_chain: Vec<(String, String)> = created
        .iter()
        .map(|(name, sha, _)| (name.clone(), sha.clone()))
        .collect();
    let apply_split =
        |s: &mut State| apply_split_chain(s, branch, base_name, base_tip, &created_chain);
    apply_split(state);
    if let Err(err) = store.update(|s| {
        apply_split(s);
        Ok(())
    }) {
        // Unwind the ref surgery so a failed state write leaves no half-split:
        // the original branch back on its old tip, the group refs gone.
        match git.update_ref(&format!("refs/heads/{branch}"), tip, Some(&final_tip)) {
            Ok(()) => {
                if let Err(reset_err) = git.reset_mixed(tip) {
                    eprintln!("warning: could not reset the index back to {tip}: {reset_err}");
                }
            }
            Err(ref_err) => {
                eprintln!("warning: could not restore `{branch}` to {tip}: {ref_err}");
            }
        }
        cleanup_created_refs(git, &created_refs);
        eprintln!("note: the split's ref moves were rolled back; re-run the split");
        return Err(err.into());
    }

    // Restack the children: their recorded base NAME is still `branch`, whose
    // tip changed (new sha, identical tree), so each child replays onto the
    // new tip, a content-no-op rebase. This runs strictly AFTER the refs and
    // state committed, so the plain `Restack` continuation is sufficient
    // (KTD-1's resumable-continuation requirement covers conflicts with
    // re-pointing still outstanding; none remains here): `continue` drains the
    // remaining children, `abort` clears the in-progress rebase and leaves the
    // finished split in place, and `stacc undo` is the full rollback net.
    let order: Vec<String> = upstack.iter().skip(1).cloned().collect();
    let restacked = restack_with_recovery(
        git,
        store,
        state,
        repo,
        &order,
        |remaining| recovery::Operation::Restack { remaining },
        &|_s| {},
    )?;
    clear_conflict_artifacts(git);
    // Best-effort: the engine leaves HEAD on the last child it rebased, so
    // restore the user to the split branch.
    if let Err(err) = git.checkout(branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    report_split_file(format, branch, &created, &restacked);
    Ok(())
}

/// The shared state delta of both modes: track each created branch chained on
/// its predecessor (the first on the original base at its live tip) and
/// re-point the original branch's base onto the last created branch.
fn apply_split_chain(
    s: &mut State,
    branch: &str,
    base_name: &str,
    base_tip: &str,
    created: &[(String, String)],
) {
    let mut prev_name = base_name.to_string();
    let mut prev_hash = base_tip.to_string();
    for (name, sha) in created {
        s.branches.insert(
            name.clone(),
            BranchState {
                base: Base {
                    name: prev_name.clone(),
                    hash: prev_hash.clone(),
                },
                pr: None,
            },
        );
        prev_name.clone_from(name);
        prev_hash.clone_from(sha);
    }
    if let Some(b) = s.branches.get_mut(branch) {
        b.base.name = prev_name;
        b.base.hash = prev_hash;
    }
}

/// Phase-one validation, all before any mutation: every requested name must be
/// new (no duplicates in the spec, not the branch being split, no existing
/// local branch ref, no tracked branch) and a valid git branch name.
fn validate_new_names(
    git: &Git,
    state: &State,
    branch: &str,
    names: &[String],
) -> Result<(), Error> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            return Err(Error::Usage(format!("duplicate split name `{name}`")));
        }
        if name == branch {
            return Err(Error::Usage(format!(
                "`{name}` is the branch being split; it keeps the tip, pick a new name"
            )));
        }
        if !git.valid_branch_name(name)? {
            return Err(Error::Usage(format!("`{name}` is not a valid branch name")));
        }
        if git.ref_commit(&format!("refs/heads/{name}"))?.is_some() {
            return Err(Error::Usage(format!("a branch named `{name}` already exists")));
        }
        if state.branches.contains_key(name) {
            return Err(Error::Usage(format!(
                "a branch named `{name}` is already tracked"
            )));
        }
    }
    Ok(())
}

/// Best-effort: delete the refs this split created, each leased on the sha it
/// was created at so a concurrent move is never clobbered.
fn cleanup_created_refs(git: &Git, created: &[(String, String)]) {
    for (name, sha) in created {
        if let Err(err) = git.delete_ref(&format!("refs/heads/{name}"), Some(sha)) {
            eprintln!(
                "warning: could not remove the created branch `{name}` ({err}); delete it with `git branch -D {name}`"
            );
        }
    }
}

/// One `--by-file` group: its pathspec, the branch name it creates, and the
/// changed paths the partition assigned to it.
struct FileGroup {
    pathspec: String,
    name: String,
    paths: Vec<String>,
}

/// Parse `--by-file` specs of the form `<pathspec>=<branch-name>` (split on
/// the LAST `=`, so a pathspec containing `=` survives). Two groups with the
/// same pathspec would match the same paths exactly, which is ambiguous:
/// refused rather than letting spec order decide silently.
fn parse_by_file_specs(specs: &[String]) -> Result<Vec<FileGroup>, Error> {
    let mut groups = Vec::with_capacity(specs.len());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for spec in specs {
        let parsed = spec
            .rsplit_once('=')
            .map(|(path, name)| (path.trim_end_matches('/'), name))
            .filter(|(path, name)| !path.is_empty() && !name.is_empty());
        let Some((pathspec, name)) = parsed else {
            return Err(Error::Usage(format!(
                "`{spec}` is not a `<pathspec>=<branch-name>` group"
            )));
        };
        if !seen.insert(pathspec.to_string()) {
            return Err(Error::Usage(format!(
                "duplicate --by-file pathspec `{pathspec}`; two groups matching the same paths is ambiguous"
            )));
        }
        groups.push(FileGroup {
            pathspec: pathspec.to_string(),
            name: name.to_string(),
            paths: Vec::new(),
        });
    }
    Ok(groups)
}

/// Whether `path` belongs to `pathspec`: a literal match, or a directory
/// prefix (`src` matches `src/a.rs`). Deliberately not gitignore-style
/// globbing: literal-or-prefix keeps the partition decidable and predictable.
/// Shared with `modify --patch`, whose path selection uses the same rule.
pub(crate) fn spec_matches(pathspec: &str, path: &str) -> bool {
    path == pathspec
        || path
            .strip_prefix(pathspec)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Index a tree's blob entries by path.
fn entry_map(entries: Vec<TreeEntry>) -> BTreeMap<String, TreeEntry> {
    entries.into_iter().map(|e| (e.path.clone(), e)).collect()
}

/// The paths whose blob differs between the base and tip trees: added,
/// modified (content or mode), or deleted. Sorted.
fn changed_paths(
    base: &BTreeMap<String, TreeEntry>,
    tip: &BTreeMap<String, TreeEntry>,
) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    for (path, entry) in tip {
        match base.get(path) {
            Some(b) if b.sha == entry.sha && b.mode == entry.mode => {}
            _ => changed.push(path.clone()),
        }
    }
    for path in base.keys() {
        if !tip.contains_key(path) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed
}

fn report_split_commit(format: OutputFormat, branch: &str, created: &[(String, String)]) {
    match format {
        OutputFormat::Json => {
            let list: Vec<Value> = created
                .iter()
                .map(|(name, sha)| json!({ "name": name, "sha": sha }))
                .collect();
            println!(
                "{}",
                json!({ "op": "split", "mode": "commit", "branch": branch, "created": list })
            );
        }
        OutputFormat::Pretty => {
            if created.is_empty() {
                println!("Nothing to split.");
            } else {
                println!("Split {branch} into {} new branch(es):", created.len());
                for (name, sha) in created {
                    println!("  {name} @ {}", &sha[..sha.len().min(8)]);
                }
                println!("{branch} keeps the tip");
            }
        }
    }
}

fn report_split_file(
    format: OutputFormat,
    branch: &str,
    created: &[(String, String, Vec<String>)],
    restacked: &[String],
) {
    match format {
        OutputFormat::Json => {
            let list: Vec<Value> = created
                .iter()
                .map(|(name, sha, paths)| json!({ "name": name, "sha": sha, "paths": paths }))
                .collect();
            println!(
                "{}",
                json!({
                    "op": "split",
                    "mode": "file",
                    "branch": branch,
                    "created": list,
                    "restacked": restacked,
                })
            );
        }
        OutputFormat::Pretty => {
            println!("Split {branch} by file into {} new branch(es):", created.len());
            for (name, sha, paths) in created {
                println!("  {name} @ {} ({})", &sha[..sha.len().min(8)], paths.join(", "));
            }
            for name in restacked {
                println!("Restacked {name}");
            }
        }
    }
}
