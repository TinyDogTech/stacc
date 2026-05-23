//! Implementations of the CLI subcommands.

use std::path::Path;

use serde_json::json;
use stacc_config::{detect, read_file, resolve, Overrides};
use stacc_git::Git;
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
