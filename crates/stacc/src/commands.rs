//! Implementations of the CLI subcommands.

use std::io::IsTerminal;
use std::path::Path;

use serde_json::json;
use stacc_config::{detect, read_file, resolve, Overrides};
use stacc_core::ops;
use stacc_git::Git;
use stacc_github::{GitHub, NewPullRequest, PrState, PullRequestUpdate};
use stacc_state::{Base, BranchState, PullRequest, RepoConfig, StateStore};

use crate::cli::{CreateArgs, InitArgs, OutputFormat, RenameArgs, SubmitArgs, TrackArgs, UntrackArgs};
use crate::error::Error;

mod absorb;
mod auth;
mod info;
mod log;
mod navigation;
mod operations;
mod removal;
mod reorder;
mod split;

pub use absorb::absorb;
pub use auth::auth;
pub use info::info;
pub use log::log;
pub use navigation::{bottom, checkout, down, top, up};
pub use operations::{
    abort_cmd, continue_cmd, fold, merge, modify, move_cmd, restack, squash, sync, undo,
};
pub use removal::{delete, pop};
pub use reorder::reorder;
pub use split::split;

/// `stacc init`: detect trunk/remote, then record them in the state ref.
pub fn init(args: &InitArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());

    if let Some(repo) = store.load()?.repo {
        report(format, "already_initialized", &repo);
        return Ok(());
    }

    let detected = detect(&git)?;
    let file = read_file(Path::new(".stacc.toml"))?;
    let flags = Overrides {
        trunk: args.trunk.clone(),
        remote: args.remote.clone(),
    };
    let config = resolve(detected, file, flags)?;
    let repo = RepoConfig {
        trunk: config.trunk,
        remote: config.remote,
    };

    // A concurrent init may have won between the check above and here; only set
    // the config if the ref is still uninitialized.
    store.update(|state| {
        if state.repo.is_none() {
            state.repo = Some(repo.clone());
        }
        Ok(())
    })?;

    report(format, "initialized", &repo);
    Ok(())
}

fn report(format: OutputFormat, status: &str, repo: &RepoConfig) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({ "status": status, "trunk": repo.trunk, "remote": repo.remote })
        ),
        OutputFormat::Pretty => {
            let verb = if status == "already_initialized" {
                "Already initialized"
            } else {
                "Initialized"
            };
            println!("{verb} stacc (trunk: {}, remote: {})", repo.trunk, repo.remote);
        }
    }
}

/// `stacc track`: record the current branch and its base in the state ref.
pub fn track(args: &TrackArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());

    let trunk = match store.load()?.repo {
        Some(repo) => repo.trunk,
        None => {
            return Err(Error::Usage(
                "stacc is not initialized; run `stacc init` first".into(),
            ))
        }
    };

    let branch = git.current_branch()?;
    if branch == trunk {
        return Err(Error::Usage(format!("cannot track the trunk branch `{trunk}`")));
    }

    let base = args.base.clone().unwrap_or(trunk);
    let base_hash = git.rev_parse(&base)?;

    store.update(|state| {
        state.branches.insert(
            branch.clone(),
            BranchState {
                base: Base {
                    name: base.clone(),
                    hash: base_hash.clone(),
                },
                pr: None,
            },
        );
        Ok(())
    })?;

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({ "status": "tracked", "branch": branch, "base": base })
        ),
        OutputFormat::Pretty => println!("Tracking {branch} (base: {base})"),
    }
    Ok(())
}

/// `stacc untrack`: drop a branch from the stack, reparenting its children onto
/// the branch's own base so the rest of the stack stays connected. Edits only
/// stacc state, never the git branch or the remote.
pub fn untrack(args: &UntrackArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    // Target the named branch, or the current one.
    let target = match &args.branch {
        Some(branch) => branch.clone(),
        None => git.current_branch().map_err(|_| {
            Error::Usage(
                "cannot resolve the current branch on a detached HEAD; pass a branch name".into(),
            )
        })?,
    };

    if target == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot untrack the trunk branch `{}`",
            repo.trunk
        )));
    }
    if !state.branches.contains_key(&target) {
        return Err(Error::Usage(format!("branch `{target}` is not tracked")));
    }

    // Remove the branch and reparent its children onto its base, re-evaluated
    // against fresh state so a concurrent change to another branch survives. A
    // `None` result means a concurrent untrack already removed it.
    let Some((base, reparented)) = store.update(|state| {
        let Some(removed) = state.branches.remove(&target) else {
            return Ok(None);
        };
        let base = removed.base.name;
        let mut reparented: Vec<String> = Vec::new();
        for (name, branch) in &mut state.branches {
            if branch.base.name == target {
                branch.base.name.clone_from(&base);
                reparented.push(name.clone());
            }
        }
        Ok(Some((base, reparented)))
    })?
    else {
        return Ok(());
    };

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "status": "untracked",
                "branch": target,
                "base": base,
                "reparented": reparented,
            })
        ),
        OutputFormat::Pretty => {
            println!("Untracked {target}");
            if !reparented.is_empty() {
                println!("  reparented onto {base}: {}", reparented.join(", "));
            }
        }
    }
    Ok(())
}

