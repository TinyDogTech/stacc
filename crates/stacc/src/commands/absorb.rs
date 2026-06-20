//! `stacc absorb`: distribute the staged hunks into the downstack commits that
//! introduced their lines (by blame), applied as in-memory tree rewrites, then
//! restack the upstack. Ambiguous and unsupported hunks are left staged and
//! reported, never prompted or silently dropped (KTD-2).
//!
//! The mapping is by *blame*, not apply-check: a hunk's context applies cleanly
//! to many ancestor commits, so "first clean apply" silently rewrites the wrong
//! commit. We blame the branch tip's content over the hunk's pre-image line
//! range and only map when every edited line blames to one commit that is in the
//! downstack chain's absorbable set.
//!
//! The apply is an in-memory `commit-tree` rewrite of the commit chain, never a
//! `git rebase`/autosquash: a non-stacc rebase strands the repo and `stacc
//! abort` refuses to clean it up. After moving all affected branch refs to the
//! rewritten tips, a `reset --mixed` leaves the absorbed hunks reading as
//! committed and only the unabsorbed hunks as unstaged modifications.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};
use stacc_core::{ops, recovery};
use stacc_git::{Git, Hunk, HunkKind};
use stacc_state::StateStore;

use super::operations::{clear_conflict_artifacts, guard_worktree, restack_with_recovery};
use crate::cli::{AbsorbArgs, OutputFormat};
use crate::error::Error;

/// A hunk assigned to a commit in the chain being rewritten. For `absorb` the
/// commit is found by blame; for `modify --into` it is the target's tip.
/// Shared with `modify --into`, which reuses [`rewrite_chain`].
pub(crate) struct Mapped {
    pub(crate) hunk: Hunk,
    /// The full SHA of the commit the hunk lands in.
    pub(crate) commit: String,
}

/// A hunk that could not be absorbed, with a machine-readable reason.
struct Unabsorbed {
    hunk: Hunk,
    /// One of: `ambiguous`, `boundary_insertion`, `outside_branch`,
    /// `added_file`, `binary`, `renamed`, `deleted`.
    reason: &'static str,
}

/// The mapping result: hunks that landed somewhere, and hunks left staged.
struct Mapping {
    mapped: Vec<Mapped>,
    unabsorbed: Vec<Unabsorbed>,
}

