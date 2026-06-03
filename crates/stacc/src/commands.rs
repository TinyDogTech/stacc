//! Implementations of the CLI subcommands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};
use stacc_config::{detect, read_file, resolve, Overrides};
use stacc_core::{ops, recovery};
use stacc_git::{Git, RebaseError};
use stacc_github::{GitHub, NewPullRequest, PrState, PullRequestUpdate};
use stacc_state::{Base, BranchState, PullRequest, RepoConfig, State, StateStore};

use crate::cli::{AuthAction, AuthArgs, InitArgs, OutputFormat, SubmitArgs, SyncArgs, TrackArgs};
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

/// `stacc submit` — push the current branch and its ancestors up to the trunk,
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

/// `stacc sync` — reconcile merged PRs and restack the stack.
///
/// Detects branches whose PR has merged (re-parenting their children and
/// dropping them), pulls the trunk from upstream, then restacks the remaining
/// branches bottom-up onto their bases.
pub fn sync(args: &SyncArgs, format: OutputFormat) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    if args.continue_ {
        return sync_continue(&git, &store, &mut state, &repo, format);
    }

    // Branches that have a recorded PR.
    let with_prs: Vec<(String, u64)> = state
        .branches
        .iter()
        .filter_map(|(name, b)| b.pr.as_ref().map(|pr| (name.clone(), pr.number)))
        .collect();

    // Ask GitHub which of those PRs have merged.
    let mut merged: BTreeSet<String> = BTreeSet::new();
    if !with_prs.is_empty() {
        let (owner, repo_name) = stacc_github::parse_remote(&git.remote_url(&repo.remote)?)
            .ok_or_else(|| Error::Usage(format!("remote `{}` is not a GitHub URL", repo.remote)))?;
        let github = GitHub::from_env()?;
        for (name, number) in &with_prs {
            if github.get_pull_request(&owner, &repo_name, *number)?.state == PrState::Merged {
                merged.insert(name.clone());
            }
        }
    }

    // Re-parent children of merged branches onto the nearest surviving base.
    let mut reparented: Vec<(String, String)> = Vec::new();
    for (name, branch) in &state.branches {
        if merged.contains(name) {
            continue;
        }
        let new_base = ops::resolve_base(&state.branches, &merged, branch.base.name.clone());
        if new_base != branch.base.name {
            reparented.push((name.clone(), new_base));
        }
    }
    for (name, new_base) in &reparented {
        if let Some(branch) = state.branches.get_mut(name) {
            branch.base.name.clone_from(new_base);
        }
    }
    for name in &merged {
        state.branches.remove(name);
    }

    // Pull the trunk from upstream. Strict by default — a flaky network or a
    // bad remote should surface immediately. `--offline` opts out and restacks
    // on whatever refs are already local.
    if !args.offline {
        if let Err(err) = fast_forward_trunk(&git, &repo.remote, &repo.trunk) {
            eprintln!("hint: pass --offline to skip the fetch and restack on local refs only");
            return Err(err);
        }
    }

    // Pull-and-restack the remaining branches bottom-up onto their bases.
    let order = ops::topo_order(&state.branches, &repo.trunk);
    let restacked = restack_with_recovery(&git, &store, &mut state, &repo, &order, |remaining| {
        recovery::Operation::Sync { remaining }
    })?;

    store.save(&state)?;
    finish_sync(&git, &store, &repo);
    report_sync(format, &merged, &reparented, &restacked);
    Ok(())
}

/// Finish the in-progress rebase, then replay the remaining branches.
fn sync_continue(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    format: OutputFormat,
) -> Result<(), Error> {
    let remaining: Vec<String> = recovery::read_continuation(&git.git_dir()?)?
        .remaining()
        .to_vec();

    match git.rebase_continue() {
        Ok(()) => {}
        Err(RebaseError::Interrupt(_)) => {
            // Still conflicting on the same branch; the context file stands.
            let branch = remaining.first().cloned().unwrap_or_default();
            return Err(Error::Conflict { branch });
        }
        Err(RebaseError::Git(err)) => return Err(err.into()),
    }

    // The first entry's rebase just completed: record its new base hash.
    let mut restacked: Vec<String> = Vec::new();
    if let Some(first) = remaining.first() {
        if let Some(base_name) = state.branches.get(first).map(|b| b.base.name.clone()) {
            let base_tip = git.rev_parse(&base_name)?;
            if let Some(b) = state.branches.get_mut(first) {
                b.base.hash = base_tip;
            }
        }
        restacked.push(first.clone());
    }

    let rest: Vec<String> = remaining.into_iter().skip(1).collect();
    restacked.extend(restack_with_recovery(git, store, state, repo, &rest, |remaining| {
        recovery::Operation::Sync { remaining }
    })?);

    store.save(state)?;
    finish_sync(git, store, repo);
    report_sync(format, &BTreeSet::new(), &[], &restacked);
    Ok(())
}

