//! `stacc log`: render the tracked stack as a vertical, multi-column graph.
//!
//! The graph mirrors graphite's `gt log`: branches are nodes (`◉` current,
//! `○` others), the trunk sits at the bottom, `│` spines run down each column,
//! and a base with several children forks with `├`/`┘` connectors. The full
//! form annotates each branch that has its own commits with a dimmed metadata
//! block (age, `sha - subject`, live PR status, a needs-restack line); the
//! `short` form is the same graph with one row per branch; `long` is a thin
//! `git log --graph` pass-through.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use stacc_core::ops;
use stacc_git::Git;
use stacc_github::{GitHub, PrState};
use stacc_state::{BranchState, RepoConfig, StateStore};

use crate::cli::{ColorChoice, LogArgs, LogForm, OutputFormat};
use crate::error::Error;

/// Upper bound on the total time spent fetching live PR status, after which the
/// remaining branches fall back to their PR number with no status.
const STATUS_BUDGET: Duration = Duration::from_secs(5);
/// Column width assumed when `$COLUMNS` is unset, for subject truncation.
const FALLBACK_WIDTH: usize = 80;

const CURRENT_GLYPH: char = '◉';
const BRANCH_GLYPH: char = '○';

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
/// Foreground colors cycled across stacks, so each stack reads as a unit.
const PALETTE: &[&str] = &[
    "\x1b[36m", // cyan
    "\x1b[32m", // green
    "\x1b[35m", // magenta
    "\x1b[33m", // yellow
    "\x1b[34m", // blue
    "\x1b[31m", // red
];

/// Per-stack coloring for the pretty graph. When disabled (piped output or
/// `--color never`), every styling call is a no-op, so the renderers produce
/// the exact plain text the tests assert.
struct Palette {
    enabled: bool,
    /// branch name -> its stack's color code.
    colors: BTreeMap<String, &'static str>,
}

impl Palette {
    /// Assign each stack (grouped by its bottom branch) a cycling palette color;
    /// the trunk and untracked rows stay default.
    fn build(
        enabled: bool,
        branches: &BTreeMap<String, BranchState>,
        visible: &BTreeSet<String>,
        trunk: &str,
    ) -> Self {
        let mut bottoms: BTreeMap<String, &'static str> = BTreeMap::new();
        let mut colors = BTreeMap::new();
        if enabled {
            for name in visible {
                if name.as_str() == trunk || !branches.contains_key(name) {
                    continue;
                }
                let bottom = ops::bottom(branches, name, trunk);
                let next = PALETTE[bottoms.len() % PALETTE.len()];
                let color = *bottoms.entry(bottom).or_insert(next);
                colors.insert(name.clone(), color);
            }
        }
        Self { enabled, colors }
    }