/// `stacc absorb`: see the module docs. Maps staged hunks to commits in the
/// downstack chain by blame, rewrites those commits' trees in memory, moves all
/// affected branch refs, `reset --mixed`es to leave only the unabsorbed hunks
/// unstaged, and restacks the upstack. `--dry-run` emits the mapping and
/// mutates nothing.
pub fn absorb(args: &AbsorbArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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
        Error::Usage("cannot absorb on a detached HEAD; check out a branch first".into())
    })?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot absorb on the trunk branch `{}`",
            repo.trunk
        )));
    }
    // Verify the current branch is tracked before building the chain.
    if !state.branches.contains_key(&branch) {
        return Err(Error::Usage(format!(
            "branch `{branch}` is not tracked; run `stacc track` first"
        )));
    }

    if !git.has_staged_changes()? {
        return Err(Error::Usage(
            "nothing staged to absorb; stage the changes you want distributed first".into(),
        ));
    }

    // Build the full downstack chain (bottom-up: [deepest, ..., current]).
    // Errors with a Usage message if any intermediate branch is untracked.
    let chain = ops::downstack_chain(&state, &branch, &repo.trunk)?;

    // For each branch in the chain, compute the tip SHA, the base-exclude SHA
    // (with the same stale-hash guard as the old single-branch code), and the
    // per-branch commit list. All SHAs are collected into commit_to_branch and
    // chain_commits so the caller can look up which branch owns a blame SHA.
    let mut commit_to_branch: BTreeMap<String, String> = BTreeMap::new();
    let mut chain_tips: BTreeMap<String, String> = BTreeMap::new();
    let mut chain_excludes: BTreeMap<String, String> = BTreeMap::new();
    let mut chain_commits: Vec<String> = Vec::new();
    for br in &chain {
        let bs = state.branches.get(br).expect("chain members are tracked");
        let br_tip = git.rev_parse(br)?;
        chain_tips.insert(br.clone(), br_tip.clone());
        let br_exclude = if git.is_ancestor(&bs.base.hash, br).unwrap_or(false) {
            bs.base.hash.clone()
        } else {
            git.rev_parse(&bs.base.name)?
        };
        chain_excludes.insert(br.clone(), br_exclude.clone());
        for sha in git.rev_list(&br_exclude, &br_tip)? {
            commit_to_branch.insert(sha.clone(), br.clone());
            chain_commits.push(sha);
        }
    }

    if commit_to_branch.is_empty() {
        return Err(Error::Usage(format!(
            "the stack containing `{branch}` has no commits above `{}` to absorb into",
            repo.trunk
        )));
    }

    let tip = chain_tips[&branch].clone();

    // absorbable_set: all commit SHAs across the full downstack chain.
    let absorbable_set: std::collections::BTreeSet<&str> =
        commit_to_branch.keys().map(String::as_str).collect();

    // Map every staged hunk to a chain commit (or report why it can't be).
    let hunks = git.diff_hunks()?;
    let mapping = map_hunks(&git, &branch, &hunks, &absorbable_set)?;

    if args.dry_run {
        report_dry_run(format, &branch, &chain_commits, &commit_to_branch, &git, &mapping);
        return Ok(());
    }

    if mapping.mapped.is_empty() {
        // Nothing absorbable: leave the staged changes exactly where they are
        // (no ref move, no reset) so the repo is untouched, never half-rewritten.
        report_result(format, &branch, &tip, &git, &mapping, &[], &commit_to_branch);
        return Ok(());
    }

    // Fail fast if any downstack branch (other than current HEAD) or upstack
    // branch is checked out elsewhere, BEFORE mutating anything.
    let upstack = ops::upstack_order(&state.branches, &branch);
    let downstack_others: Vec<String> =
        chain.iter().filter(|br| br.as_str() != branch.as_str()).cloned().collect();
    guard_worktree(&git, &downstack_others)?;
    guard_worktree(&git, &upstack)?;

    // Find the deepest branch in the chain that has at least one mapped commit.
    let rewrite_start_idx = chain
        .iter()
        .position(|br| {
            mapping
                .mapped
                .iter()
                .any(|m| commit_to_branch.get(&m.commit).map(String::as_str) == Some(br.as_str()))
        })
        .expect("at least one mapped hunk implies at least one matching branch");
    let affected_chain = &chain[rewrite_start_idx..];

    // All commits from the deepest affected branch's base through the current
    // branch tip -- the full range rewrite_chain must process.
    let first_exclude = chain_excludes[&affected_chain[0]].clone();
    let rewrite_commits = git.rev_list(&first_exclude, &tip)?;

    // Rewrite the full range in memory. rewrite_chain propagates each mapped
    // hunk from its introducing commit onward, so intermediate branch commits
    // pick up the edit automatically without a separate per-branch rewrite.
    let new_by_old =
        rewrite_chain(&git, Some(&first_exclude), &rewrite_commits, &mapping.mapped)?;
    let new_tip = new_by_old
        .get(&tip)
        .cloned()
        .expect("current branch tip is always in the rewrite range");

    // Move all affected branch refs bottom-up (affected_chain order: deepest
    // first). Moving in this order means each ref's old-tip CAS lease is valid
    // when we reach it; its parent's ref has already moved but did not change
    // this branch's tip SHA.
    for br in affected_chain {
        let old_br_tip = &chain_tips[br];
        if let Some(new_br_tip) = new_by_old.get(old_br_tip) {
            git.update_ref(&format!("refs/heads/{br}"), new_br_tip, Some(old_br_tip))
                .map_err(|e| Error::Usage(format!(
                    "could not move `{br}` to absorbed tip ({e}); the ref moved under absorb, re-run"
                )))?;
        }
    }

    // Move HEAD and the index to the new tip while leaving the working tree
    // untouched: the absorbed hunks now read as committed, and only the
    // unabsorbed hunks remain as unstaged modifications.
    git.reset_mixed(&new_tip)?;

    // Collect base.hash updates for each child branch in the rewrite range.
    // These are written through restack_with_recovery's command_deltas so they
    // land in the same transactional store.update() call as the restack result.
    let base_updates: Vec<(String, String)> = affected_chain
        .windows(2)
        .filter_map(|pair| {
            let parent_old_tip = &chain_tips[&pair[0]];
            new_by_old
                .get(parent_old_tip)
                .map(|new_parent_tip| (pair[1].clone(), new_parent_tip.clone()))
        })
        .collect();

    // Restack the upstack onto the absorbed tip. Absorb changes the branch
    // tip(s) but not the recorded base pointers of upstack children, so the
    // deltas closure only needs to write the intermediate base.hash updates.
    let restacked = restack_with_recovery(
        &git,
        &store,
        &mut state,
        &repo,
        &upstack,
        |remaining| recovery::Operation::Restack { remaining },
        &|s| {
            for (br, hash) in &base_updates {
                if let Some(b) = s.branches.get_mut(br) {
                    hash.clone_into(&mut b.base.hash);
                }
            }
        },
    )?;
    clear_conflict_artifacts(&git);
    if let Err(err) = git.checkout(&branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    report_result(format, &branch, &new_tip, &git, &mapping, &restacked, &commit_to_branch);
    Ok(())
}

