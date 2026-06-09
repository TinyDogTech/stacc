//! `stacc absorb`: distribute the staged hunks into the downstack commits that
//! introduced the lines each hunk edits (by blame), applied as in-memory tree
//! rewrites, then restack the upstack. Ambiguous and unsupported hunks are left
//! staged and reported, never prompted or silently dropped (KTD-2).
//!
//! The mapping is by *blame*, not apply-check: a hunk's context applies cleanly
//! to many ancestor commits, so "first clean apply" silently rewrites the wrong
//! commit. We blame the branch tip's content over the hunk's pre-image line
//! range and only map when every edited line blames to one commit that is one of
//! the branch's own commits.
//!
//! The apply is an in-memory `commit-tree` rewrite of the branch's own commit
//! chain, never a `git rebase`/autosquash: a non-stacc rebase strands the repo
//! and `stacc abort` refuses to clean it up. After moving the branch ref to the
//! rewritten tip, a `reset --mixed` leaves the absorbed hunks reading as
//! committed and only the unabsorbed hunks as unstaged modifications.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use stacc_core::{ops, recovery};
use stacc_git::{Git, Hunk, HunkKind};
use stacc_state::StateStore;

use super::operations::{clear_conflict_artifacts, guard_worktree, restack_with_recovery};
use crate::cli::{AbsorbArgs, OutputFormat};
use crate::error::Error;

/// A hunk that mapped to one of the branch's own commits.
struct Mapped {
    hunk: Hunk,
    /// The full SHA of the commit the hunk's lines blame to.
    commit: String,
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

/// `stacc absorb`: see the module docs. Maps staged hunks to the branch's own
/// commits by blame, rewrites those commits' trees in memory, moves the branch
/// ref, `reset --mixed`es to leave only the unabsorbed hunks unstaged, and
/// restacks the upstack. `--dry-run` emits the mapping and mutates nothing.
pub fn absorb(args: &AbsorbArgs, format: OutputFormat) -> Result<(), Error> {
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
        Error::Usage("cannot absorb on a detached HEAD; check out a branch first".into())
    })?;
    if branch == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot absorb on the trunk branch `{}`",
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

    if !git.has_staged_changes()? {
        return Err(Error::Usage(
            "nothing staged to absorb; stage the changes you want distributed first".into(),
        ));
    }

    let tip = git.rev_parse(&branch)?;

    // The branch's own commits, oldest-first: the only commits absorb may
    // rewrite. Defined as `base..tip`, using the recorded base hash when it is
    // still an ancestor of the tip (the common case), otherwise the live base
    // tip, so a drifted recorded hash does not under- or over-count the set.
    let exclude = if git.is_ancestor(&base.hash, &branch).unwrap_or(false) {
        base.hash.clone()
    } else {
        git.rev_parse(&base.name)?
    };
    let own_commits = git.rev_list(&exclude, &tip)?;
    if own_commits.is_empty() {
        return Err(Error::Usage(format!(
            "`{branch}` has no commits of its own above `{}` to absorb into",
            base.name
        )));
    }
    let own_set: std::collections::BTreeSet<&str> =
        own_commits.iter().map(String::as_str).collect();

    // Map every staged hunk to a branch-own commit (or report why it can't be).
    let hunks = git.diff_hunks()?;
    let mapping = map_hunks(&git, &branch, &hunks, &own_set)?;

    if args.dry_run {
        report_dry_run(format, &branch, &own_commits, &git, &mapping);
        return Ok(());
    }

    if mapping.mapped.is_empty() {
        // Nothing absorbable: leave the staged changes exactly where they are
        // (no ref move, no reset) so the repo is untouched, never half-rewritten.
        report_result(format, &branch, &tip, &git, &mapping, &[]);
        return Ok(());
    }

    // Fail fast if any branch we would restack is checked out elsewhere, BEFORE
    // mutating anything (mirrors `modify`/`move`).
    let upstack = ops::upstack_order(&state.branches, &branch);
    guard_worktree(&git, &upstack)?;

    // Rewrite the branch's own commit chain in memory, landing each commit's
    // assigned hunks, and move the branch ref to the new tip under the old tip
    // as a lease.
    let new_tip = rewrite_chain(&git, &exclude, &own_commits, &mapping)?;
    git.update_ref(&format!("refs/heads/{branch}"), &new_tip, Some(&tip))
        .map_err(|e| {
            Error::Usage(format!(
                "could not move `{branch}` to the absorbed tip ({e}); the branch tip moved under absorb, re-run it"
            ))
        })?;

    // Move HEAD and the index to the new tip while leaving the working tree
    // untouched: the absorbed hunks now read as committed, and only the
    // unabsorbed hunks remain as unstaged modifications.
    git.reset_mixed(&new_tip)?;

    // Restack the upstack onto the absorbed tip. Like `modify`, absorb changes
    // only the branch tip, not any recorded base pointer, so the deltas closure
    // is a no-op and the continuation is a plain `Restack`.
    let restacked = restack_with_recovery(
        &git,
        &store,
        &mut state,
        &repo,
        &upstack,
        |remaining| recovery::Operation::Restack { remaining },
        &|_s| {},
    )?;
    clear_conflict_artifacts(&git);
    if let Err(err) = git.checkout(&branch) {
        eprintln!("warning: could not switch back to `{branch}`: {err}");
    }

    report_result(format, &branch, &new_tip, &git, &mapping, &restacked);
    Ok(())
}