/// Run the engine's [`ops::restack`], persisting recovery artifacts on a
/// conflict: the typed [`recovery::Operation`] continuation (built by `make_op`
/// from the unfinished queue) plus the GitHub-enriched conflict-context file.
/// The context writer stays in the CLI crate so `stacc-core` stays off
/// `stacc-github`.
fn restack_with_recovery(
    git: &Git,
    store: &StateStore,
    state: &mut State,
    repo: &RepoConfig,
    order: &[String],
    make_op: impl Fn(Vec<String>) -> recovery::Operation,
) -> Result<Vec<String>, Error> {
    match ops::restack(git, store, state, order) {
        Ok(restacked) => Ok(restacked),
        Err(ops::OpsError::Conflict { branch, remaining }) => {
            // `ops::restack` already saved state before returning.
            recovery::write_continuation(&git.git_dir()?, &make_op(remaining))?;
            write_conflict_context(git, state, repo, &branch);
            Err(Error::Conflict { branch })
        }
        Err(err) => Err(err.into()),
    }
}

/// Push the state ref (best-effort) and clear any conflict artifacts.
fn finish_sync(git: &Git, store: &StateStore, repo: &RepoConfig) {
    if let Err(err) = store.push(&repo.remote) {
        eprintln!("warning: could not push state to `{}`: {err}", repo.remote);
    }
    clear_conflict_artifacts(git);
}

fn report_sync(
    format: OutputFormat,
    merged: &BTreeSet<String>,
    reparented: &[(String, String)],
    restacked: &[String],
) {
    match format {
        OutputFormat::Json => {
            let merged_list: Vec<&String> = merged.iter().collect();
            let reparented_list: Vec<Value> = reparented
                .iter()
                .map(|(branch, base)| json!({ "branch": branch, "base": base }))
                .collect();
            println!(
                "{}",
                json!({
                    "merged": merged_list,
                    "reparented": reparented_list,
                    "restacked": restacked,
                })
            );
        }
        OutputFormat::Pretty => {
            if merged.is_empty() && reparented.is_empty() && restacked.is_empty() {
                println!("Already up to date.");
            } else {
                for name in merged {
                    println!("Merged, untracked: {name}");
                }
                for (name, base) in reparented {
                    println!("Re-parented {name} -> {base}");
                }
                for name in restacked {
                    println!("Restacked {name}");
                }
            }
        }
    }
}

fn clear_conflict_artifacts(git: &Git) {
    if let Ok(dir) = git.git_dir() {
        recovery::clear_continuation(&dir);
        let _ = std::fs::remove_file(dir.join("stacc-conflict-context.json"));
    }
}

/// Best-effort: write the conflict context for an agent to read and resolve.
fn write_conflict_context(git: &Git, state: &State, repo: &RepoConfig, branch: &str) {
    let base = state
        .branches
        .get(branch)
        .map(|b| b.base.name.clone())
        .unwrap_or_default();
    let conflicted = git.conflicted_files().unwrap_or_default();
    let base_pr = fetch_base_pr(git, repo, state, &base).unwrap_or(Value::Null);
    let context = json!({
        "branch": branch,
        "base": base,
        "conflicted_files": conflicted,
        "base_pr": base_pr,
    });
    if let Ok(dir) = git.git_dir() {
        let _ = std::fs::write(
            dir.join("stacc-conflict-context.json"),
            serde_json::to_string_pretty(&context).unwrap_or_default(),
        );
    }
}

/// The base branch's PR (number/title/body), if it has one. `None` on any
/// failure — the context is best-effort.
fn fetch_base_pr(git: &Git, repo: &RepoConfig, state: &State, base: &str) -> Option<Value> {
    let number = state.branches.get(base)?.pr.as_ref()?.number;
    let (owner, name) = stacc_github::parse_remote(&git.remote_url(&repo.remote).ok()?)?;
    let pr = GitHub::from_env().ok()?.get_pull_request(&owner, &name, number).ok()?;
    Some(json!({ "number": pr.number, "title": pr.title, "body": pr.body }))
}