/// `stacc create`: create a new branch stacked on the current one, commit any
/// staged changes, and track it. The base is the current branch (the trunk when
/// starting a stack). Refuses only on a detached HEAD.
pub fn create(args: &CreateArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    // Refuse names that would shadow the trunk or clobber a tracked branch
    // (which would silently drop its recorded PR), before mutating anything.
    if args.name == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot create the trunk branch `{}`",
            repo.trunk
        )));
    }
    if state.branches.contains_key(&args.name) {
        return Err(Error::Usage(format!(
            "branch `{}` is already tracked",
            args.name
        )));
    }

    let base = git.current_branch().map_err(|_| {
        Error::Usage(
            "cannot create a branch from a detached HEAD; check out a branch first".into(),
        )
    })?;
    let base_hash = git.rev_parse(&base)?;

    git.checkout_new_branch(&args.name)?;

    // Track the branch before committing so a failing commit (e.g. a pre-commit
    // hook) can't strand it untracked; the staged changes survive for a retry.
    store
        .update(|state| {
            state.branches.insert(
                args.name.clone(),
                BranchState {
                    base: Base {
                        name: base.clone(),
                        hash: base_hash.clone(),
                    },
                    pr: None,
                },
            );
            Ok(())
        })
        .map_err(|e| {
            Error::Usage(format!(
                "created branch `{}` but could not save state: {e}; run `stacc track` to recover",
                args.name
            ))
        })?;

    let (committed, sha) = if git.has_staged_changes()? {
        let message = args.message.clone().unwrap_or_else(|| args.name.clone());
        git.commit(&message)?;
        (true, Some(git.rev_parse("HEAD")?))
    } else {
        (false, None)
    };

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "status": "created",
                "branch": args.name,
                "base": base,
                "committed": committed,
                "sha": sha,
            })
        ),
        OutputFormat::Pretty => {
            let suffix = if committed {
                " (committed staged changes)"
            } else {
                ""
            };
            println!("Created {} (base: {base}){suffix}", args.name);
        }
    }
    Ok(())
}

/// `stacc status`: the current branch's position in the stack and its PR state.
pub fn status(format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    let branch = git.current_branch()?;

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

    let children: Vec<&str> = state
        .branches
        .iter()
        .filter(|(_, b)| b.base.name == branch)
        .map(|(name, _)| name.as_str())
        .collect();

    // Fetch the live PR state only when a PR is recorded for this branch.
    let pr = match &branch_state.pr {
        Some(pr) => {
            let url = git.remote_url(&repo.remote)?;
            let (owner, repo_name) = stacc_github::parse_remote(&url).ok_or_else(|| {
                Error::Usage(format!("remote `{}` is not a GitHub URL", repo.remote))
            })?;
            let live = GitHub::from_env()?.get_pull_request(&owner, &repo_name, pr.number)?;
            Some((pr.number, live.state))
        }
        None => None,
    };

    match format {
        OutputFormat::Json => {
            let pr_json =
                pr.map(|(number, state)| json!({ "number": number, "state": pr_state_str(state) }));
            println!(
                "{}",
                json!({
                    "branch": branch,
                    "base": branch_state.base.name,
                    "children": children,
                    "pr": pr_json,
                })
            );
        }
        OutputFormat::Pretty => {
            println!("{branch} (base: {})", branch_state.base.name);
            if let Some((number, state)) = pr {
                println!("  PR #{number}: {}", pr_state_str(state));
            }
            if !children.is_empty() {
                println!("  children: {}", children.join(", "));
            }
        }
    }
    Ok(())
}

pub(crate) fn pr_state_str(state: PrState) -> &'static str {
    match state {
        PrState::Open => "open",
        PrState::Closed => "closed",
        PrState::Merged => "merged",
    }
}