/// Map each staged hunk to the branch-own commit that introduced its edited
/// lines, by blame. A `Modified` hunk whose pre-image lines all blame to one
/// own commit maps to it; otherwise it is left unabsorbed with a reason. A
/// non-`Modified` kind is unsupported and reported as such.
fn map_hunks(
    git: &Git,
    branch: &str,
    hunks: &[Hunk],
    own_set: &std::collections::BTreeSet<&str>,
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

        let path = hunk
            .old_path
            .clone()
            .unwrap_or_else(|| hunk.path.clone());
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

        // Unanimous blame to a single commit, and that commit is one of the
        // branch's own. Otherwise ambiguous (multi-commit) or outside-branch.
        let first = shas[0].clone();
        if shas.iter().any(|s| s != &first) {
            unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "ambiguous" });
        } else if own_set.contains(first.as_str()) {
            mapped.push(Mapped { hunk: hunk.clone(), commit: first });
        } else {
            unabsorbed.push(Unabsorbed { hunk: hunk.clone(), reason: "outside_branch" });
        }
    }

    Ok(Mapping { mapped, unabsorbed })
}

/// Rebuild the branch's own commit chain from `parent_of_first` (exclusive),
/// replaying each commit onto its rewritten parent with the absorbed hunks
/// spliced into its blobs. Returns the new tip.
///
/// Each commit keeps its own tree; the only change is that every file edited by
/// an absorbed hunk gets that hunk applied to **every** own commit from the
/// hunk's target onward (its target index and later). This is sound because the
/// blame mapping guarantees the hunk's pre-image lines are byte-identical from
/// the introducing commit through the tip: the same contiguous block exists in
/// each of those commits' blobs, so a content-located splice patches each one
/// without disturbing that commit's own unrelated changes. An ancestor's
/// absorbed edit therefore propagates to every descendant (they all carry the
/// edited lines), and a descendant's own edits to other lines survive intact.
fn rewrite_chain(
    git: &Git,
    parent_of_first: &str,
    own_commits: &[String],
    mapping: &Mapping,
) -> Result<String, Error> {
    // Index each own commit so a hunk applies from its target commit onward.
    let index_of: BTreeMap<&str, usize> = own_commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.as_str(), i))
        .collect();

    // Per file, the absorbed hunks with each one's target commit index, so at a
    // given commit we apply exactly the hunks whose target is this commit or an
    // ancestor own commit (their edited lines exist from there on).
    let mut hunks_by_path: BTreeMap<String, Vec<(usize, &Hunk)>> = BTreeMap::new();
    for m in &mapping.mapped {
        let path = m.hunk.old_path.clone().unwrap_or_else(|| m.hunk.path.clone());
        let idx = index_of[m.commit.as_str()];
        hunks_by_path.entry(path).or_default().push((idx, &m.hunk));
    }

    let mut new_parent = parent_of_first.to_string();
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
        new_parent = git.commit_tree_like(&new_tree, Some(&new_parent), commit)?;
    }

    Ok(new_parent)
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
/// (commit id + hunk count + short subject), and the unabsorbed set with a
/// reason per hunk. Mutates nothing.
fn report_dry_run(
    format: OutputFormat,
    branch: &str,
    own_commits: &[String],
    git: &Git,
    mapping: &Mapping,
) {
    // Per-target summary, grouped and ordered by the branch's own commit order.
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for m in &mapping.mapped {
        *counts.entry(m.commit.as_str()).or_default() += 1;
    }
    let mut targets = Vec::new();
    for commit in own_commits {
        if let Some(n) = counts.get(commit.as_str()) {
            let subject = git.commit_subject(commit).unwrap_or_default();
            targets.push(json!({ "commit": commit, "hunks": n, "subject": subject }));
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
                    "mapping": mapping_json(&mapping.mapped),
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
                for commit in own_commits {
                    if let Some(n) = counts.get(commit.as_str()) {
                        let subject = git.commit_subject(commit).unwrap_or_default();
                        let short = &commit[..commit.len().min(8)];
                        println!("  {short} {subject} ({n} hunk(s))");
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
                    "mapping": mapping_json(&mapping.mapped),
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
                    println!("  {short} {subject} ({n} hunk(s))");
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
/// pre-image line range, and the target commit.
fn mapping_json(mapped: &[Mapped]) -> Vec<Value> {
    mapped
        .iter()
        .map(|m| {
            json!({
                "path": m.hunk.path,
                "old_start": m.hunk.old_range.start,
                "old_count": m.hunk.old_range.count,
                "commit": m.commit,
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