    /// The color for a branch's glyph and name, if coloring is on.
    fn node(&self, name: &str) -> Option<&'static str> {
        self.enabled.then(|| self.colors.get(name).copied()).flatten()
    }

    /// Dim metadata text (leaving the graph lanes their normal weight).
    fn dim(&self, text: &str) -> String {
        if self.enabled && !text.is_empty() {
            format!("{DIM}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

/// Whether to emit ANSI styling for the given choice and output stream.
fn color_enabled(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => std::io::stdout().is_terminal(),
    }
}

/// `stacc log`: render the tracked stack from the state ref.
pub fn log(args: &LogArgs, format: OutputFormat, color: ColorChoice) -> Result<(), Error> {
    let git = Git::open(".");
    let store = StateStore::new(git.clone());
    let state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;
    let trunk = repo.trunk.clone();
    let branches = &state.branches;
    let current = git.current_branch().unwrap_or_default();

    // Trunk-reachable tracked branches (orphans are surfaced separately).
    let reachable = ops::topo_order(branches, &trunk);
    let reachable_set: BTreeSet<&str> = reachable.iter().map(String::as_str).collect();

    // The visible set after scope flags, always rooted at and including the trunk.
    let visible = visible_set(args, branches, &trunk, &current, &reachable_set);

    // long form: a pure git pass-through; JSON is a documented no-op.
    if args.form() == Some(LogForm::Long) {
        if format == OutputFormat::Json {
            println!("{}", json!({ "trunk": trunk, "form": "long" }));
            return Ok(());
        }
        let tips = tracked_tips(&visible, branches);
        let tip_refs: Vec<&str> = tips.iter().map(String::as_str).collect();
        let out = git.log_graph(&tip_refs, &trunk).unwrap_or_default();
        if !out.is_empty() {
            println!("{out}");
        }
        return Ok(());
    }

    let children = child_map(&visible, branches, &trunk);

    // Live PR status: fetched for JSON and the full pretty form (unless
    // --no-status); the short form is offline by contract.
    let want_status =
        !args.no_status && (format == OutputFormat::Json || args.form().is_none());
    let pr_status = if want_status {
        fetch_pr_status(&git, &repo, branches, &visible)
    } else {
        BTreeMap::new()
    };

    match format {
        OutputFormat::Json => {
            let stack = stack_json(&trunk, &children, branches, &git, &pr_status);
            println!("{}", json!({ "trunk": trunk, "stack": stack }));
        }
        OutputFormat::Pretty => {
            let palette = Palette::build(color_enabled(color), branches, &visible, &trunk);
            let ctx = RenderCtx {
                branches,
                trunk: &trunk,
                current: &current,
                git: &git,
                full: args.form().is_none(),
                pr_status: &pr_status,
                palette: &palette,
            };
            let lines = if args.reverse {
                render_reverse(&children, &ctx)
            } else {
                render_forward(&children, &ctx)
            };
            for line in lines {
                println!("{}", line.trim_end());
            }
            print_orphans(branches, &reachable_set, &current);
            if args.show_untracked {
                print_untracked(&git, branches, &trunk);
            }
        }
    }
    Ok(())
}

// --- Scope -----------------------------------------------------------------

/// The branches to render after the scope flags, always including the trunk so
/// the graph has a root. Without `--stack`/`--steps` this is every
/// trunk-reachable branch. Scoped, it is the current branch's full downstack
/// (to the trunk) plus its upstack limited to `--steps` levels.
fn visible_set(
    args: &LogArgs,
    branches: &BTreeMap<String, BranchState>,
    trunk: &str,
    current: &str,
    reachable: &BTreeSet<&str>,
) -> BTreeSet<String> {
    let mut visible = BTreeSet::new();
    visible.insert(trunk.to_string());

    // Unscoped, or scoped from a branch we can't anchor on: show everything.
    if !args.scoped() || current == trunk || !reachable.contains(current) {
        for name in reachable {
            visible.insert((*name).to_string());
        }
        return visible;
    }

    // Downstack: the full ancestor chain to the trunk (keeps the graph rooted).
    let mut node = current.to_string();
    loop {
        visible.insert(node.clone());
        match ops::parent(branches, &node) {
            Some(base) if base != trunk && reachable.contains(base.as_str()) => node = base,
            _ => break,
        }
    }

    // Upstack: descendants, limited to `--steps` levels (unbounded for --stack).
    let up_limit = args.steps.unwrap_or(usize::MAX);
    let mut queue = vec![(current.to_string(), 0usize)];
    while let Some((name, depth)) = queue.pop() {
        if depth >= up_limit {
            continue;
        }
        for kid in ops::children(branches, &name) {
            if visible.insert(kid.clone()) {
                queue.push((kid, depth + 1));
            }
        }
    }
    visible
}

/// The visible tracked branches (excludes the trunk), used as `git log` tips.
fn tracked_tips(visible: &BTreeSet<String>, branches: &BTreeMap<String, BranchState>) -> Vec<String> {
    visible
        .iter()
        .filter(|name| branches.contains_key(*name))
        .cloned()
        .collect()
}

/// base name -> its visible children, name-sorted. Only edges where both ends
/// are visible are kept, so the result is the visible subtree rooted at trunk.
fn child_map(
    visible: &BTreeSet<String>,
    branches: &BTreeMap<String, BranchState>,
    trunk: &str,
) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in visible {
        if name.as_str() == trunk {
            continue;
        }
        if let Some(bs) = branches.get(name) {
            if visible.contains(&bs.base.name) {
                map.entry(bs.base.name.clone()).or_default().push(name.clone());
            }
        }
    }
    for kids in map.values_mut() {
        kids.sort();
    }
    map
}

// --- Render ----------------------------------------------------------------

/// The shared inputs both render directions read, bundled so the renderers take
/// a graph plus one context rather than a long argument list.
struct RenderCtx<'a> {
    branches: &'a BTreeMap<String, BranchState>,
    trunk: &'a str,
    current: &'a str,
    git: &'a Git,
    full: bool,
    pr_status: &'a BTreeMap<String, Option<PrState>>,
    palette: &'a Palette,
}

