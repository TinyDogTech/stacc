//! `stacc info`: read-only per-branch detail.
//!
//! The default fields are cheap (state plus local git): the recorded base, the
//! head sha, a live needs-restack check, parent/children, a diffstat against
//! the base, and the recorded PR number/url. The heavy fields are gated:
//! `--diff` and `--patch` include the diff/patch text, `--body` fetches the PR
//! title/state/body from GitHub best-effort. No state writes, no ref
//! mutations, and no network on the default path.

use serde_json::json;
use stacc_core::ops;
use stacc_git::{CommitInfo, DiffStat, Git};
use stacc_github::GitHub;
use stacc_state::{BranchState, RepoConfig, StateStore};

use crate::cli::{InfoArgs, OutputFormat};
use crate::error::Error;

/// Everything `info` reports for a tracked branch, gathered once and rendered
/// as JSON or pretty. The live-base facts (`needs_restack`, `commits`,
/// `diffstat`, and the gated diff/patch) are `None` when the base ref no
/// longer resolves, so a dangling base degrades to nulls rather than failing
/// a read-only probe.
struct Details {
    branch: String,
    base_name: String,
    base_hash: String,
    head: String,
    children: Vec<String>,
    needs_restack: Option<bool>,
    commits: Option<usize>,
    commit: Option<CommitInfo>,
    diffstat: Option<DiffStat>,
    /// `--diff`: the branch's diff against its base.
    diff: Option<String>,
    /// `--patch`: the branch's per-commit patches (`git log -p`).
    patch: Option<String>,
    pr: Option<stacc_state::PullRequest>,
    /// The live PR (title/state/body), when `--body` fetched it.
    pr_live: Option<stacc_github::PullRequest>,
    /// `--body` outcome when a PR is recorded: `"ok"` or `"failed"`.
    pr_fetch: Option<&'static str>,
}

/// `stacc info`: show a branch's stack detail. Read-only.
pub fn info(args: &InfoArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    let branch = match &args.branch {
        Some(branch) => branch.clone(),
        None => git.current_branch().map_err(|_| {
            Error::Usage(
                "cannot resolve the current branch on a detached HEAD; pass a branch name".into(),
            )
        })?,
    };

    // The trunk and untracked branches are a clear structured result, not an
    // error: scripts probe branches with `info` (mirrors `status`).
    if branch == repo.trunk {
        match format {
            OutputFormat::Json => println!("{}", json!({ "branch": branch, "trunk": true })),
            OutputFormat::Pretty => println!("{branch} (trunk)"),
        }
        return Ok(());
    }
    let Some(branch_state) = state.branches.get(&branch) else {
        match format {
            OutputFormat::Json => println!("{}", json!({ "branch": branch, "tracked": false })),
            OutputFormat::Pretty => println!("{branch} (not tracked)"),
        }
        return Ok(());
    };

    let children = ops::children(&state.branches, &branch);
    let details = gather(args, &git, &repo, &branch, branch_state, children)?;
    match format {
        OutputFormat::Json => render_json(&details),
        OutputFormat::Pretty => render_pretty(&details),
    }
    Ok(())
}

/// Collect the branch's detail from state and local git, plus the gated
/// extras the flags asked for.
fn gather(
    args: &InfoArgs,
    git: &Git,
    repo: &RepoConfig,
    branch: &str,
    branch_state: &BranchState,
    children: Vec<String>,
) -> Result<Details, Error> {
    let base = &branch_state.base;
    let head = git.rev_parse(branch)?;

    // Live-base facts. The needs-restack predicate is the same one squash/fold
    // and the restack engine use: the branch is current iff the base's live
    // tip is an ancestor of the branch tip. The diffstat/diff/patch run from
    // the merge base, which IS the live base tip once restacked and still
    // covers exactly the branch's own changes when it has drifted.
    let mut needs_restack = None;
    let mut commits = None;
    let mut diffstat = None;
    let mut diff = None;
    let mut patch = None;
    if let Some(base_tip) = git.ref_commit(&base.name)? {
        needs_restack = Some(!git.is_ancestor(&base_tip, &head)?);
        let fork = git.merge_base(&base.name, branch)?;
        commits = Some(git.ahead_behind(&base.name, branch)?.0);
        diffstat = Some(git.diffstat(&fork, branch)?);
        if args.diff {
            diff = Some(git.diff_text(&fork, branch)?);
        }
        if args.patch {
            patch = Some(git.log_patch(&fork, branch)?);
        }
    }

    // `--body`: fetch the PR detail best-effort. A failure leaves the recorded
    // number/url untouched and marks the fetch failed, never fails the command.
    let pr = branch_state.pr.clone();
    let (pr_live, pr_fetch) = match (&pr, args.body) {
        (Some(pr), true) => match fetch_pr(git, repo, pr.number) {
            Some(live) => (Some(live), Some("ok")),
            None => (None, Some("failed")),
        },
        _ => (None, None),
    };

    Ok(Details {
        branch: branch.to_string(),
        base_name: base.name.clone(),
        base_hash: base.hash.clone(),
        head,
        children,
        needs_restack,
        commits,
        commit: git.commit_info(branch).ok(),
        diffstat,
        diff,
        patch,
        pr,
        pr_live,
        pr_fetch,
    })
}

