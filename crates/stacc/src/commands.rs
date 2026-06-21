//! Implementations of the CLI subcommands.

use std::io::IsTerminal;
use std::path::Path;

use serde_json::{json, Value};
use stacc_config::{detect, read_file, resolve, Overrides};
use stacc_core::ops;
use stacc_git::Git;
use stacc_forge::SCHEMA_VERSION;
use stacc_github::{GitHub, NewPullRequest, PrState, PullRequestUpdate};
use stacc_state::{Base, BranchState, PullRequest, RepoConfig, StateStore};

use crate::cli::{CreateArgs, InitArgs, OutputFormat, RenameArgs, SubmitArgs, TrackArgs, UntrackArgs};
use crate::error::Error;

mod absorb;
mod agent;
mod auth;
mod completion;
mod config;
mod info;
mod log;
mod navigation;
mod operations;
mod removal;
mod reorder;
mod split;

pub use absorb::absorb;
pub use agent::agent;
pub use auth::auth;
pub use completion::completion;
pub use config::config;
pub use info::info;
pub use log::log;
pub use navigation::{bottom, checkout, children, down, parent, top, up};
pub use operations::{
    abort_cmd, continue_cmd, fold, merge, merged, modify, move_cmd, restack, squash, sync, undo,
};
pub use removal::{delete, pop};
pub use reorder::reorder;
pub use split::split;

/// `stacc init`: detect trunk/remote, then record them in the state ref.
pub fn init(args: &InitArgs, format: OutputFormat, no_interactive: bool, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let store = StateStore::new(git.clone());

    if let Some(repo) = store.load()?.repo {
        report(format, "already_initialized", &repo);
        return Ok(());
    }

    let detected = detect(&git)?;
    let file = read_file(&work_dir.join(".stacc.toml"))?;
    let flags = Overrides {
        trunk: args.trunk.clone(),
        remote: args.remote.clone(),
        local: None,
    };
    let config = resolve(detected, file, flags)?;
    let repo = RepoConfig {
        trunk: config.trunk,
        remote: config.remote,
        declined_tracking: Default::default(),
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

    if crate::interactive::allowed(std::io::stdout().is_terminal(), no_interactive, format) {
        if let Err(e) = agent::agent_install_interactive(format) {
            eprintln!("warning: agent context install skipped: {e}");
        }
    }

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
pub fn track(args: &TrackArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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
    track_branch_impl(&git, &store, &branch, &base)?;

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({ "status": "tracked", "branch": branch, "base": base })
        ),
        OutputFormat::Pretty => println!("Tracking {branch} (base: {base})"),
    }
    Ok(())
}

/// Core tracking logic shared between `stacc track` and the sync untracked flow.
/// Records `branch` with `base` in state and removes `branch` from
/// `declined_tracking` atomically in one `store.update` call.
pub(crate) fn track_branch_impl(
    git: &Git,
    store: &StateStore,
    branch: &str,
    base: &str,
) -> Result<(), Error> {
    let base_hash = git.rev_parse(base)?;
    store.update(|state| {
        state.branches.insert(
            branch.to_owned(),
            BranchState {
                base: Base {
                    name: base.to_owned(),
                    hash: base_hash.clone(),
                },
                pr: None,
                pr_title: None,
                pr_description: None,
            },
        );
        if let Some(repo) = &mut state.repo {
            repo.declined_tracking.remove(branch);
        }
        Ok(())
    })?;
    Ok(())
}