/// Fetch the trunk from `remote` and fast-forward the local trunk to it.
fn fast_forward_trunk(git: &Git, remote: &str, trunk: &str) -> Result<(), Error> {
    git.fetch(remote, trunk)?;
    let remote_tip = git.rev_parse(&format!("{remote}/{trunk}"))?;
    let local_tip = git.rev_parse(trunk)?;
    if local_tip != remote_tip && git.is_ancestor(&local_tip, &remote_tip)? {
        git.update_ref(
            &format!("refs/heads/{trunk}"),
            &remote_tip,
            Some(local_tip.as_str()),
        )?;
    }
    Ok(())
}

/// `stacc auth` — dispatch to login / logout / status.
pub fn auth(args: &AuthArgs, format: OutputFormat) -> Result<(), Error> {
    match args.action {
        AuthAction::Login => auth_login(format),
        AuthAction::Logout => auth_logout(format),
        AuthAction::Status => auth_status(format),
    }
}

fn auth_login(format: OutputFormat) -> Result<(), Error> {
    let flow = stacc_github::DeviceFlow::default();
    let code = flow.request_code()?;

    // Surface the user code before polling starts — the user has to type this
    // into GitHub for the poll to ever succeed.
    match format {
        OutputFormat::Pretty => {
            println!();
            println!("To authorize stacc, open: {}", code.verification_uri);
            println!("And enter the code: {}", code.user_code);
            println!();
            println!(
                "Waiting for authorization (expires in {} seconds)...",
                code.expires_in
            );
        }
        OutputFormat::Json => println!(
            "{}",
            json!({
                "status": "pending",
                "verification_uri": code.verification_uri,
                "user_code": code.user_code,
                "expires_in": code.expires_in,
            })
        ),
    }

    let token = flow.poll_token(&code, std::thread::sleep)?;
    stacc_github::store_token(&token)?;

    // Best-effort: confirm the token by looking up the user. A network error
    // here doesn't undo a successful login.
    let user = GitHub::new(token).current_user().ok();

    match format {
        OutputFormat::Pretty => match &user {
            Some(login) => println!("Authenticated as {login}"),
            None => println!("Token stored"),
        },
        OutputFormat::Json => println!(
            "{}",
            json!({
                "status": "authenticated",
                "user": user,
            })
        ),
    }
    Ok(())
}

fn auth_logout(format: OutputFormat) -> Result<(), Error> {
    stacc_github::clear_token()?;
    match format {
        OutputFormat::Pretty => println!("Cleared stored token"),
        OutputFormat::Json => println!("{}", json!({ "status": "logged_out" })),
    }
    Ok(())
}

// Returns `Result<(), Error>` to match the dispatcher's uniform signature even
// though no failure path is observable here.
#[allow(clippy::unnecessary_wraps)]
fn auth_status(format: OutputFormat) -> Result<(), Error> {
    let env_set =
        std::env::var("GITHUB_TOKEN").is_ok() || std::env::var("GH_TOKEN").is_ok();
    let keyring_set = stacc_github::load_token().is_some();
    let source = if env_set {
        "env"
    } else if keyring_set {
        "keyring"
    } else {
        "none"
    };

    // Only verify by hitting the API if we actually have a token.
    let user = if source == "none" {
        None
    } else {
        GitHub::from_env().ok().and_then(|gh| gh.current_user().ok())
    };

    match format {
        OutputFormat::Pretty => match source {
            "env" => {
                println!("Authenticated via GITHUB_TOKEN");
                if let Some(u) = &user {
                    println!("Logged in as {u}");
                }
                if keyring_set {
                    println!("(a stored token also exists; env var takes precedence)");
                }
            }
            "keyring" => {
                println!("Authenticated via stored token");
                if let Some(u) = &user {
                    println!("Logged in as {u}");
                }
            }
            _ => println!("Not authenticated. Run `stacc auth login` or set GITHUB_TOKEN."),
        },
        OutputFormat::Json => println!(
            "{}",
            json!({
                "source": source,
                "user": user,
                "env_set": env_set,
                "keyring_set": keyring_set,
            })
        ),
    }
    Ok(())
}
