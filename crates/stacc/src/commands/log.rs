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
use stacc_forge::SCHEMA_VERSION;
use stacc_git::Git;
use stacc_github::{CheckRollup, GitHub, PrChecks, PrState, ReviewDecision};
use stacc_state::{BranchState, PullRequest, RepoConfig, StateStore};

use crate::cli::{ColorChoice, LogArgs, LogForm, OutputFormat};
use crate::error::Error;

/// Upper bound on the total time spent fetching live PR status, after which the
/// remaining branches fall back to their PR number with no status.
const STATUS_BUDGET: Duration = Duration::from_secs(5);
/// Leftover budget below which another status call is not worth starting.
const MIN_CALL_BUDGET: Duration = Duration::from_millis(50);
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
    let mut state = store.load()?;
    let repo = state
        .repo
        .clone()
        .ok_or_else(|| Error::Usage("stacc is not initialized; run `stacc init` first".into()))?;
    let trunk = repo.trunk.clone();
    let current = git.current_branch().unwrap_or_default();

    // Trunk-reachable tracked branches (orphans are surfaced separately).
    let reachable = ops::topo_order(&state.branches, &trunk);
    let reachable_set: BTreeSet<&str> = reachable.iter().map(String::as_str).collect();

    // The visible set after scope flags, always rooted at and including the trunk.
    let visible = visible_set(args, &state.branches, &trunk, &current, &reachable_set);

    // long form: a pure git pass-through; JSON is a documented no-op.
    if args.form() == Some(LogForm::Long) {
        if format == OutputFormat::Json {
            println!(
                "{}",
                json!({ "trunk": trunk, "form": "long", "schema_version": SCHEMA_VERSION })
            );
            return Ok(());
        }
        let tips = tracked_tips(&visible, &state.branches);
        let tip_refs: Vec<&str> = tips.iter().map(String::as_str).collect();
        let out = git.log_graph(&tip_refs, &trunk).unwrap_or_default();
        if !out.is_empty() {
            println!("{out}");
        }
        return Ok(());
    }

    let children = child_map(&visible, &state.branches, &trunk);

    // Tracked branches whose git ref is gone (deleted or merged-and-pruned). We
    // mark them and skip their metadata + PR fetch, since git can't resolve them.
    let deleted = missing_branches(&git, &visible, &state.branches, &trunk);

    // Live PR status: fetched for JSON and the full pretty form (unless
    // --no-status); the short form is offline by contract.
    let want_status =
        !args.no_status && (format == OutputFormat::Json || args.form().is_none());
    let (pr_status, adopted) = if want_status {
        fetch_pr_status(&git, &repo, &state.branches, &visible, &deleted)
    } else {
        (BTreeMap::new(), Vec::new())
    };

    // Self-heal: PRs discovered by head branch are recorded so the next log can
    // fetch them by number directly. One transactional write covers them all;
    // a failed write only loses the cache, this run still renders the statuses.
    // No lookups succeed offline/without a token, so nothing is written then.
    if !adopted.is_empty() {
        let _ = store.update(|s| {
            for (name, pr) in &adopted {
                if let Some(branch) = s.branches.get_mut(name) {
                    if branch.pr.is_none() {
                        branch.pr = Some(pr.clone());
                    }
                }
            }
            Ok(())
        });
        for (name, pr) in adopted {
            if let Some(branch) = state.branches.get_mut(&name) {
                branch.pr = Some(pr);
            }
        }
    }
    let branches = &state.branches;

    match format {
        OutputFormat::Json => {
            let stack = stack_json(&trunk, &children, branches, &git, &pr_status, &deleted);
            println!(
                "{}",
                json!({ "trunk": trunk, "stack": stack, "schema_version": SCHEMA_VERSION })
            );
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
                deleted: &deleted,
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

/// Visible tracked branches whose local git ref no longer resolves. `ref_missing`
/// treats a git error (vs a clean "not found") as present, so a transient
/// failure never mislabels a live branch as deleted.
fn missing_branches(
    git: &Git,
    visible: &BTreeSet<String>,
    branches: &BTreeMap<String, BranchState>,
    trunk: &str,
) -> BTreeSet<String> {
    visible
        .iter()
        .filter(|name| name.as_str() != trunk && branches.contains_key(*name))
        .filter(|name| git.ref_missing(name))
        .cloned()
        .collect()
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
    pr_status: &'a PrStatusMap,
    palette: &'a Palette,
    deleted: &'a BTreeSet<String>,
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

        let is_deleted = ctx.deleted.contains(node);
        let glyph = glyph_for(node, ctx.current);
        let lbl = label(node, ctx.current, ctx.full, is_deleted);
        out.push(node_row(&lanes, node_col, glyph, &lbl, ctx.palette.node(node)));

        // The lane now seeks this branch's base (trunk closes its lane).
        lanes[node_col] = if is_trunk {
            None
        } else {
            Some(ctx.branches.get(node).map_or_else(String::new, |b| b.base.name.clone()))
        };

        if ctx.full {
            for meta in meta_lines(ctx.git, node, is_trunk, is_deleted, ctx.branches, ctx.pr_status) {
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

        let is_deleted = ctx.deleted.contains(node);
        let glyph = glyph_for(node, ctx.current);
        let lbl = label(node, ctx.current, ctx.full, is_deleted);
        out.push(node_row(&lanes, node_col, glyph, &lbl, ctx.palette.node(node)));

        // This node's children continue below it: the first inherits this
        // column, the rest fork into fresh columns to the RIGHT (so the fork
        // connector always points rightward, never back across a lower lane).
        let kids = children.get(node).cloned().unwrap_or_default();
        let mut extra = Vec::new();
        if kids.is_empty() {
            lanes[node_col] = None;
        } else {
            lanes[node_col] = Some(kids[0].clone());
            for kid in &kids[1..] {
                let lane = free_lane_after(&mut lanes, node_col);
                lanes[lane] = Some(kid.clone());
                extra.push(lane);
            }
        }

        // The fork connector is drawn before the metadata so the metadata rows
        // sit under legitimately-open lanes, not phantom ones.
        if !extra.is_empty() {
            out.push(connector_row(&lanes, node_col, &extra, '┐'));
        }
        if ctx.full {
            for meta in meta_lines(ctx.git, node, is_trunk, is_deleted, ctx.branches, ctx.pr_status) {
                out.push(cont_row(&lanes, node_col, &ctx.palette.dim(&meta)));
            }
            if i != last {
                out.push(cont_row(&lanes, node_col, ""));
            }
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

/// The first free lane strictly to the right of `min`, widening if needed. Used
/// when forking children downward so a new branch never claims a column to the
/// left of its parent.
fn free_lane_after(lanes: &mut Vec<Option<String>>, min: usize) -> usize {
    if let Some(col) = lanes.iter().skip(min + 1).position(Option::is_none) {
        min + 1 + col
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

fn label(node: &str, current: &str, full: bool, deleted: bool) -> String {
    if deleted {
        format!("{node} (deleted)")
    } else if full && node == current {
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
    let span_end = branch_cols.iter().copied().max().unwrap_or(node_col).max(node_col);
    // The row must also cover any active lane beyond the connector span, so an
    // unrelated column to the right keeps its spine.
    let max_active = (0..lanes.len())
        .filter(|&c| lanes[c].is_some())
        .max()
        .unwrap_or(node_col);
    let width = span_end.max(max_active) + 1;
    let mut row = vec![' '; 2 * width - 1];

    // The horizontal run from the fork point to the furthest branching column.
    row[2 * node_col] = '├';
    for cell in row.iter_mut().take(2 * span_end).skip(2 * node_col + 1) {
        *cell = '─';
    }
    for &col in branch_cols {
        row[2 * col] = end;
    }
    // Restore the spine of every active lane that is not a branching column:
    // those left of / right of the span pass straight through (`│`), those the
    // horizontal run crosses are drawn as a crossing (`┼`).
    for c in 0..width {
        if c == node_col || branch_cols.contains(&c) || !lanes.get(c).is_some_and(Option::is_some) {
            continue;
        }
        row[2 * c] = if c > node_col && c < span_end { '┼' } else { '│' };
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
    is_deleted: bool,
    branches: &BTreeMap<String, BranchState>,
    pr_status: &PrStatusMap,
) -> Vec<String> {
    let mut lines = Vec::new();
    // A branch whose git ref is gone has no resolvable commits; show the bare
    // `(deleted)` name with no metadata block.
    if is_deleted {
        return lines;
    }
    if is_trunk {
        if let Ok(info) = git.commit_info(node) {
            if !info.age.is_empty() {
                lines.push(info.age);
            }
        }
        return lines;
    }

    // A branch with no commits of its own (an empty stacked branch) renders as a
    // bare name, like graphite's untouched branches.
    let base = branches.get(node).map_or_else(String::new, |b| b.base.name.clone());
    let (ahead, behind) = git.ahead_behind(&base, node).unwrap_or((0, 0));
    if ahead == 0 {
        return lines;
    }

    if let Ok(info) = git.commit_info(node) {
        if !info.age.is_empty() {
            lines.push(info.age);
        }
        lines.push(format!("{} - {}", info.sha, truncate_subject(&info.subject)));
    }

    if let Some(pr) = branches.get(node).and_then(|b| b.pr.as_ref()) {
        match pr_status.get(node).and_then(Option::as_ref) {
            Some(live) => {
                lines.push(pr_line(live));
                if let Some(rollup) = rollup_line(live.checks) {
                    lines.push(rollup);
                }
            }
            None => lines.push(format!("#{}", pr.number)),
        }
    }

    // `behind > 0`: the base has commits this branch lacks, i.e. it drifted.
    if behind > 0 {
        lines.push("needs restack".to_string());
    }
    lines
}

fn pr_status_label(state: PrState) -> &'static str {
    match state {
        PrState::Open => "Open",
        PrState::Closed => "Closed",
        PrState::Merged => "Merged",
    }
}

/// The PR metadata line: `#NN <state>` plus a mergeable-state hint when GitHub
/// reports the PR stuck, then the truncated title. An open draft renders as
/// `Draft` (GitHub's draft flag is a sub-state of open).
fn pr_line(detail: &PrLive) -> String {
    let state = if detail.pr.draft && detail.pr.state == PrState::Open {
        "Draft"
    } else {
        pr_status_label(detail.pr.state)
    };
    let mut line = format!("#{} {state}", detail.pr.number);
    if detail.pr.state == PrState::Open {
        if let Some(hint) = super::mergeable_hint(detail.pr.mergeable_state.as_deref()) {
            line.push_str(" (");
            line.push_str(hint);
            line.push(')');
        }
    }
    if !detail.pr.title.is_empty() {
        let reserved = line.chars().count() + 3; // the prefix built so far + " - "
        line.push_str(" - ");
        line.push_str(&truncate_text(&detail.pr.title, reserved));
    }
    line
}

/// The review/CI rollup line (e.g. `approved, CI pass`), or `None` when the
/// batched fetch had nothing for this PR.
fn rollup_line(checks: PrChecks) -> Option<String> {
    let parts: Vec<&str> = [checks.review.map(review_label), checks.checks.map(check_label)]
        .into_iter()
        .flatten()
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn review_label(review: ReviewDecision) -> &'static str {
    match review {
        ReviewDecision::Approved => "approved",
        ReviewDecision::ChangesRequested => "changes requested",
        ReviewDecision::ReviewRequired => "review required",
    }
}

fn check_label(checks: CheckRollup) -> &'static str {
    match checks {
        CheckRollup::Pass => "CI pass",
        CheckRollup::Fail => "CI fail",
        CheckRollup::Pending => "CI pending",
    }
}

/// Neutral JSON spelling of the review state, `no_review` when GitHub reports
/// none; mirrors the forge `ReviewState` values.
fn review_state_str(review: Option<ReviewDecision>) -> &'static str {
    match review {
        Some(ReviewDecision::Approved) => "approved",
        Some(ReviewDecision::ChangesRequested) => "changes_requested",
        Some(ReviewDecision::ReviewRequired) => "review_required",
        None => "no_review",
    }
}

/// Neutral JSON spelling of the checks state, `no_checks` when GitHub reports
/// none; mirrors the forge `ChecksState` values.
fn checks_state_str(checks: Option<CheckRollup>) -> &'static str {
    match checks {
        Some(CheckRollup::Pass) => "passed",
        Some(CheckRollup::Fail) => "failed",
        Some(CheckRollup::Pending) => "pending",
        None => "no_checks",
    }
}

/// Sanitize and clip a commit subject for display: strip control bytes (a
/// hostile or garbled subject must not inject ANSI escapes into the colored
/// output) and truncate to fit the terminal width after the `sha - ` prefix.
fn truncate_subject(subject: &str) -> String {
    truncate_text(subject, 12)
}

/// [`truncate_subject`] with an explicit column reserve for the line's prefix.
fn truncate_text(text: &str, reserved: usize) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let budget = term_width().saturating_sub(reserved).max(20);
    if clean.chars().count() <= budget {
        return clean;
    }
    let kept: String = clean.chars().take(budget.saturating_sub(3)).collect();
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

/// A branch's live PR detail: the REST fetch (state, title, draft, mergeable
/// state) plus its slice of the batched review/CI rollup.
struct PrLive {
    pr: stacc_github::PullRequest,
    checks: PrChecks,
}

impl PrLive {
    /// Wrap a fetched PR with no rollup yet; the batched call fills it in.
    fn new(pr: stacc_github::PullRequest) -> Self {
        Self {
            pr,
            checks: PrChecks::default(),
        }
    }
}

/// branch name -> its live PR detail (`None` when unavailable).
type PrStatusMap = BTreeMap<String, Option<PrLive>>;
/// PRs discovered by head branch this run, for the caller to record in state.
type Adoptions = Vec<(String, PullRequest)>;

/// Fetch each visible branch's live PR detail, never fatally. Branches with a
/// recorded PR map to `Some(PrLive)` (the full REST response) on success and
/// `None` on any failure (no token, no remote, an API error, or the wall-clock
/// budget exhausted). A final batched GraphQL call fills in the review/CI
/// rollup for the open PRs with whatever budget is left.
///
/// Tracked branches with NO recorded PR (e.g. a stack migrated from another
/// tool) are looked up by head branch under the same budget and tolerance; a
/// hit yields a live status now plus an adoption record `(branch, pr)` the
/// caller persists, so the next log fetches by number directly.
fn fetch_pr_status(
    git: &Git,
    repo: &RepoConfig,
    branches: &BTreeMap<String, BranchState>,
    visible: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
) -> (PrStatusMap, Adoptions) {
    let mut map: PrStatusMap = BTreeMap::new();
    let mut adopted = Vec::new();
    let recorded: Vec<(String, u64)> = visible
        .iter()
        .filter(|name| !deleted.contains(*name))
        .filter_map(|name| {
            branches
                .get(name)
                .and_then(|b| b.pr.as_ref())
                .map(|pr| (name.clone(), pr.number))
        })
        .collect();
    let unrecorded: Vec<String> = visible
        .iter()
        .filter(|name| !deleted.contains(*name))
        .filter(|name| branches.get(*name).is_some_and(|b| b.pr.is_none()))
        .cloned()
        .collect();
    if recorded.is_empty() && unrecorded.is_empty() {
        return (map, adopted);
    }

    let Some((github, owner, repo_name)) = build_client(git, repo) else {
        map.extend(recorded.into_iter().map(|(name, _)| (name, None)));
        return (map, adopted);
    };

    // Bound the TOTAL fetch time, not just when we stop starting calls: each
    // call's timeout is whatever budget remains, so a single hung request can't
    // blow past STATUS_BUDGET. A branch we run out of time for falls back to None.
    let start = Instant::now();
    let budget_left = || STATUS_BUDGET.saturating_sub(start.elapsed());
    for (name, number) in recorded {
        let remaining = budget_left();
        let status = if remaining > MIN_CALL_BUDGET {
            github
                .get_pull_request_within(&owner, &repo_name, number, remaining)
                .ok()
                .map(PrLive::new)
        } else {
            None
        };
        map.insert(name, status);
    }
    // By-head adoption lookups, after the recorded fetches so they only spend
    // leftover budget. Any failure (or no match) leaves the branch status-less,
    // exactly as before; only a confirmed open PR produces an adoption. The
    // list endpoint omits `mergeable_state`, so an adopted PR shows no
    // blocked/behind/dirty hint until the next run fetches it by number.
    for name in unrecorded {
        let remaining = budget_left();
        if remaining <= MIN_CALL_BUDGET {
            break;
        }
        if let Ok(Some(pr)) =
            github.pull_request_for_branch_within(&owner, &repo_name, &name, remaining)
        {
            adopted.push((
                name.clone(),
                PullRequest {
                    number: pr.number,
                    url: Some(pr.url.clone()),
                },
            ));
            map.insert(name, Some(PrLive::new(pr)));
        }
    }

    // Review decision + CI rollup for every open PR found above, in ONE
    // batched GraphQL call spending only leftover budget. Closed/merged PRs
    // have no actionable rollup and are skipped; any failure (or an exhausted
    // budget) just leaves every rollup empty, silently.
    let open: BTreeSet<u64> = map
        .values()
        .flatten()
        .filter(|detail| detail.pr.state == PrState::Open)
        .map(|detail| detail.pr.number)
        .collect();
    let open: Vec<u64> = open.into_iter().collect();
    let remaining = budget_left();
    let rollups = if !open.is_empty() && remaining > MIN_CALL_BUDGET {
        github
            .pull_request_checks_within(&owner, &repo_name, &open, remaining)
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    for detail in map.values_mut().flatten() {
        detail.checks = rollups.get(&detail.pr.number).copied().unwrap_or_default();
    }
    (map, adopted)
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

/// The stack tree for `--format json`. `pr` is an object `{number, url,
/// status, title, draft, mergeable_state, review, checks}`. The fields past
/// `url` are live data: all null when the status fetch failed, and
/// `mergeable_state`/`review`/`checks` also null when GitHub reports nothing
/// (state not yet computed, no reviewers, no CI) or the PR is not open (the
/// rollup is only fetched for open PRs). Each branch with its own
/// commits carries a `commit {sha, subject, age}`. A branch whose git ref is
/// gone carries `"deleted": true` and no `commit`.
fn stack_json(
    node: &str,
    children: &BTreeMap<String, Vec<String>>,
    branches: &BTreeMap<String, BranchState>,
    git: &Git,
    pr_status: &PrStatusMap,
    deleted: &BTreeSet<String>,
) -> Vec<Value> {
    let Some(kids) = children.get(node) else {
        return Vec::new();
    };
    kids.iter()
        .map(|kid| {
            let is_deleted = deleted.contains(kid);
            let live = pr_status.get(kid).and_then(Option::as_ref);
            let change = branches.get(kid).and_then(|b| b.pr.as_ref()).map(|p| {
                json!({
                    "number": p.number,
                    "url": p.url,
                    "state": live.map(|l| super::pr_state_str(l.pr.state)),
                    "title": live.map(|l| l.pr.title.clone()),
                    "draft": live.map(|l| l.pr.draft),
                    "readiness": live.map(|l| super::readiness_str(l.pr.mergeable_state.as_deref())),
                    "review": live.map(|l| review_state_str(l.checks.review)),
                    "checks": live.map(|l| checks_state_str(l.checks.checks)),
                })
            });
            let commit = if is_deleted { None } else { commit_json(git, kid, node) };
            let mut value = json!({
                "name": kid,
                "base": node,
                "change": change,
                "commit": commit,
                "children": stack_json(kid, children, branches, git, pr_status, deleted),
            });
            if is_deleted {
                value["deleted"] = Value::Bool(true);
            }
            value
        })
        .collect()
}

fn commit_json(git: &Git, branch: &str, base: &str) -> Option<Value> {
    if git.ahead_behind(base, branch).unwrap_or((0, 0)).0 == 0 {
        return None;
    }
    git.commit_info(branch).ok().map(|info| {
        json!({ "sha": info.sha, "subject": info.subject, "age": info.age })
    })
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
        let deleted = BTreeSet::new();
        let palette = Palette::build(false, &branches, &visible, trunk);
        let ctx = RenderCtx {
            branches: &branches,
            trunk,
            current,
            git: &git,
            full: false,
            pr_status: &pr,
            palette: &palette,
            deleted: &deleted,
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
    fn reverse_nested_fork_preserves_the_crossing_sibling_lane() {
        // main -> {p, q}; p -> {x, y}. When p forks (├─┼─┐), q's open lane sits
        // between p's two children and must survive as a crossing (┼), not be
        // severed by the horizontal run. Regression for the reverse pass-through.
        let lines = graph(
            &[("p", "main"), ("q", "main"), ("x", "p"), ("y", "p")],
            "main",
            "",
            true,
        );
        assert_eq!(
            lines,
            vec![
                "○ main",
                "├─┐",
                "○ │ p",
                "├─┼─┐",
                "○ │ │ x",
                "  │ ○ y",
                "  ○ q",
            ]
        );
        // The crossing glyph is present and q's lane is never dropped.
        assert!(lines.iter().any(|l| l.contains('┼')), "crossing expected: {lines:?}");
    }

    #[test]
    fn pr_status_label_covers_all_states() {
        assert_eq!(pr_status_label(PrState::Open), "Open");
        assert_eq!(pr_status_label(PrState::Merged), "Merged");
        assert_eq!(pr_status_label(PrState::Closed), "Closed");
    }

    /// A PrLive with the given live fields and no rollup.
    fn pr_live(state: PrState, draft: bool, ms: Option<&str>, title: &str) -> PrLive {
        PrLive {
            pr: stacc_github::PullRequest {
                number: 7,
                url: "u".into(),
                state,
                title: title.into(),
                body: String::new(),
                draft,
                mergeable_state: ms.map(String::from),
            },
            checks: PrChecks::default(),
        }
    }

    #[test]
    fn pr_line_shows_draft_hint_and_title() {
        assert_eq!(pr_line(&pr_live(PrState::Open, false, None, "")), "#7 Open");
        assert_eq!(pr_line(&pr_live(PrState::Open, true, None, "")), "#7 Draft");
        assert_eq!(
            pr_line(&pr_live(PrState::Open, false, Some("blocked"), "Add foo")),
            "#7 Open (blocked) - Add foo"
        );
        // `clean`/`unknown` are not actionable: no hint.
        assert_eq!(pr_line(&pr_live(PrState::Open, false, Some("clean"), "")), "#7 Open");
        assert_eq!(pr_line(&pr_live(PrState::Open, false, Some("unknown"), "")), "#7 Open");
        // A merged PR never reads as draft and carries no stale hint.
        assert_eq!(
            pr_line(&pr_live(PrState::Merged, true, Some("dirty"), "t")),
            "#7 Merged - t"
        );
    }

    #[test]
    fn pr_line_clips_a_long_title_to_the_terminal_width() {
        let long = "y".repeat(500);
        let line = pr_line(&pr_live(PrState::Open, true, Some("blocked"), &long));
        assert!(line.ends_with("..."), "got: {line}");
        // The whole line (prefix + clipped title) fits the terminal; 44 covers
        // the widest prefix plus the truncation floor on absurdly narrow ones.
        assert!(
            line.chars().count() <= term_width().max(44),
            "line overflows the terminal: {} cols",
            line.chars().count()
        );
    }

    #[test]
    fn rollup_line_joins_review_and_ci() {
        let mk = |review, checks| PrChecks { review, checks };
        assert_eq!(rollup_line(mk(None, None)), None);
        assert_eq!(
            rollup_line(mk(Some(ReviewDecision::Approved), Some(CheckRollup::Pass))),
            Some("approved, CI pass".into())
        );
        assert_eq!(
            rollup_line(mk(Some(ReviewDecision::ChangesRequested), None)),
            Some("changes requested".into())
        );
        assert_eq!(
            rollup_line(mk(Some(ReviewDecision::ReviewRequired), Some(CheckRollup::Fail))),
            Some("review required, CI fail".into())
        );
        assert_eq!(
            rollup_line(mk(None, Some(CheckRollup::Pending))),
            Some("CI pending".into())
        );
    }

    #[test]
    fn truncate_subject_strips_control_bytes() {
        // A subject with an embedded ANSI escape and a bell must not leak them.
        let out = truncate_subject("feat: \x1b[31mred\x1b[0m\x07 done");
        assert!(!out.contains('\x1b') && !out.contains('\x07'), "got: {out:?}");
        assert!(out.contains("red") && out.contains("done"));
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
        let deleted = BTreeSet::new();
        let on = Palette::build(true, &branches, &visible, "main");
        let ctx = RenderCtx {
            branches: &branches,
            trunk: "main",
            current: "a",
            git: &git,
            full: false,
            pr_status: &pr,
            palette: &on,
            deleted: &deleted,
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