/// `stacc untrack`: drop a branch from the stack, reparenting its children onto
/// the branch's own base so the rest of the stack stays connected. Edits only
/// stacc state, never the git branch or the remote.
pub fn untrack(args: &UntrackArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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

/// `stacc create`: create a new branch stacked on the current one (or on
/// `--onto <branch>`), commit any staged changes (`--all` stages everything
/// first), and track it. `--insert` splices the new branch between the current
/// branch and its existing children, reparenting and restacking them. Refuses
/// only on a detached HEAD (unless `--onto` names the base explicitly).
// A cohesive validate -> branch -> track -> commit -> (insert-restack) -> report
// sequence; splitting it would only trade this lint for too_many_arguments.
#[allow(clippy::too_many_lines)]
pub fn create(args: &CreateArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let store = StateStore::new(git.clone());
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    if args.insert && args.onto.is_some() {
        return Err(Error::Usage(
            "--insert and --onto are mutually exclusive: --insert splices the new branch above the current one, --onto bases it elsewhere".into(),
        ));
    }

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

    let base = match &args.onto {
        Some(onto) => onto.clone(),
        None => git.current_branch().map_err(|_| {
            Error::Usage(
                "cannot create a branch from a detached HEAD; check out a branch first, or pass --onto".into(),
            )
        })?,
    };
    let base_hash = git.rev_parse(&base)?;

    // --insert: the base's existing children move onto the new branch. Capture
    // them and fail fast on a worktree conflict BEFORE mutating anything.
    let children = if args.insert {
        ops::children(&state.branches, &base)
    } else {
        Vec::new()
    };
    if !children.is_empty() {
        let subtrees: Vec<String> = ops::upstack_order(&state.branches, &base)
            .into_iter()
            .skip(1)
            .collect();
        operations::guard_worktree(&git, &subtrees)?;
    }

    if args.all {
        git.stage_all_respecting_ignores()?;
    }

    match &args.onto {
        Some(onto) => git.checkout_new_branch_at(&args.name, onto)?,
        None => git.checkout_new_branch(&args.name)?,
    }

    // Track the branch (and, under --insert, reparent the base's children onto
    // it; their base.hash is kept, it marks where each child's own commits
    // start) before committing, so a failing commit (e.g. a pre-commit hook)
    // can't strand it untracked; the staged changes survive for a retry.
    let apply_insert = {
        let name = args.name.clone();
        let children = children.clone();
        move |state: &mut stacc_state::State| {
            for child in &children {
                if let Some(b) = state.branches.get_mut(child) {
                    b.base.name.clone_from(&name);
                }
            }
        }
    };
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
                    pr_title: None,
                    pr_description: None,
                },
            );
            apply_insert(state);
            Ok(())
        })
        .map_err(|e| {
            Error::Usage(format!(
                "created branch `{}` but could not save state: {e}; run `stacc track` to recover",
                args.name
            ))
        })?;
    state.branches.insert(
        args.name.clone(),
        BranchState {
            base: Base {
                name: base.clone(),
                hash: base_hash.clone(),
            },
            pr: None,
            pr_title: None,
            pr_description: None,
        },
    );
    apply_insert(&mut state);

    let (committed, sha) = if git.has_staged_changes()? {
        let message = args.message.clone().unwrap_or_else(|| args.name.clone());
        git.commit(&message)?;
        (true, Some(git.rev_parse("HEAD")?))
    } else {
        (false, None)
    };

    // --insert: restack the reparented children's subtrees onto the new branch's
    // tip. A conflict resumes via the plain Restack continuation; the reparent
    // is already saved, so the re-applied delta only keeps a CAS retry honest.
    let mut restacked = Vec::new();
    if !children.is_empty() {
        let order: Vec<String> = ops::upstack_order(&state.branches, &args.name)
            .into_iter()
            .skip(1)
            .collect();
        restacked = operations::restack_with_recovery(
            &git,
            &store,
            &mut state,
            &repo,
            &order,
            |remaining| stacc_core::recovery::Operation::Restack { remaining },
            &apply_insert,
        )?;
        operations::clear_conflict_artifacts(&git);
        // Best-effort: the engine leaves HEAD on the last child it rebased.
        if let Err(err) = git.checkout(&args.name) {
            eprintln!("warning: could not switch back to `{}`: {err}", args.name);
        }
    }

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "status": "created",
                "branch": args.name,
                "base": base,
                "committed": committed,
                "sha": sha,
                "reparented": children,
                "restacked": restacked,
            })
        ),
        OutputFormat::Pretty => {
            let suffix = if committed {
                " (committed staged changes)"
            } else {
                ""
            };
            println!("Created {} (base: {base}){suffix}", args.name);
            if !children.is_empty() {
                println!("  reparented onto {}: {}", args.name, children.join(", "));
            }
            for name in &restacked {
                println!("Restacked {name}");
            }
        }
    }
    Ok(())
}

