//! Implementations of the CLI subcommands.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};
use stacc_config::{detect, read_file, resolve, Overrides};
use stacc_core::ops;
use stacc_git::Git;
use stacc_github::{GitHub, NewPullRequest, PrState, PullRequestUpdate};
use stacc_state::{Base, BranchState, PullRequest, RepoConfig, StateStore};

use crate::cli::{CreateArgs, InitArgs, LogArgs, OutputFormat, SubmitArgs, TrackArgs};
use crate::error::Error;

mod auth;
mod navigation;
mod operations;

pub use auth::auth;
pub use navigation::{bottom, checkout, down, top, up};
pub use operations::{abort_cmd, continue_cmd, modify, restack, sync};

/// `stacc init`: detect trunk/remote, then record them in the state ref.
pub fn init(args: &InitArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());

    let mut state = store.load()?;
    if let Some(repo) = &state.repo {
        report(format, "already_initialized", repo);
        return Ok(());
    }

    let detected = detect(&git)?;
    let file = read_file(Path::new(".stacc.toml"))?;
    let flags = Overrides {
        trunk: args.trunk.clone(),
        remote: args.remote.clone(),
    };
    let config = resolve(detected, file, flags)?;

    state.repo = Some(RepoConfig {
        trunk: config.trunk,
        remote: config.remote,
    });
    store.save(&state)?;

    report(format, "initialized", state.repo.as_ref().expect("just set"));
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

    let mut state = store.load()?;
    let trunk = match &state.repo {
        Some(repo) => repo.trunk.clone(),
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

    state.branches.insert(
        branch.clone(),
        BranchState {
            base: Base {
                name: base.clone(),
                hash: base_hash,
            },
            pr: None,
        },
    );
    store.save(&state)?;

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({ "status": "tracked", "branch": branch, "base": base })
        ),
        OutputFormat::Pretty => println!("Tracking {branch} (base: {base})"),
    }
    Ok(())
}

/// `stacc create`: create a new branch stacked on the current one, commit any
/// staged changes, and track it. The base is the current branch (the trunk when
/// starting a stack). Refuses only on a detached HEAD.
pub fn create(args: &CreateArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
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
    state.branches.insert(
        args.name.clone(),
        BranchState {
            base: Base {
                name: base.clone(),
                hash: base_hash,
            },
            pr: None,
        },
    );
    store.save(&state).map_err(|e| {
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

/// `stacc log`: render the tracked stack from the state ref.
pub fn log(args: &LogArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;

    let trunk = match &state.repo {
        Some(repo) => repo.trunk.clone(),
        None => {
            return Err(Error::Usage(
                "stacc is not initialized; run `stacc init` first".into(),
            ))
        }
    };

    // Group tracked branches by the base they're stacked on.
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, branch) in &state.branches {
        children
            .entry(branch.base.name.as_str())
            .or_default()
            .push(name.as_str());
    }

    match format {
        // JSON is a stable machine contract: do not change its shape here.
        OutputFormat::Json => {
            let stack = stack_json(&trunk, &children, &state.branches);
            println!("{}", json!({ "trunk": trunk, "stack": stack }));
        }
        OutputFormat::Pretty => {
            let current = git.current_branch().unwrap_or_default();
            if args.short {
                for name in ops::topo_order(&state.branches, &trunk) {
                    let base = state
                        .branches
                        .get(&name)
                        .map_or(trunk.as_str(), |b| b.base.name.as_str());
                    let glyph = if name == current { "*" } else { "o" };
                    println!("{glyph} {}", branch_line(&git, &name, base, &state.branches));
                }
            } else {
                let trunk_glyph = if current == trunk { "* " } else { "" };
                println!("{trunk_glyph}{trunk}");
                print_graph(&git, &trunk, &children, &state.branches, &current, 1);
            }
        }
    }
    Ok(())
}

fn print_graph(
    git: &Git,
    node: &str,
    children: &BTreeMap<&str, Vec<&str>>,
    branches: &BTreeMap<String, BranchState>,
    current: &str,
    depth: usize,
) {
    // A valid tree path can't be longer than the branch count; a deeper path
    // means malformed cyclic state, so bail rather than recurse forever.
    if depth > branches.len() + 1 {
        return;
    }
    let Some(kids) = children.get(node) else {
        return;
    };
    for &kid in kids {
        let indent = "  ".repeat(depth);
        let glyph = if kid == current { "*" } else { "o" };
        println!("{indent}{glyph} {}", branch_line(git, kid, node, branches));
        print_graph(git, kid, children, branches, current, depth + 1);
    }
}

/// One branch's label: its name, recorded PR number, and a needs-restack marker.
fn branch_line(
    git: &Git,
    name: &str,
    base: &str,
    branches: &BTreeMap<String, BranchState>,
) -> String {
    let pr = branches
        .get(name)
        .and_then(|b| b.pr.as_ref())
        .map(|p| format!(" (#{})", p.number))
        .unwrap_or_default();
    let restack = if needs_restack(git, name, base) {
        "  (needs restack)"
    } else {
        ""
    };
    format!("{name}{pr}{restack}")
}

/// Whether `branch` has drifted off `base` (its base tip is no longer an
/// ancestor). Any git lookup failure is treated as up-to-date, so `log` never
/// raises a false alarm on a transient error.
fn needs_restack(git: &Git, branch: &str, base: &str) -> bool {
    let Ok(base_tip) = git.rev_parse(base) else {
        return false;
    };
    !git.is_ancestor(&base_tip, branch).unwrap_or(true)
}

fn stack_json(
    node: &str,
    children: &BTreeMap<&str, Vec<&str>>,
    branches: &BTreeMap<String, BranchState>,
) -> Vec<Value> {
    let Some(kids) = children.get(node) else {
        return Vec::new();
    };
    kids.iter()
        .map(|&kid| {
            let pr = branches.get(kid).and_then(|b| b.pr.as_ref()).map(|p| p.number);
            json!({
                "name": kid,
                "base": node,
                "pr": pr,
                "children": stack_json(kid, children, branches),
            })
        })
        .collect()
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

fn pr_state_str(state: PrState) -> &'static str {
    match state {
        PrState::Open => "open",
        PrState::Closed => "closed",
        PrState::Merged => "merged",
    }
}

/// `stacc submit`: push the current branch and its ancestors up to the trunk,
/// creating or updating each branch's PR with its parent as the base.
pub fn submit(args: &SubmitArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
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

        if let Some(branch_state) = state.branches.get_mut(branch) {
            branch_state.pr = Some(PullRequest {
                number: pr.number,
                url: Some(pr.url.clone()),
            });
        }

        results.push((branch.clone(), existing.is_none(), pr.number, pr.url));
    }

    store.save(&state)?;

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