// Forward render: trunk at the bottom.
fn render_forward(children: &BTreeMap<String, Vec<String>>, ctx: &RenderCtx) -> Vec<String> {
    let mut order = Vec::new();
    post_order(ctx.trunk, children, &mut order);

    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let last = order.len().saturating_sub(1);

    for (i, node) in order.iter().enumerate() {
        let is_trunk = node.as_str() == ctx.trunk;
        let seekers = seekers_of(&lanes, node);
        let node_col = column_for(&mut lanes, &seekers);

        // Multiple children of this base merge their columns back here.
        if seekers.len() > 1 {
            out.push(connector_row(&lanes, node_col, &seekers[1..], '┘'));
            for &lane in &seekers[1..] {
                lanes[lane] = None;
            }
        }

        let glyph = glyph_for(node, ctx.current);
        let lbl = label(node, ctx.current, ctx.full);
        out.push(node_row(&lanes, node_col, glyph, &lbl, ctx.palette.node(node)));

        // The lane now seeks this branch's base (trunk closes its lane).
        lanes[node_col] = if is_trunk {
            None
        } else {
            Some(ctx.branches.get(node).map_or_else(String::new, |b| b.base.name.clone()))
        };

        if ctx.full {
            for meta in meta_lines(ctx.git, node, is_trunk, ctx.branches, ctx.pr_status) {
                out.push(cont_row(&lanes, node_col, &ctx.palette.dim(&meta)));
            }
            if i != last {
                out.push(cont_row(&lanes, node_col, ""));
            }
        }
    }
    out
}

// Reverse render: trunk on top.
fn render_reverse(children: &BTreeMap<String, Vec<String>>, ctx: &RenderCtx) -> Vec<String> {
    let mut order = Vec::new();
    pre_order(ctx.trunk, children, &mut order);

    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let last = order.len().saturating_sub(1);

    for (i, node) in order.iter().enumerate() {
        let is_trunk = node.as_str() == ctx.trunk;
        let seekers = seekers_of(&lanes, node);
        let node_col = column_for(&mut lanes, &seekers);

        let glyph = glyph_for(node, ctx.current);
        let lbl = label(node, ctx.current, ctx.full);
        out.push(node_row(&lanes, node_col, glyph, &lbl, ctx.palette.node(node)));

        // This node's children continue below it: the first inherits this
        // column, the rest fork into fresh columns.
        let kids = children.get(node).cloned().unwrap_or_default();
        let mut extra = Vec::new();
        if kids.is_empty() {
            lanes[node_col] = None;
        } else {
            lanes[node_col] = Some(kids[0].clone());
            for kid in &kids[1..] {
                let lane = free_lane(&mut lanes);
                lanes[lane] = Some(kid.clone());
                extra.push(lane);
            }
        }

        if ctx.full {
            for meta in meta_lines(ctx.git, node, is_trunk, ctx.branches, ctx.pr_status) {
                out.push(cont_row(&lanes, node_col, &ctx.palette.dim(&meta)));
            }
        }
        if !extra.is_empty() {
            out.push(connector_row(&lanes, node_col, &extra, '┐'));
        }
        if ctx.full && i != last {
            out.push(cont_row(&lanes, node_col, ""));
        }
    }
    out
}

// --- Lane helpers ----------------------------------------------------------

fn post_order(node: &str, children: &BTreeMap<String, Vec<String>>, out: &mut Vec<String>) {
    if let Some(kids) = children.get(node) {
        for kid in kids {
            post_order(kid, children, out);
        }
    }
    out.push(node.to_string());
}

fn pre_order(node: &str, children: &BTreeMap<String, Vec<String>>, out: &mut Vec<String>) {
    out.push(node.to_string());
    if let Some(kids) = children.get(node) {
        for kid in kids {
            pre_order(kid, children, out);
        }
    }
}

/// Lane indices currently seeking `node` (its already-emitted children).
fn seekers_of(lanes: &[Option<String>], node: &str) -> Vec<usize> {
    lanes
        .iter()
        .enumerate()
        .filter_map(|(c, lane)| (lane.as_deref() == Some(node)).then_some(c))
        .collect()
}