/// `stacc status`: the current branch's position in the stack and its PR state.
pub fn status(format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;

    let branch = git.current_branch()?;

    if branch == repo.trunk {
        match format {
            OutputFormat::Json => println!(
                "{}",
                json!({ "branch": branch, "trunk": true, "schema_version": SCHEMA_VERSION })
            ),
            OutputFormat::Pretty => println!("{branch} (trunk)"),
        }
        return Ok(());
    }

    let Some(branch_state) = state.branches.get(&branch) else {
        match format {
            OutputFormat::Json => println!(
                "{}",
                json!({ "branch": branch, "tracked": false, "schema_version": SCHEMA_VERSION })
            ),
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
            let change_json =
                pr.map(|(number, state)| json!({ "number": number, "state": pr_state_str(state) }));
            print_compact(json!({
                "branch": branch,
                "base": branch_state.base.name,
                "children": children,
                "change": change_json,
                "schema_version": SCHEMA_VERSION,
            }));
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

/// The actionable `mergeable_state` values worth surfacing: blocked on
/// reviews/checks, behind its base, or conflicting. `clean`/`unknown`/absent
/// show no hint.
pub(crate) fn mergeable_hint(state: Option<&str>) -> Option<&str> {
    state.filter(|state| matches!(*state, "blocked" | "behind" | "dirty"))
}

/// Neutral readiness value for the agent-facing JSON, flattened from GitHub's
/// `mergeable_state` string. Display-only and never gates. Only the states
/// stacc surfaces are named; everything else, including absence and GitHub's
/// not-yet-computed `unknown`, is `"unknown"`. Mirrors the forge adapter's
/// `readiness` mapping; the two collapse once the CLI holds neutral `Change`
/// values directly (the forge-wiring unit, STA-109).
pub(crate) fn readiness_str(mergeable_state: Option<&str>) -> &'static str {
    match mergeable_state {
        Some("clean") => "ready",
        Some("dirty") => "conflicted",
        Some("behind") => "behind",
        Some("blocked") => "blocked",
        _ => "unknown",
    }
}

/// Strip content an agent does not need from a JSON value before printing it:
/// object keys whose value is null (an absent key reads as none), an empty
/// `children` array (a leaf node), and `draft: false` (the non-draft default).
/// Recurses through objects and arrays. Key names stay descriptive; only
/// redundant content is dropped, so the output costs fewer tokens with no loss
/// of signal.
pub(crate) fn compact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, v| {
                let is_waste = v.is_null()
                    || (key == "children" && v.as_array().is_some_and(Vec::is_empty))
                    || (key == "draft" && v.as_bool() == Some(false));
                !is_waste
            });
            for v in map.values_mut() {
                compact(v);
            }
        }
        Value::Array(items) => {
            for v in items {
                compact(v);
            }
        }
        _ => {}
    }
}

/// Compact (see [`compact`]) a JSON value and print it as one line. The agent
/// emit path for commands that return structured data.
pub(crate) fn print_compact(mut value: Value) {
    compact(&mut value);
    println!("{value}");
}