/// Map each staged hunk to a commit in `absorbable_set` (the union of all
/// commits in the downstack chain) that introduced its edited lines, by blame.
/// A `Modified` hunk whose pre-image lines all blame to one absorbable commit
/// maps to it; otherwise it is left unabsorbed with a reason.
fn map_hunks(
    git: &Git,
    branch: &str,
    hunks: &[Hunk],
    absorbable_set: &std::collections::BTreeSet<&str>,
) -> Result<Mapping, Error> {
    let mut mapped = Vec::new();
    let mut unabsorbed = Vec::new();
    // Blame each pre-image path once; many hunks share a file.
    let mut blames: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for hunk in hunks {
        match hunk.kind {
            HunkKind::Added => {
                unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "added_file" });
                continue;
            }
            HunkKind::Binary => {
                unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "binary" });
                continue;
            }
            HunkKind::Renamed => {
                unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "renamed" });
                continue;
            }
            HunkKind::Deleted => {
                unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "deleted" });
                continue;
            }
            HunkKind::Modified => {}
        }

        // A pure insertion has no pre-image lines to blame: it straddles a
        // commit boundary, which jj treats as ambiguous. Leave it staged.
        if hunk.old_range.count == 0 {
            unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "boundary_insertion" });
            continue;
        }

        let path = hunk.old_path.clone().unwrap_or_else(|| hunk.path.clone());
        if !blames.contains_key(&path) {
            let b = git.blame(branch, &path)?;
            blames.insert(path.clone(), b);
        }
        let blame = &blames[&path];

        // Look up the SHA for each pre-image line in the hunk's old range.
        // `start` is 1-indexed; line n lives at blame[n-1].
        let start = hunk.old_range.start as usize;
        let count = hunk.old_range.count as usize;
        let mut shas = Vec::with_capacity(count);
        let mut out_of_range = false;
        for line in start..start + count {
            match blame.get(line - 1) {
                Some(sha) if !sha.is_empty() => shas.push(sha.clone()),
                _ => {
                    out_of_range = true;
                    break;
                }
            }
        }
        if out_of_range {
            unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "ambiguous" });
            continue;
        }

        // Unanimous blame to a single commit, and that commit is in the
        // absorbable set. Otherwise ambiguous (multi-commit) or outside the chain.
        let first = shas[0].clone();
        if shas.iter().any(|s| s != &first) {
            unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "ambiguous" });
        } else if absorbable_set.contains(first.as_str()) {
            mapped.push(Mapped { hunk: hunk.clone(), commit: first });
        } else {
            unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "outside_branch" });
        }
    }

    Ok(Mapping { mapped, unabsorbed })
}