/// The column a node occupies: the leftmost lane already seeking it, or a fresh
/// lane when it is a tip with no waiting children.
fn column_for(lanes: &mut Vec<Option<String>>, seekers: &[usize]) -> usize {
    match seekers.first() {
        Some(&col) => col,
        None => free_lane(lanes),
    }
}

/// The first free lane, reusing a closed slot before widening the graph.
fn free_lane(lanes: &mut Vec<Option<String>>) -> usize {
    if let Some(col) = lanes.iter().position(Option::is_none) {
        col
    } else {
        lanes.push(None);
        lanes.len() - 1
    }
}

fn glyph_for(node: &str, current: &str) -> char {
    if node == current {
        CURRENT_GLYPH
    } else {
        BRANCH_GLYPH
    }
}

fn label(node: &str, current: &str, full: bool) -> String {
    if full && node == current {
        format!("{node} (current)")
    } else {
        node.to_string()
    }
}

/// How many columns to draw on a row, covering every active lane and the node.
fn render_width(lanes: &[Option<String>], node_col: usize) -> usize {
    let max_active = lanes
        .iter()
        .enumerate()
        .filter_map(|(c, lane)| lane.is_some().then_some(c))
        .max()
        .unwrap_or(0);
    max_active.max(node_col) + 1
}

/// A branch row: the glyph at `node_col`, `│` for other active lanes, then the
/// label. The glyph and label carry the stack `color` when set; the lanes do
/// not, so columns stay legible.
fn node_row(
    lanes: &[Option<String>],
    node_col: usize,
    glyph: char,
    label: &str,
    color: Option<&str>,
) -> String {
    let width = render_width(lanes, node_col);
    let mut s = String::new();
    for c in 0..width {
        if c == node_col {
            match color {
                Some(code) => {
                    s.push_str(code);
                    s.push(glyph);
                    s.push_str(RESET);
                }
                None => s.push(glyph),
            }
        } else if lanes.get(c).is_some_and(Option::is_some) {
            s.push('│');
        } else {
            s.push(' ');
        }
        s.push(' ');
    }
    match color {
        Some(code) => {
            s.push_str(code);
            s.push_str(label);
            s.push_str(RESET);
        }
        None => s.push_str(label),
    }
    s
}

/// A continuation row (metadata or spacer): `│` for active lanes, then content.
/// Content aligns under the owning branch's label.
fn cont_row(lanes: &[Option<String>], node_col: usize, content: &str) -> String {
    let width = render_width(lanes, node_col);
    let mut s = String::new();
    for c in 0..width {
        let ch = if lanes.get(c).is_some_and(Option::is_some) {
            '│'
        } else {
            ' '
        };
        s.push(ch);
        s.push(' ');
    }
    s.push_str(content);
    s
}

/// A fork/join row: `├` at `node_col`, a horizontal run to the furthest
/// `branch_cols` lane, and `end` (`┘` joining down, `┐` forking down) at each.
fn connector_row(
    lanes: &[Option<String>],
    node_col: usize,
    branch_cols: &[usize],
    end: char,
) -> String {
    let max_col = branch_cols.iter().copied().max().unwrap_or(node_col).max(node_col);
    let mut row = vec![' '; 2 * max_col + 1];
    for c in 0..node_col {
        if lanes.get(c).is_some_and(Option::is_some) {
            row[2 * c] = '│';
        }
    }
    row[2 * node_col] = '├';
    for cell in row.iter_mut().take(2 * max_col + 1).skip(2 * node_col + 1) {
        *cell = '─';
    }
    for &col in branch_cols {
        row[2 * col] = end;
    }
    row.into_iter().collect()
}

// --- Metadata --------------------------------------------------------------

/// The dimmed metadata block under a branch in the full form. Branches with no
/// commits of their own (and the trunk, beyond its age) render as a bare name.
fn meta_lines(
    git: &Git,
    node: &str,
    is_trunk: bool,
    branches: &BTreeMap<String, BranchState>,
    pr_status: &BTreeMap<String, Option<PrState>>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if is_trunk {
        if let Ok(info) = git.commit_info(node) {
            if !info.age.is_empty() {
                lines.push(info.age);
            }
        }
        return lines;
    }

    let base = branches.get(node).map_or_else(String::new, |b| b.base.name.clone());
    if git.commits_ahead(&base, node).unwrap_or(0) > 0 {
        if let Ok(info) = git.commit_info(node) {
            if !info.age.is_empty() {
                lines.push(info.age);
            }
            lines.push(format!("{} - {}", info.sha, truncate_subject(&info.subject)));
        }
    }

    if let Some(pr) = branches.get(node).and_then(|b| b.pr.as_ref()) {
        let line = match pr_status.get(node).copied().flatten() {
            Some(state) => format!("#{} {}", pr.number, pr_status_label(state)),
            None => format!("#{}", pr.number),
        };
        lines.push(line);
    }

    if needs_restack(git, node, &base) {
        lines.push("needs restack".to_string());
    }
    lines
}