/// `stacc pr`: print the current branch's recorded PR URL, and open it in a
/// browser when run on a terminal. Errors when the branch has no recorded PR.
pub fn pr(format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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
            println!(
                "{}",
                json!({ "branch": branch, "number": pr.number, "url": url, "schema_version": SCHEMA_VERSION })
            );
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
/// creating or updating each branch's PR with its parent as the base. `--stack`
/// widens the scope to the current branch's whole stack (its upstack too),
/// still downstack-first so every PR's base ref exists when its PR opens.
/// `--update-only` skips (and reports) branches with no recorded PR; `--draft`
/// opens new PRs as drafts.
// A cohesive validate -> push/PR loop -> persist -> report sequence; splitting it
// would only trade this lint for `too_many_arguments` on a helper.
#[allow(clippy::too_many_lines)]
pub fn submit(args: &SubmitArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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
    // --stack appends the current branch's upstack, still base-before-dependent
    // (`upstack_order` is bottom-up), so the ordering property holds stack-wide.
    let mut chain = ops::downstack_chain(&state, &current, &repo.trunk)?;
    if args.stack {
        chain.extend(
            ops::upstack_order(&state.branches, &current)
                .into_iter()
                .skip(1),
        );
    }

    let (github, owner, repo_name) = operations::require_github_forge(&git, &repo, "submit", work_dir)?;

    // (branch, action, number, url) for each branch we acted on.
    let mut results: Vec<(String, PrAction, u64, String)> = Vec::new();
    // Branches passed over by --update-only (no recorded PR to update).
    let mut skipped: Vec<String> = Vec::new();
    // The PR records to write back, applied together in one transactional update
    // after the network work so a concurrent change to another branch survives.
    let mut pr_updates: Vec<(String, PullRequest)> = Vec::new();
    // Caller-supplied title/description for the current branch, to persist in
    // state so re-submits reuse them without repeating the flags.
    let mut title_update: Option<(String, String)> = None;
    let mut desc_update: Option<(String, String)> = None;

    for branch in &chain {
        let is_current = branch == &current;
        let branch_state = state.branches.get(branch).expect("branch is in chain");
        let base = branch_state.base.name.clone();
        let stored_title = branch_state.pr_title.clone();
        let stored_desc = branch_state.pr_description.clone();

        let recorded = branch_state.pr.as_ref().map(|pr| pr.number);

        // --update-only: a branch with no PR is skipped entirely (not even
        // pushed), and reported, instead of getting a new PR.
        if args.update_only && recorded.is_none() {
            skipped.push(branch.clone());
            continue;
        }

        // Adoption: a branch with no recorded PR may still have one open on
        // GitHub (created by gh/graphite before the stack migrated to stacc).
        // Look it up by head before creating, so submit reconciles the existing
        // PR instead of hitting GitHub's 422 duplicate error. A failed lookup
        // falls through to create; a real problem (auth, network) surfaces there.
        let mut adopted = false;
        let existing = match recorded {
            Some(number) => Some(number),
            None => match github.pull_request_for_branch(&owner, &repo_name, branch) {
                Ok(Some(found)) => {
                    adopted = true;
                    Some(found.number)
                }
                Ok(None) | Err(_) => None,
            },
        };

        git.push_force_with_lease(&repo.remote, branch)?;

        // Title: --title (current branch only) > stored pr_title > commit subject.
        // Description: --description (current branch only) > stored pr_description > commit body.
        let title = if is_current {
            match &args.title {
                Some(t) => {
                    title_update = Some((branch.clone(), t.clone()));
                    t.clone()
                }
                None => stored_title.unwrap_or_else(|| git.commit_subject(branch).unwrap_or_default()),
            }
        } else {
            stored_title.unwrap_or_else(|| git.commit_subject(branch).unwrap_or_default())
        };
        let body = if is_current {
            match &args.description {
                Some(value) => {
                    let resolved = resolve_description(value)?;
                    desc_update = Some((branch.clone(), resolved.clone()));
                    resolved
                }
                None => stored_desc
                    .clone()
                    .unwrap_or_else(|| reflow_body(&git.commit_body(branch).unwrap_or_default())),
            }
        } else {
            stored_desc
                .clone()
                .unwrap_or_else(|| reflow_body(&git.commit_body(branch).unwrap_or_default()))
        };

        // Only include body in PATCH when there is an explicit source: --description
        // on this run, or a previously-stored description. When neither exists, omit
        // the field so GitHub preserves any manual PR body edits.
        let update_body: Option<String> = if is_current {
            desc_update.as_ref().map(|(_, d)| d.clone()).or(stored_desc)
        } else {
            stored_desc
        };

        let pr = match existing {
            Some(number) => github.update_pull_request(
                &owner,
                &repo_name,
                number,
                &PullRequestUpdate {
                    title: Some(title),
                    body: update_body,
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
                    draft: args.draft,
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

        let action = if adopted {
            PrAction::Adopted
        } else if recorded.is_none() {
            PrAction::Created
        } else {
            PrAction::Updated
        };
        results.push((branch.clone(), action, pr.number, pr.url));
    }

    store.update(|state| {
        for (branch, pr) in &pr_updates {
            if let Some(branch_state) = state.branches.get_mut(branch) {
                branch_state.pr = Some(pr.clone());
            }
        }
        if let Some((branch, title)) = &title_update {
            if let Some(branch_state) = state.branches.get_mut(branch) {
                branch_state.pr_title = Some(title.clone());
            }
        }
        if let Some((branch, desc)) = &desc_update {
            if let Some(branch_state) = state.branches.get_mut(branch) {
                branch_state.pr_description = Some(desc.clone());
            }
        }
        Ok(())
    })?;

    match format {
        OutputFormat::Json => {
            let list: Vec<serde_json::Value> = results
                .iter()
                .map(|(branch, action, number, url)| {
                    // An adopted PR took the update path; the extra flag says
                    // stacc discovered (rather than created) the record.
                    let mut entry = json!({
                        "status": if *action == PrAction::Created { "created" } else { "updated" },
                        "branch": branch,
                        "number": number,
                        "url": url,
                    });
                    if *action == PrAction::Adopted {
                        entry["adopted"] = json!(true);
                    }
                    entry
                })
                .collect();
            println!(
                "{}",
                json!({ "submitted": list, "skipped": skipped, "schema_version": SCHEMA_VERSION })
            );
        }
        OutputFormat::Pretty => {
            for (branch, action, number, url) in &results {
                let verb = match action {
                    PrAction::Created => "Created",
                    PrAction::Updated => "Updated",
                    PrAction::Adopted => "Adopted",
                };
                println!("{verb} PR #{number} for {branch}: {url}");
            }
            for branch in &skipped {
                println!("Skipped {branch} (no PR to update; --update-only)");
            }
        }
    }
    Ok(())
}

/// How `submit` handled one branch's PR: created fresh, updated a recorded one,
/// or adopted an open PR that existed on GitHub but not yet in stacc state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrAction {
    Created,
    Updated,
    Adopted,
}

/// Resolve a `--description` value: `@path` reads a file, anything else is literal.
fn resolve_description(value: &str) -> Result<String, Error> {
    match value.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| Error::Usage(format!("failed to read description file `{path}`: {e}"))),
        None => Ok(value.to_string()),
    }
}

/// Reflow a commit body for use as a PR description.
///
/// Commit bodies are conventionally hard-wrapped at ~72 columns. GitHub renders
/// each bare newline as `<br>`, breaking mid-sentence.
fn reflow_body(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_para = false;
    for line in text.lines() {
        let t = line.trim_end();
        if t.is_empty() {
            if in_para {
                out.push_str("\n\n");
                in_para = false;
            }
        } else {
            if in_para {
                out.push(' ');
            }
            out.push_str(t);
            in_para = true;
        }
    }
    out
}

#[cfg(test)]
mod reflow_tests {
    use super::reflow_body;

    #[test]
    fn empty_string() {
        assert_eq!(reflow_body(""), "");
    }

    #[test]
    fn single_line_is_noop() {
        assert_eq!(reflow_body("One complete sentence."), "One complete sentence.");
    }

    #[test]
    fn joins_wrapped_paragraph() {
        assert_eq!(
            reflow_body("First line\nsecond line\nthird line."),
            "First line second line third line."
        );
    }

    #[test]
    fn preserves_paragraph_breaks() {
        assert_eq!(
            reflow_body("Para one line one\npara one line two.\n\nPara two."),
            "Para one line one para one line two.\n\nPara two."
        );
    }

    #[test]
    fn trailing_newline_no_extra_separator() {
        assert_eq!(reflow_body("One line.\n"), "One line.");
    }

    #[test]
    fn multiple_blank_lines_collapse_to_one_separator() {
        assert_eq!(reflow_body("Para one.\n\n\n\nPara two."), "Para one.\n\nPara two.");
    }

    #[test]
    fn strips_trailing_whitespace_per_line() {
        assert_eq!(reflow_body("First line   \nsecond line."), "First line second line.");
    }
}

/// `stacc rename`: rename the current branch, updating local state, children,
/// and (when it has a recorded PR, so it is on the remote) the remote branch.
/// Renaming a branch with its own open PR closes that PR on GitHub, so it
/// requires `--force` and drops the recorded PR so the next `submit` recreates
/// it.
pub fn rename(args: &RenameArgs, format: OutputFormat, work_dir: &Path) -> Result<(), Error> {
    let git = Git::open(work_dir);
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn init_git_repo() -> TempDir {
        let tmp = TempDir::new().expect("temp dir");
        let p = tmp.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@test.com"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            let ok = Command::new("git").arg("-C").arg(p).args(&args).status().unwrap().success();
            assert!(ok, "git {args:?} failed");
        }
        tmp
    }

    fn seed_repo_config(store: &StateStore, trunk: &str, declined: impl IntoIterator<Item = String>) {
        let trunk = trunk.to_owned();
        let declined: BTreeSet<String> = declined.into_iter().collect();
        store.update(|state| {
            state.repo = Some(RepoConfig {
                trunk: trunk.clone(),
                remote: "origin".into(),
                declined_tracking: declined.clone(),
            });
            Ok(())
        }).expect("seed repo config");
    }

    #[test]
    fn track_branch_impl_clears_declined_tracking() {
        let tmp = init_git_repo();
        let git = Git::open(tmp.path());
        let store = StateStore::new(git.clone());

        // Create and checkout a feature branch.
        Command::new("git")
            .arg("-C").arg(tmp.path())
            .args(["checkout", "-q", "-b", "feature"])
            .status().unwrap();

        // Seed state: repo config with the feature branch in declined_tracking.
        seed_repo_config(&store, "main", ["feature".to_owned()]);

        // Verify it's in declined before tracking.
        let state = store.load().unwrap();
        assert!(
            state.repo.as_ref().unwrap().declined_tracking.contains("feature"),
            "precondition: feature must be in declined_tracking"
        );

        // Track the branch.
        track_branch_impl(&git, &store, "feature", "main").expect("track_branch_impl failed");

        // After tracking: branch is in state.branches, NOT in declined_tracking.
        let state = store.load().unwrap();
        assert!(
            state.branches.contains_key("feature"),
            "feature must be in branches after tracking"
        );
        assert!(
            !state.repo.as_ref().unwrap().declined_tracking.contains("feature"),
            "feature must be removed from declined_tracking after tracking"
        );
    }

    #[test]
    fn track_branch_impl_works_when_not_declined() {
        let tmp = init_git_repo();
        let git = Git::open(tmp.path());
        let store = StateStore::new(git.clone());

        Command::new("git")
            .arg("-C").arg(tmp.path())
            .args(["checkout", "-q", "-b", "feature"])
            .status().unwrap();

        seed_repo_config(&store, "main", []);

        // Tracking a branch that was never declined must not error.
        track_branch_impl(&git, &store, "feature", "main").expect("track_branch_impl must succeed");

        let state = store.load().unwrap();
        assert!(state.branches.contains_key("feature"));
        assert!(state.repo.as_ref().unwrap().declined_tracking.is_empty());
    }
}