/// Best-effort PR fetch for `--body`: `None` on any failure (no token, a
/// non-GitHub remote, a network or API error).
fn fetch_pr(git: &Git, repo: &RepoConfig, number: u64) -> Option<stacc_github::PullRequest> {
    let url = git.remote_url(&repo.remote).ok()?;
    let (owner, repo_name) = stacc_github::parse_remote(&url)?;
    let github = GitHub::from_env().ok()?;
    github.get_pull_request(&owner, &repo_name, number).ok()
}

/// The JSON object: every pretty field is present; the heavy fields (`diff`,
/// `patch`, the PR body and `pr_fetch`) appear only when their flag was set.
fn render_json(d: &Details) {
    let pr = d.pr.as_ref().map(|pr| {
        let mut obj = json!({ "number": pr.number, "url": pr.url });
        if let Some(live) = &d.pr_live {
            obj["title"] = json!(live.title);
            obj["state"] = json!(super::pr_state_str(live.state));
            obj["body"] = json!(live.body);
        }
        obj
    });
    let mut obj = json!({
        "branch": d.branch,
        "tracked": true,
        "base": { "name": d.base_name, "hash": d.base_hash },
        "head": d.head,
        "parent": d.base_name,
        "children": d.children,
        "needs_restack": d.needs_restack,
        "commits": d.commits,
        "commit": d.commit.as_ref().map(|c| {
            json!({ "sha": c.sha, "subject": c.subject, "age": c.age })
        }),
        "diffstat": d.diffstat.map(|s| {
            json!({ "files": s.files, "insertions": s.insertions, "deletions": s.deletions })
        }),
        "pr": pr,
    });
    if let Some(diff) = &d.diff {
        obj["diff"] = json!(diff);
    }
    if let Some(patch) = &d.patch {
        obj["patch"] = json!(patch);
    }
    if let Some(fetch) = d.pr_fetch {
        obj["pr_fetch"] = json!(fetch);
    }
    println!("{obj}");
}

/// The compact human layout, mirroring `status`'s header style.
fn render_pretty(d: &Details) {
    println!("{} (base: {})", d.branch, d.base_name);
    println!("  Parent:   {}", d.base_name);
    if !d.children.is_empty() {
        println!("  Children: {}", d.children.join(", "));
    }
    match &d.commit {
        Some(c) => println!("  Head:     {} - {} ({})", c.sha, c.subject, c.age),
        None => println!("  Head:     {}", d.head),
    }
    if let Some(commits) = d.commits {
        println!("  Commits:  {commits}");
    }
    if let Some(s) = d.diffstat {
        let noun = if s.files == 1 { "file" } else { "files" };
        println!(
            "  Diffstat: {} {noun} changed, +{} -{}",
            s.files, s.insertions, s.deletions
        );
    }
    if let Some(pr) = &d.pr {
        let mut line = format!("  PR:       #{}", pr.number);
        if let Some(live) = &d.pr_live {
            line.push_str(" (");
            line.push_str(super::pr_state_str(live.state));
            line.push(')');
        }
        if let Some(url) = &pr.url {
            line.push(' ');
            line.push_str(url);
        }
        println!("{line}");
    }
    if d.pr_fetch == Some("failed") {
        println!("  (PR fetch failed; showing the recorded PR)");
    }
    if d.needs_restack == Some(true) {
        println!("  needs restack");
    }
    if let Some(live) = &d.pr_live {
        println!("\n{}", live.title);
        if !live.body.is_empty() {
            println!("\n{}", live.body);
        }
    }
    if let Some(diff) = &d.diff {
        if !diff.is_empty() {
            println!("\n{diff}");
        }
    }
    if let Some(patch) = &d.patch {
        if !patch.is_empty() {
            println!("\n{patch}");
        }
    }
}