/// Whether `branch` has drifted off `base`. Any git failure reads as up to date
/// so `log` never raises a false alarm on a transient error.
fn needs_restack(git: &Git, branch: &str, base: &str) -> bool {
    !git.is_ancestor(base, branch).unwrap_or(true)
}

fn pr_status_label(state: PrState) -> &'static str {
    match state {
        PrState::Open => "Open",
        PrState::Closed => "Closed",
        PrState::Merged => "Merged",
    }
}

/// Clip a subject so the `sha - subject` line fits the terminal width.
fn truncate_subject(subject: &str) -> String {
    let budget = term_width().saturating_sub(12).max(20);
    if subject.chars().count() <= budget {
        return subject.to_string();
    }
    let kept: String = subject.chars().take(budget.saturating_sub(3)).collect();
    format!("{kept}...")
}

fn term_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|&w| w > 20)
        .unwrap_or(FALLBACK_WIDTH)
}

// --- Live PR status --------------------------------------------------------

/// Fetch each visible branch's live PR status, never fatally. Branches with a
/// recorded PR map to `Some(state)` on success and `None` on any failure (no
/// token, no remote, an API error, or the wall-clock budget exhausted).
fn fetch_pr_status(
    git: &Git,
    repo: &RepoConfig,
    branches: &BTreeMap<String, BranchState>,
    visible: &BTreeSet<String>,
) -> BTreeMap<String, Option<PrState>> {
    let mut map = BTreeMap::new();
    let prs: Vec<(String, u64)> = visible
        .iter()
        .filter_map(|name| {
            branches
                .get(name)
                .and_then(|b| b.pr.as_ref())
                .map(|pr| (name.clone(), pr.number))
        })
        .collect();
    if prs.is_empty() {
        return map;
    }

    let Some((github, owner, repo_name)) = build_client(git, repo) else {
        for (name, _) in prs {
            map.insert(name, None);
        }
        return map;
    };

    let start = Instant::now();
    for (name, number) in prs {
        let status = if start.elapsed() < STATUS_BUDGET {
            github
                .get_pull_request(&owner, &repo_name, number)
                .ok()
                .map(|pr| pr.state)
        } else {
            None
        };
        map.insert(name, status);
    }
    map
}

/// Best-effort GitHub client + (owner, repo) from the configured remote. `None`
/// when the remote is missing, not a GitHub URL, or no token is available.
fn build_client(git: &Git, repo: &RepoConfig) -> Option<(GitHub, String, String)> {
    let url = git.remote_url(&repo.remote).ok()?;
    let (owner, repo_name) = stacc_github::parse_remote(&url)?;
    let github = GitHub::from_env().ok()?;
    Some((github, owner, repo_name))
}

// --- JSON ------------------------------------------------------------------

/// The stack tree for `--format json`. `pr` is an object
/// `{number, url, status}` (status null when unavailable) and each branch with
/// its own commits carries a `commit {sha, subject, age}`.
fn stack_json(
    node: &str,
    children: &BTreeMap<String, Vec<String>>,
    branches: &BTreeMap<String, BranchState>,
    git: &Git,
    pr_status: &BTreeMap<String, Option<PrState>>,
) -> Vec<Value> {
    let Some(kids) = children.get(node) else {
        return Vec::new();
    };
    kids.iter()
        .map(|kid| {
            let pr = branches.get(kid).and_then(|b| b.pr.as_ref()).map(|p| {
                json!({
                    "number": p.number,
                    "url": p.url,
                    "status": pr_status.get(kid).copied().flatten().map(pr_state_str),
                })
            });
            json!({
                "name": kid,
                "base": node,
                "pr": pr,
                "commit": commit_json(git, kid, node),
                "children": stack_json(kid, children, branches, git, pr_status),
            })
        })
        .collect()
}