/// Rebuild a commit chain from `parent_of_first` (exclusive; `None` when the
/// first chain commit is a root), replaying each commit onto its rewritten
/// parent with the assigned hunks spliced into its blobs. Returns the
/// old-commit -> new-commit map (the last chain commit's entry is the new tip).
/// Shared by `absorb` (chain = the full downstack range being rewritten) and
/// `modify --into` (chain = the target's tip plus everything above it).
///
/// Each commit keeps its own tree; the only change is that every file edited by
/// an assigned hunk gets that hunk applied to **every** chain commit from the
/// hunk's target onward (its target index and later). This is sound because the
/// mapping guarantees the hunk's pre-image lines are byte-identical from the
/// target commit through the tip: the same contiguous block exists in each of
/// those commits' blobs, so a content-located splice patches each one without
/// disturbing that commit's own unrelated changes. An ancestor's edit therefore
/// propagates to every descendant (they all carry the edited lines), and a
/// descendant's own edits to other lines survive intact.
pub(crate) fn rewrite_chain(
    git: &Git,
    parent_of_first: Option<&str>,
    own_commits: &[String],
    mapped: &[Mapped],
) -> Result<BTreeMap<String, String>, Error> {
    // Index each chain commit so a hunk applies from its target commit onward.
    let index_of: BTreeMap<&str, usize> = own_commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.as_str(), i))
        .collect();

    // Per file, the assigned hunks with each one's target commit index, so at a
    // given commit we apply exactly the hunks whose target is this commit or an
    // ancestor chain commit (their edited lines exist from there on).
    let mut hunks_by_path: BTreeMap<String, Vec<(usize, &Hunk)>> = BTreeMap::new();
    for m in mapped {
        let path = m.hunk.old_path.clone().unwrap_or_else(|| m.hunk.path.clone());
        let idx = index_of[m.commit.as_str()];
        hunks_by_path.entry(path).or_default().push((idx, &m.hunk));
    }

    let mut new_by_old: BTreeMap<String, String> = BTreeMap::new();
    let mut new_parent: Option<String> = parent_of_first.map(ToString::to_string);
    for (idx, commit) in own_commits.iter().enumerate() {
        let mut entries = git.tree_entries(commit)?;

        for (path, file_hunks) in &hunks_by_path {
            // Only the hunks whose introducing commit is at or below this one:
            // before then their edited lines do not yet exist to splice.
            let active: Vec<&Hunk> = file_hunks
                .iter()
                .filter(|(target, _)| *target <= idx)
                .map(|(_, h)| *h)
                .collect();
            if active.is_empty() {
                continue;
            }
            let Some(content) = git.read_blob(commit, path)? else {
                // Not in this commit (added by a later own commit): skip here.
                continue;
            };
            let Some(patched) = splice_hunks(&content, &active) else {
                return Err(Error::Usage(format!(
                    "could not locate the edited lines of `{path}` in commit {commit}; absorb aborted before mutating"
                )));
            };
            if patched == content {
                continue;
            }
            let new_sha = git.hash_object(patched.as_bytes())?;
            let entry = entries.iter_mut().find(|e| e.path == *path).ok_or_else(|| {
                Error::Usage(format!(
                    "`{path}` is not in commit {commit}'s tree; cannot absorb into it"
                ))
            })?;
            entry.sha = new_sha;
        }

        let new_tree = git.mktree(&entries)?;
        let new_commit = git.commit_tree_like(&new_tree, new_parent.as_deref(), commit)?;
        new_by_old.insert(commit.clone(), new_commit.clone());
        new_parent = Some(new_commit);
    }

    Ok(new_by_old)
}

