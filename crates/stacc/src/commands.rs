//! Implementations of the CLI subcommands.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};
use stacc_config::{detect, read_file, resolve, Overrides};
use stacc_git::Git;
use stacc_github::{GitHub, PrState};
use stacc_state::{Base, BranchState, RepoConfig, StateStore};

use crate::cli::{InitArgs, OutputFormat, TrackArgs};
use crate::error::Error;

/// `stacc init` — detect trunk/remote, then record them in the state ref.
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

/// `stacc track` — record the current branch and its base in the state ref.
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

/// `stacc log` — render the tracked stack from the state ref.
pub fn log(format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git);
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
        OutputFormat::Json => {
            let stack = stack_json(&trunk, &children, &state.branches);
            println!("{}", json!({ "trunk": trunk, "stack": stack }));
        }
        OutputFormat::Pretty => {
            println!("{trunk}");
            print_stack(&trunk, &children, &state.branches, 1);
        }
    }
    Ok(())
}

fn print_stack(
    node: &str,
    children: &BTreeMap<&str, Vec<&str>>,
    branches: &BTreeMap<String, BranchState>,
    depth: usize,
) {
    let Some(kids) = children.get(node) else {
        return;
    };
    for &kid in kids {
        let indent = "  ".repeat(depth);
        let pr = branches
            .get(kid)
            .and_then(|b| b.pr.as_ref())
            .map(|pr| format!(" (#{})", pr.number))
            .unwrap_or_default();
        println!("{indent}{kid}{pr}");
        print_stack(kid, children, branches, depth + 1);
    }
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

/// `stacc status` — the current branch's position in the stack and its PR state.
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