/// `stacc pr`: print the current branch's recorded PR URL, and open it in a
/// browser when run on a terminal. Errors when the branch has no recorded PR.
pub fn pr(format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    let branch = git.current_branch().map_err(|_| {
        Error::Usage("cannot resolve a PR for a detached HEAD; check out a branch first".into())
    })?;
    let pr = state
        .branches
        .get(&branch)
        .and_then(|b| b.pr.clone())
        .ok_or_else(|| {
            Error::Usage(format!(
                "no PR recorded for `{branch}`; run `stacc submit` first"
            ))
        })?;

    // Prefer the recorded URL; build one from the remote when it is absent.
    let url = if let Some(url) = pr.url {
        url
    } else {
        let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
            .ok_or_else(|| Error::Usage(format!("remote `{}` is not a GitHub URL", repo.remote)))?;
        format!("https://github.com/{owner}/{repo_name}/pull/{}", pr.number)
    };

    match format {
        OutputFormat::Json => {
            println!("{}", json!({ "branch": branch, "number": pr.number, "url": url }));
        }
        OutputFormat::Pretty => {
            println!("{url}");
            if std::io::stdout().is_terminal() {
                open_in_browser(&url);
            }
        }
    }
    Ok(())
}

/// Best-effort: open `url` in the platform browser, ignoring any failure.
fn open_in_browser(url: &str) {
    let mut command = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `stacc submit`: push the current branch and its ancestors up to the trunk,
/// creating or updating each branch's PR with its parent as the base.
// A cohesive validate -> push/PR loop -> persist -> report sequence; splitting it
// would only trade this lint for `too_many_arguments` on a helper.
#[allow(clippy::too_many_lines)]
pub fn submit(args: &SubmitArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    let current = git.current_branch()?;
    if current == repo.trunk {
        return Err(Error::Usage("cannot submit the trunk branch".into()));
    }

    // Walk the downstack bottom-up so each PR's base ref is already on the
    // remote when we open the PR (the lowest base is always the trunk).
    let chain = ops::downstack_chain(&state, &current, &repo.trunk)?;

    let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
        .ok_or_else(|| Error::Usage(format!("remote `{}` is not a GitHub URL", repo.remote)))?;
    let github = GitHub::from_env()?;

    // (branch, created?, number, url) for each branch we acted on.
    let mut results: Vec<(String, bool, u64, String)> = Vec::new();
    // The PR records to write back, applied together in one transactional update
    // after the network work so a concurrent change to another branch survives.
    let mut pr_updates: Vec<(String, PullRequest)> = Vec::new();

    for branch in &chain {
        let is_current = branch == &current;
        let base = state
            .branches
            .get(branch)
            .expect("branch is in chain")
            .base
            .name
            .clone();

        git.push_force_with_lease(&repo.remote, branch)?;

        let title = git.commit_subject(branch)?;
        // --description applies to the branch the user is actually submitting;
        // ancestors fall back to their own commit body.
        let body = if is_current {
            match &args.description {
                Some(value) => resolve_description(value)?,
                None => git.commit_body(branch)?,
            }
        } else {
            git.commit_body(branch)?
        };

        let existing = state
            .branches
            .get(branch)
            .and_then(|b| b.pr.as_ref().map(|pr| pr.number));

        let pr = match existing {
            Some(number) => github.update_pull_request(
                &owner,
                &repo_name,
                number,
                &PullRequestUpdate {
                    title: Some(title),
                    body: Some(body),
                    base: Some(base),
                },
            )?,
            None => github.create_pull_request(
                &owner,
                &repo_name,
                &NewPullRequest {
                    title,
                    head: branch.clone(),
                    base,
                    body,
                },
            )?,
        };

        pr_updates.push((
            branch.clone(),
            PullRequest {
                number: pr.number,
                url: Some(pr.url.clone()),
            },
        ));

        results.push((branch.clone(), existing.is_none(), pr.number, pr.url));
    }

    store.update(|state| {
        for (branch, pr) in &pr_updates {
            if let Some(branch_state) = state.branches.get_mut(branch) {
                branch_state.pr = Some(pr.clone());
            }
        }
        Ok(())
    })?;

    match format {
        OutputFormat::Json => {
            let list: Vec<serde_json::Value> = results
                .iter()
                .map(|(branch, created, number, url)| {
                    json!({
                        "status": if *created { "created" } else { "updated" },
                        "branch": branch,
                        "number": number,
                        "url": url,
                    })
                })
                .collect();
            println!("{}", json!({ "submitted": list }));
        }
        OutputFormat::Pretty => {
            for (branch, created, number, url) in &results {
                let verb = if *created { "Created" } else { "Updated" };
                println!("{verb} PR #{number} for {branch}: {url}");
            }
        }
    }
    Ok(())
}

/// Resolve a `--description` value: `@path` reads a file, anything else is literal.
fn resolve_description(value: &str) -> Result<String, Error> {
    match value.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| Error::Usage(format!("failed to read description file `{path}`: {e}"))),
        None => Ok(value.to_string()),
    }
}