fn commit_json(git: &Git, branch: &str, base: &str) -> Option<Value> {
    if git.commits_ahead(base, branch).unwrap_or(0) == 0 {
        return None;
    }
    git.commit_info(branch).ok().map(|info| {
        json!({ "sha": info.sha, "subject": info.subject, "age": info.age })
    })
}

/// Lowercase PR state for the JSON contract (matches `stacc status`).
fn pr_state_str(state: PrState) -> &'static str {
    super::pr_state_str(state)
}

// --- Trailing sections -----------------------------------------------------

/// Tracked branches not reachable from the trunk (an orphaned base or a cycle),
/// surfaced so corrupt state is never silently hidden.
fn print_orphans(branches: &BTreeMap<String, BranchState>, reachable: &BTreeSet<&str>, current: &str) {
    let orphans: Vec<&String> = branches
        .keys()
        .filter(|name| !reachable.contains(name.as_str()))
        .collect();
    if orphans.is_empty() {
        return;
    }
    println!("unreachable:");
    for name in orphans {
        let glyph = glyph_for(name, current);
        let base = branches.get(name).map_or("", |b| b.base.name.as_str());
        println!("  {glyph} {name} (base: {base})");
    }
}

/// Local git branches stacc is not tracking, listed under `--show-untracked`.
fn print_untracked(git: &Git, branches: &BTreeMap<String, BranchState>, trunk: &str) {
    let untracked: Vec<String> = git
        .local_branches()
        .unwrap_or_default()
        .into_iter()
        .filter(|name| name.as_str() != trunk && !branches.contains_key(name))
        .collect();
    if untracked.is_empty() {
        return;
    }
    println!("untracked:");
    for name in untracked {
        println!("  ◌ {name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stacc_state::Base;

    /// Branch map from (name, base) pairs.
    fn stack(pairs: &[(&str, &str)]) -> BTreeMap<String, BranchState> {
        pairs
            .iter()
            .map(|(name, base)| {
                (
                    (*name).to_string(),
                    BranchState {
                        base: Base {
                            name: (*base).to_string(),
                            hash: "h".into(),
                        },
                        pr: None,
                    },
                )
            })
            .collect()
    }

    fn all_visible(branches: &BTreeMap<String, BranchState>, trunk: &str) -> BTreeSet<String> {
        let mut v: BTreeSet<String> = branches.keys().cloned().collect();
        v.insert(trunk.to_string());
        v
    }

    /// The short form's graph rows (full = false), which never touch git.
    fn graph(pairs: &[(&str, &str)], trunk: &str, current: &str, reverse: bool) -> Vec<String> {
        let branches = stack(pairs);
        let visible = all_visible(&branches, trunk);
        let children = child_map(&visible, &branches, trunk);
        let git = Git::open(".");
        let pr = BTreeMap::new();
        let palette = Palette::build(false, &branches, &visible, trunk);
        let ctx = RenderCtx {
            branches: &branches,
            trunk,
            current,
            git: &git,
            full: false,
            pr_status: &pr,
            palette: &palette,
        };
        if reverse {
            render_reverse(&children, &ctx)
        } else {
            render_forward(&children, &ctx)
        }
    }

    #[test]
    fn forward_linear_stack_has_trunk_at_the_bottom() {
        let lines = graph(&[("a", "main"), ("b", "a")], "main", "b", false);
        assert_eq!(lines, vec!["◉ b", "○ a", "○ main"]);
    }

    #[test]
    fn forward_forked_stack_joins_columns() {
        // main -> a and main -> b: two columns merging at the trunk.
        let lines = graph(&[("a", "main"), ("b", "main")], "main", "b", false);
        assert_eq!(lines, vec!["○ a", "│ ◉ b", "├─┘", "○ main"]);
    }

    #[test]
    fn reverse_puts_trunk_on_top() {
        let lines = graph(&[("a", "main"), ("b", "a")], "main", "b", true);
        assert_eq!(lines, vec!["○ main", "○ a", "◉ b"]);
    }

    #[test]
    fn reverse_forked_stack_forks_downward() {
        // Trunk on top forks into a (col 0) and b (col 1); b's lane descends to
        // the right of the leaf `a`.
        let lines = graph(&[("a", "main"), ("b", "main")], "main", "", true);
        assert_eq!(lines, vec!["○ main", "├─┐", "○ │ a", "  ○ b"]);
    }

    #[test]
    fn unscoped_visible_set_is_every_reachable_branch() {
        let branches = stack(&[("a", "main"), ("b", "a"), ("c", "b"), ("sib", "main")]);
        let reachable = ops::topo_order(&branches, "main");
        let reachable_set: BTreeSet<&str> = reachable.iter().map(String::as_str).collect();
        let args = log_args(false, None);
        let visible = visible_set(&args, &branches, "main", "b", &reachable_set);
        let names: BTreeSet<&str> = visible.iter().map(String::as_str).collect();
        assert_eq!(names, ["main", "a", "b", "c", "sib"].into_iter().collect());
    }

    #[test]
    fn stack_scope_keeps_only_the_current_branchs_line() {
        let branches = stack(&[("a", "main"), ("b", "a"), ("c", "b"), ("sib", "main")]);
        let reachable = ops::topo_order(&branches, "main");
        let reachable_set: BTreeSet<&str> = reachable.iter().map(String::as_str).collect();
        let args = log_args(true, None);
        let visible = visible_set(&args, &branches, "main", "b", &reachable_set);
        let names: BTreeSet<&str> = visible.iter().map(String::as_str).collect();
        // The sibling stack `sib` is excluded; the downstack to trunk is kept.
        assert_eq!(names, ["main", "a", "b", "c"].into_iter().collect());
    }

    #[test]
    fn steps_limits_the_upstack_depth() {
        let branches = stack(&[("a", "main"), ("b", "a"), ("c", "b")]);
        let reachable = ops::topo_order(&branches, "main");
        let reachable_set: BTreeSet<&str> = reachable.iter().map(String::as_str).collect();
        // From `a`, one level up reaches `b` but not `c`; the downstack to trunk
        // is always kept.
        let args = log_args(false, Some(1));
        let visible = visible_set(&args, &branches, "main", "a", &reachable_set);
        let names: BTreeSet<&str> = visible.iter().map(String::as_str).collect();
        assert_eq!(names, ["main", "a", "b"].into_iter().collect());
    }

    #[test]
    fn truncate_subject_clips_long_lines_with_an_ellipsis() {
        let long = "x".repeat(500);
        let out = truncate_subject(&long);
        assert!(out.ends_with("..."), "got: {out}");
        assert!(out.chars().count() < long.chars().count());
        // A short subject is returned untouched.
        assert_eq!(truncate_subject("feat: small"), "feat: small");
    }

    #[test]
    fn palette_colors_each_stack_distinctly() {
        let branches = stack(&[("a", "main"), ("b", "a"), ("c", "main")]);
        let visible = all_visible(&branches, "main");
        let on = Palette::build(true, &branches, &visible, "main");
        // Same stack (b is upstack of a) shares a color; a different stack differs.
        assert_eq!(on.node("a"), on.node("b"), "same stack shares a color");
        assert_ne!(on.node("a"), on.node("c"), "different stacks differ");
        assert!(on.node("a").is_some());

        // A disabled palette never colors or dims.
        let off = Palette::build(false, &branches, &visible, "main");
        assert!(off.node("a").is_none());
        assert_eq!(off.dim("x"), "x");
        let dimmed = on.dim("x");
        assert!(dimmed.contains('x') && dimmed != "x", "dim wraps: {dimmed:?}");
    }

    #[test]
    fn enabled_palette_wraps_glyph_and_name_in_ansi() {
        let branches = stack(&[("a", "main")]);
        let visible = all_visible(&branches, "main");
        let children = child_map(&visible, &branches, "main");
        let git = Git::open(".");
        let pr = BTreeMap::new();
        let on = Palette::build(true, &branches, &visible, "main");
        let ctx = RenderCtx {
            branches: &branches,
            trunk: "main",
            current: "a",
            git: &git,
            full: false,
            pr_status: &pr,
            palette: &on,
        };
        let joined = render_forward(&children, &ctx).join("\n");
        assert!(joined.contains("\x1b["), "expected ANSI codes: {joined:?}");
        assert!(joined.contains(RESET), "expected a reset: {joined:?}");
    }

    fn log_args(stack: bool, steps: Option<usize>) -> LogArgs {
        LogArgs {
            form: None,
            stack,
            steps,
            reverse: false,
            show_untracked: false,
            no_status: false,
            short: false,
        }
    }
}