/// Apply each hunk to `content` by locating its pre-image (removed) lines
/// verbatim and replacing them with its post-image (added) lines. With `-U0`
/// the hunk body is exactly the removed (`-`) then added (`+`) lines, no
/// context, so the pre-image block is the removed lines. Returns `None` if any
/// hunk's removed block is not found (which the blame guarantee should prevent;
/// surfacing it aborts rather than mis-patching). Hunks are applied
/// bottom-to-top so an earlier splice does not shift a later one's match.
fn splice_hunks(content: &str, hunks: &[&Hunk]) -> Option<String> {
    // Split keeping the trailing-newline structure: a file is a sequence of
    // lines each ending in `\n` (the common case). We operate on `\n`-joined
    // lines and rebuild with a trailing newline if the original had one.
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(ToString::to_string).collect();

    // Resolve each hunk to a (match-start, removed-len, replacement-lines) edit,
    // then apply from the bottom so indices stay valid.
    let mut edits: Vec<(usize, usize, Vec<String>)> = Vec::new();
    // Track regions already consumed so two hunks editing identical text land
    // in distinct places rather than both matching the first occurrence.
    let mut consumed: Vec<(usize, usize)> = Vec::new();

    for hunk in hunks {
        let (removed, added) = split_hunk_body(&hunk.body);
        // A `Modified` hunk with a non-empty old range always has removed lines.
        if removed.is_empty() {
            return None;
        }
        let at = find_block(&lines, &removed, &consumed)?;
        consumed.push((at, removed.len()));
        edits.push((at, removed.len(), added));
    }

    // Apply bottom-up so earlier edits do not shift later match positions.
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (at, len, replacement) in edits {
        lines.splice(at..at + len, replacement);
    }

    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    Some(out)
}

/// Split a `-U0` hunk body into its removed (pre-image) and added (post-image)
/// lines, stripping the one-char `-`/`+` prefix. Context lines (` `) and the
/// `\ No newline` marker are not expected with `-U0` and are ignored.
fn split_hunk_body(body: &str) -> (Vec<String>, Vec<String>) {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix('-') {
            removed.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('+') {
            added.push(rest.to_string());
        }
        // ` ` context and `\` markers: ignored.
    }
    (removed, added)
}

/// Find the first index where `needle` appears as a contiguous block in
/// `haystack`, skipping any region already `consumed` (so duplicate edited text
/// maps to distinct occurrences). Returns the start index, or `None` if absent.
fn find_block(
    haystack: &[String],
    needle: &[String],
    consumed: &[(usize, usize)],
) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    'outer: for start in 0..=haystack.len() - needle.len() {
        // Skip a start that overlaps an already-consumed region.
        for &(c_at, c_len) in consumed {
            if start < c_at + c_len && c_at < start + needle.len() {
                continue 'outer;
            }
        }
        if haystack[start..start + needle.len()] == *needle {
            return Some(start);
        }
    }
    None
}

/// Emit the `--dry-run` JSON: the per-hunk mapping, a per-target summary
/// (commit id + branch + hunk count + short subject), and the unabsorbed set
/// with a reason per hunk. Mutates nothing.
fn report_dry_run(
    format: OutputFormat,
    branch: &str,
    chain_commits: &[String],
    commit_to_branch: &BTreeMap<String, String>,
    git: &Git,
    mapping: &Mapping,
) {
    // Per-target summary, ordered by chain position (oldest first).
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for m in &mapping.mapped {
        *counts.entry(m.commit.as_str()).or_default() += 1;
    }
    let mut targets = Vec::new();
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for commit in chain_commits {
        if let Some(n) = counts.get(commit.as_str()) {
            if !seen.insert(commit.as_str()) {
                continue;
            }
            let subject = git.commit_subject(commit).unwrap_or_default();
            let br = commit_to_branch.get(commit).map_or("", String::as_str);
            targets.push(json!({ "commit": commit, "branch": br, "hunks": n, "subject": subject }));
        }
    }

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                json!({
                    "op": "absorb",
                    "dry_run": true,
                    "branch": branch,
                    "mapping": mapping_json(&mapping.mapped, commit_to_branch),
                    "targets": targets,
                    "unabsorbed": unabsorbed_json(&mapping.unabsorbed),
                })
            );
        }
        OutputFormat::Pretty => {
            if mapping.mapped.is_empty() {
                println!("Nothing to absorb.");
            } else {
                println!("Would absorb {} hunk(s):", mapping.mapped.len());
                let mut printed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
                for commit in chain_commits {
                    if let Some(n) = counts.get(commit.as_str()) {
                        if !printed.insert(commit.as_str()) {
                            continue;
                        }
                        let subject = git.commit_subject(commit).unwrap_or_default();
                        let short = &commit[..commit.len().min(8)];
                        let br = commit_to_branch.get(commit).map_or("", String::as_str);
                        println!("  {short} {subject} ({n} hunk(s)) [{br}]");
                    }
                }
            }
            report_unabsorbed_pretty(&mapping.unabsorbed);
        }
    }
}