/// `stacc rename`: rename the current branch, updating local state, children,
/// and (when it has a recorded PR, so it is on the remote) the remote branch.
/// Renaming a branch with its own open PR closes that PR on GitHub, so it
/// requires `--force` and drops the recorded PR so the next `submit` recreates
/// it.
pub fn rename(args: &RenameArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    let from = git.current_branch().map_err(|_| {
        Error::Usage("cannot rename a detached HEAD; check out a branch first".into())
    })?;
    let to = &args.name;

    if from == repo.trunk {
        return Err(Error::Usage(format!(
            "cannot rename the trunk branch `{}`",
            repo.trunk
        )));
    }
    if to == &repo.trunk {
        return Err(Error::Usage(format!("`{to}` is the trunk branch name")));
    }
    if to.starts_with('-') {
        return Err(Error::Usage(format!("`{to}` is not a valid branch name")));
    }
    if !state.branches.contains_key(&from) {
        return Err(Error::Usage(format!(
            "branch `{from}` is not tracked; run `stacc track` first"
        )));
    }
    if state.branches.contains_key(to) {
        return Err(Error::Usage(format!(
            "a branch named `{to}` is already tracked"
        )));
    }

    // Renaming a branch with its own open PR closes that PR on GitHub. Require
    // --force, and name the PR that will close so the error is actionable.
    let own_pr = state.branches.get(&from).and_then(|b| b.pr.clone());
    if let Some(pr) = &own_pr {
        if !args.force {
            let url = pr.url.as_deref().unwrap_or_default();
            return Err(Error::Usage(format!(
                "renaming `{from}` will close its open PR #{} ({url}); pass --force to rename and recreate it on the next `submit`",
                pr.number
            )));
        }
    }

    // Local rename: move the ref (HEAD follows), the state key, and every
    // child's recorded base. Persist this BEFORE the remote call so a remote
    // failure leaves a consistent (renamed, PR-still-recorded) state to
    // re-`submit` from rather than a half-applied one.
    git.rename_branch(&from, to)?;
    store
        .update(|state| {
            if let Some(moved) = state.branches.remove(&from) {
                state.branches.insert(to.clone(), moved);
                for branch in state.branches.values_mut() {
                    if branch.base.name == from {
                        branch.base.name.clone_from(to);
                    }
                }
            }
            Ok(())
        })
        .map_err(|e| {
            Error::Usage(format!(
                "renamed `{from}` locally but could not save state: {e}; run `stacc track` on `{to}` to recover"
            ))
        })?;

    // Remote rename only when the branch was on the remote (it had a PR). The
    // API retargets child base-PRs but closes this branch's own PR, so on
    // success drop the record (the next `submit` recreates it) and save again;
    // on failure KEEP the record so the next `submit` reconciles the still-open
    // PR instead of orphaning it.
    let mut remote_renamed = false;
    let mut pr_closed = None;
    if own_pr.is_some() {
        match rename_remote_branch(&git, &repo, &from, to) {
            Ok(()) => {
                remote_renamed = true;
                pr_closed.clone_from(&own_pr);
                if let Err(err) = store.update(|state| {
                    if let Some(branch) = state.branches.get_mut(to) {
                        branch.pr = None;
                    }
                    Ok(())
                }) {
                    eprintln!(
                        "warning: renamed `{to}` on the remote and closed its PR, but could not drop the local PR record ({err}); run `stacc submit` to reconcile"
                    );
                }
            }
            Err(err) => eprintln!(
                "warning: renamed locally, but the remote branch rename failed ({err}); the open PR is unchanged, rename it on the remote by hand or re-`submit`"
            ),
        }
    }

    report_rename(format, &from, to, pr_closed.as_ref(), remote_renamed);
    Ok(())
}

fn rename_remote_branch(git: &Git, repo: &RepoConfig, from: &str, to: &str) -> Result<(), Error> {
    let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
        .ok_or_else(|| Error::Usage(format!("remote `{}` is not a GitHub URL", repo.remote)))?;
    let github = GitHub::from_env()?;
    github.rename_branch(&owner, &repo_name, from, to)?;
    Ok(())
}

fn report_rename(
    format: OutputFormat,
    from: &str,
    to: &str,
    pr_closed: Option<&PullRequest>,
    remote_renamed: bool,
) {
    match format {
        OutputFormat::Json => {
            let closed = pr_closed.map(|pr| json!({ "number": pr.number, "url": pr.url }));
            println!(
                "{}",
                json!({
                    "op": "rename",
                    "branch": to,
                    "from": from,
                    "to": to,
                    "remote_renamed": remote_renamed,
                    "closed_pr": closed,
                })
            );
        }
        OutputFormat::Pretty => {
            println!("Renamed {from} to {to}");
            if let Some(pr) = pr_closed {
                println!("Closed PR #{} (re-submit to recreate it)", pr.number);
            }
        }
    }
}