/// Emit the result of a real absorb (or a no-op when nothing mapped): where
/// each hunk landed, what was restacked, and what was left unabsorbed.
fn report_result(
    format: OutputFormat,
    branch: &str,
    tip: &str,
    git: &Git,
    mapping: &Mapping,
    restacked: &[String],
    commit_to_branch: &BTreeMap<String, String>,
) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for m in &mapping.mapped {
        *counts.entry(m.commit.as_str()).or_default() += 1;
    }

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                json!({
                    "op": "absorb",
                    "branch": branch,
                    "sha": tip,
                    "absorbed": mapping.mapped.len(),
                    "mapping": mapping_json(&mapping.mapped, commit_to_branch),
                    "unabsorbed": unabsorbed_json(&mapping.unabsorbed),
                    "restacked": restacked,
                })
            );
        }
        OutputFormat::Pretty => {
            if mapping.mapped.is_empty() {
                println!("Nothing absorbed.");
            } else {
                println!("Absorbed {} hunk(s) into {branch}:", mapping.mapped.len());
                for (commit, n) in &counts {
                    let subject = git.commit_subject(commit).unwrap_or_default();
                    let short = &commit[..commit.len().min(8)];
                    let br = commit_to_branch.get(*commit).map_or("", String::as_str);
                    println!("  {short} {subject} ({n} hunk(s)) [{br}]");
                }
                for name in restacked {
                    println!("Restacked {name}");
                }
            }
            report_unabsorbed_pretty(&mapping.unabsorbed);
        }
    }
}

/// The `{hunk -> commit}` mapping as JSON: each entry names the file, the
/// pre-image line range, the target commit, and the branch that owns it.
fn mapping_json(mapped: &[Mapped], commit_to_branch: &BTreeMap<String, String>) -> Vec<Value> {
    mapped
        .iter()
        .map(|m| {
            let br = commit_to_branch.get(&m.commit).map_or(String::new(), Clone::clone);
            json!({
                "path": m.hunk.path,
                "old_start": m.hunk.old_range.start,
                "old_count": m.hunk.old_range.count,
                "commit": m.commit,
                "branch": br,
            })
        })
        .collect()
}

/// The unabsorbed set as JSON: each entry names the file, the pre-image line
/// range, and a machine-readable reason.
fn unabsorbed_json(unabsorbed: &[Unabsorbed]) -> Vec<Value> {
    unabsorbed
        .iter()
        .map(|u| {
            json!({
                "path": u.hunk.path,
                "old_start": u.hunk.old_range.start,
                "old_count": u.hunk.old_range.count,
                "reason": u.reason,
            })
        })
        .collect()
}

fn report_unabsorbed_pretty(unabsorbed: &[Unabsorbed]) {
    if unabsorbed.is_empty() {
        return;
    }
    println!("Left staged ({} hunk(s)):", unabsorbed.len());
    for u in unabsorbed {
        println!("  {} ({})", u.hunk.path, u.reason);
    }
}
