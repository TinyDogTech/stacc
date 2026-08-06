# Concurrency under parallel streams: does the existing state-ref CAS hold?

Research for [STA-150](https://linear.app/tiny-dog-tech/issue/STA-150/concurrency-under-parallel-streams-does-the-existing-state-ref-cas),
a ticket on the [stacc streams](https://linear.app/tiny-dog-tech/issue/STA-145/stacc-streams-worktree-aware-stacking)
wayfinding map. Read at commit `4199376`.

There was no `docs/research/` directory before this file. The repo keeps
`docs/brainstorms/` and `docs/plans/`; research notes had no home, so this
starts one, matching the existing `YYYY-MM-DD-<slug>.md` naming.

## Headline

`plans/stacc.md:116` still lists "Parallel agent / worktree support, locking on
state ref for concurrent writes" under **Later (v2+)**. That line is stale. The
locking landed with the P1 parallel-agent foundation
(`docs/plans/2026-06-07-001-feat-parallel-agent-foundation-plan.md`), and the
compare-and-swap holds: **no production write path bypasses it**.

What does not hold under parallel streams is everything built *on top* of the
state ref: `undo`'s version chain, the trunk fast-forward, the remote state
push, and the reach of the bulk restack passes. Four of the five investigated
items are already-solved or already-ticketed; the design work streams actually
creates is in items 1 and 5, plus two newly found defects.

| # | Item | Verdict |
| --- | --- | --- |
| 1 | CAS on every write path | Already handled. One deliberate exception (`undo`) that streams turns into a design decision. |
| 2 | `Contention` in the `--json` envelope | Already handled in code, undocumented in the agent contract. Not the STA-143 shape. |
| 3 | STA-137 stale trunk worktree | Separate bug, must land before streams. Root cause pinpointed to one function. |
| 4 | STA-62 recovery isolation | Mechanism already correct and verified live. The tests it asks for still do not exist. |
| 5 | `WorktreeConflict` guard | Needs a design decision. Correct for focused ops; the bulk passes are the problem. Graduated as STA-161. |
| A | Remote state ref is permanently wedged | New bug. Reproduced in this repo, today. Filed as STA-160. |
| B | `undo` is global across streams | New design decision. Folded into STA-159. |
| C | Harness worktrees are git-locked | New constraint on the teardown design. Feeds STA-149 and STA-151. |
| D | Dropped-tip retention is shared | Note only. No action. |

## Method

Source reading against the working tree at `4199376`, plus two live probes run
from this repo's own linked worktree
(`.claude/worktrees/sta-156-tracker-doc`), which is exactly the topology
streams generalizes. Every claim below cites either a `file:line` in this repo
or the command whose output is quoted.

---

## 1. Is the CAS applied on every write path?

**Verdict: already handled.** There is no write that bypasses the
compare-and-swap.

`StateStore::update` (`crates/stacc-state/src/store.rs:133-165`) is the only
function that moves `refs/stacc/data`. It loads at the captured tip, runs the
caller's closure, commits onto that tip, and calls
`update_ref(ref, new, expected_old = tip)`. `Git::update_ref`
(`crates/stacc-git/src/lib.rs:799-805`) passes `old` as `git update-ref`'s
third positional argument, which is git's own compare-and-swap. A lost race is
distinguished from a real git failure by re-reading the ref
(`store.rs:150-152`), then retried with jittered exponential backoff
(`store.rs:336-342`) up to `SAVE_ATTEMPTS = 10` (`store.rs:20`), after which
`StateError::Contention` surfaces rather than clobbering the winner.

`StateStore::save` (`store.rs:175-180`) is the whole-state-replacement wrapper
and it also routes through `update`, so even it cannot tear a write. Its
closure discards the reloaded state, so it *is* a semantic clobber, but only
of state, never of the ref transaction.

Every call site was enumerated. Production `update` callers:

```
crates/stacc/src/commands.rs:71,148,204,330,835,1054,1083,1170
crates/stacc/src/commands/operations.rs:1154,1431,2701,2779,2855,3436
crates/stacc/src/commands/removal.rs:267
crates/stacc/src/commands/reorder.rs:128
crates/stacc/src/commands/split.rs:180,338
crates/stacc/src/commands/log.rs:165
```

Production `save` callers: exactly one, `crates/stacc/src/commands/operations.rs:2939`,
inside `undo`. Every other `.save(` hit in the repo is a test fixture.

Two things worth recording rather than fixing:

* **The restack path is transactional too.** `persist_restack`
  (`operations.rs:3432-3450`) folds the command's own delta and the engine's
  `(branch, base_hash)` updates into a single `update` closure, so a concurrent
  writer on a different branch is re-applied onto rather than overwritten. The
  plan's F1 sequence diagram
  (`docs/plans/2026-06-07-001-feat-parallel-agent-foundation-plan.md:303-323`)
  is an accurate description of what the code does.
* **`stacc log` silently drops its write on contention.** `log.rs:165` writes
  discovered PR numbers back as a cache with `let _ = store.update(...)`. This
  is deliberate and commented ("a failed write only loses the cache"), and it
  is correct: the render still succeeds. Under streams it will simply lose the
  cache more often. No action.

The temporary index used to build each state commit is
`git_dir()/stacc-index-<pid>` (`crates/stacc-git/src/lib.rs:651-653`). Both
components are per-writer under streams, so two streams committing state
concurrently cannot corrupt each other's index. No action.

### The exception, and why streams promotes it to a decision

`undo` reads a target version off the shared chain and replaces the whole state
with it (`operations.rs:2878-2939`). See **finding B** below. The CAS is not
the problem; the payload is.

---

## 2. What does a `Contention` failure look like to an agent?

**Verdict: already handled in code, missing from the documented agent
contract.** This is *not* the STA-143 shape.

`StateError::Contention` gets its own user-facing variant rather than being
folded into the transparent `State` wrapper, precisely so an agent can branch
on it (`crates/stacc/src/error.rs:20-25`, and the mapping at `error.rs:63-75`).
Its JSON is:

```json
{"type": "contention",
 "message": "state ref contention: gave up after 10 attempts; another agent is updating the same stack, retry",
 "schema_version": 3}
```

(`crates/stacc/src/error.rs:121-123`, asserted by
`contention_maps_to_its_own_discriminator` at `error.rs:169-175`.)

Contrast STA-143, where a push failure was reported as `readiness:"conflicted"`
with `retryable:false` and the real cause only on stderr. Here the
discriminator is honest, the message names the cause, and it tells the caller
to retry. Nothing is misreported.

The gap is documentation, not behavior:

* **There is no `retryable` field.** `retryable` exists only nested inside
  `merge`'s `stopped_at` object. An agent following the installed contract
  (`crates/stacc/assets/agent/skill-content.md:40,45,114,117`) looks for
  `stopped_at.retryable` and finds nothing, because a contention failure is a
  top-level error envelope, not a merge stop state.
* **Neither `type:"contention"` nor `type:"worktree_conflict"` appears anywhere
  in `skill-content.md`.** Both are error types an agent will meet routinely
  once streams exist, and neither is documented. Grepping that file for
  "contention" returns nothing.

**Recommendation.** Fold two rows into STA-138, which already edits
`skill-content.md`: document `type:"contention"` as retry-immediately, and
`type:"worktree_conflict"` as act-elsewhere. Cheaper than a new ticket, and
STA-153 already sequences STA-138 as fold-in work. Do **not** add a top-level
`retryable` field just for this; one retry semantics per envelope shape is
already confusing, and the type is self-describing.

---

## 3. STA-137: does the streams design have to fix it?

**Verdict: separate bug, and it must land before streams. The streams design
does not have to solve it, but it does raise it from High to blocking.**

The root cause is one function. `fast_forward_trunk`
(`crates/stacc/src/commands/operations.rs:3729-3740`):

```rust
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
```

It moves `refs/heads/<trunk>` with a leased plumbing write and never consults
`Git::branch_checked_out_elsewhere`. It is the **only** branch-ref move in the
codebase that skips that guard. Every sibling path calls it:

* `guard_worktree` for focused ops (`operations.rs:3331-3341`)
* `cleanup_merged_ref` before deleting a merged branch (`operations.rs:3366-3371`)
* `undo` before restoring a tip (`operations.rs:2915`)
* the restack engine before rebasing (`crates/stacc-core/src/ops.rs:290`)

So this is a gap in STA-65's guard coverage, exactly as STA-137's own report
guessed, and the fix is small: consult the guard, then choose between STA-137's
option 1 (`git -C <path> merge --ff-only`) and option 2 (skip the local ref
move and name the checkout that needs a pull). Option 2 cannot fail partway.

`fast_forward_trunk` has a single caller, `operations.rs:1543`, on the online
sync path.

Why it is blocking rather than merely urgent: under the sibling-directory
layout the main checkout holds trunk, and every stream is a linked worktree.
So *every* `stacc merge` or `stacc sync` run from a stream, with a trunk
advance, hits this. Not a rare interleaving, the default path.

**Cross-link to STA-151.** Under the bare-clone layout, no worktree holds
trunk, so this failure mode cannot occur in the same shape. That is an argument
in favor of the bare-clone layout that neither prior rejection had, because
neither predates the trunk-staleness bug being understood. STA-151 should
weigh it. It is not decisive: fixing `fast_forward_trunk` is cheap and a repo
layout migration is not.

---

## 4. STA-62: overlap with the streams design, and what it established

**Verdict: the mechanism is correct and I verified it live. The work STA-62
actually asks for (tests and a documented invariant) is still outstanding. No
overlap with the streams *design*; it is validation that streams makes
load-bearing.**

STA-62's finding is confirmed. `Git::git_dir` (`crates/stacc-git/src/lib.rs:584-587`)
shells `git rev-parse --git-dir`, which returns the per-worktree private
directory in a linked worktree. Probed from this worktree:

```
$ git rev-parse --git-dir
/Users/jilliankozyra/projects/stacc/.git/worktrees/sta-156-tracker-doc
$ git rev-parse --git-common-dir
/Users/jilliankozyra/projects/stacc/.git
```

Both recovery records resolve through it, so both are per-worktree isolated:

* `stacc-continue.json` via `recovery::write_continuation(git_dir, op)`
  (`crates/stacc-core/src/recovery.rs:193-205`), read at
  `operations.rs:3017`
* `stacc-conflict-context.json` at `operations.rs:3712-3714`, cleared at
  `operations.rs:3578-3580`

Both write to a pid-suffixed temp sibling then rename, so a crash mid-write
cannot leave a torn record (`recovery.rs:199-204`).

What is missing is what STA-62 scoped as its deliverable. `crates/stacc/tests/`
contains `worktree_safety.rs` (from STA-65: the guard) and `recovery.rs`, but
**no `worktree_recovery.rs`**, so the two-worktree isolation tests were never
written. Nor is there a regression guard or comment at `git_dir()` warning
against a switch to `--git-common-dir`, which is the whole point of "fence".

The isolation is therefore correct by construction but unprotected. One
well-meaning change to `git_dir()` collapses it, and under streams that would
mean stream A's `stacc continue` resuming stream B's rebase.

**Recommendation.** STA-62 stays a separate ticket, unchanged in scope, and
STA-153 should sequence it as must-land-before rather than parallel-with. It is
cheap (two tests plus a comment) and it is the only thing standing between the
current correct-by-accident state and a durable invariant.

---

## 5. The `WorktreeConflict` guard: is refusing still right?

**Verdict: needs a design decision, and it is the one item on this list that
the streams requirements document must answer itself.**

The guard is right where it is applied to focused operations and wrong-shaped
where it is applied to bulk passes.

**Focused ops stay correct.** `modify` and `move` fail fast via
`guard_worktree` (`operations.rs:3331-3341`) with
`{"type":"worktree_conflict","branch":...,"worktree":...}`
(`crates/stacc/src/error.rs:155-160`). Under decision 5 a stream is closed
under descent, so a stream only ever rewrites its own branches from its own
worktree. This guard should be quiet in normal operation and should fire
exactly when something has gone wrong. Keep it, and make sure it is documented
(see item 2).

**Bulk passes are the problem.** `restack` and `sync` do not refuse; they skip
per-branch and report (`crates/stacc-core/src/ops.rs:286-293`). Note the
arithmetic: a worktree has exactly one branch checked out, so with N live
streams there are N branches in the repo that no bulk pass can rewrite, one per
stream, and each is that stream's current working branch, which is the branch
most likely to need a restack after a trunk advance.

So `stacc sync` run from stream A, after trunk moves, will:

* restack A's own branches fine
* skip each other stream's HEAD branch with a worktree-skipped report
* leave the branches *above* those skipped ones stranded, because the engine
  skips a branch whose base no longer resolves (`ops.rs:281-284`)

That is not a bug today, when a second worktree is rare. Under streams it is
the normal outcome of every sync, and the report will name other people's
work, which is noise the running agent cannot act on.

The same arithmetic hits merged-branch cleanup: `cleanup_merged_ref` skips a
branch checked out elsewhere (`operations.rs:3366-3371`), so a stream's merged
branches linger until that stream itself runs a command.

**The decision to make.** Should the bulk passes be stream-scoped? Three
shapes, and the requirements document should pick one:

1. **Global, as today.** Sync restacks everything it can and reports the rest.
   Zero code change, but every sync in a multi-stream repo emits skip noise
   about branches the caller does not own, and cross-stream stacks silently
   stop being restacked.
2. **Stream-scoped by default.** Sync restacks only the current stream's
   branches (plus trunk-based ancestors), with a flag to widen. Matches the
   isolation model streams is for, and makes the skip list empty in the normal
   case. Cost: a branch left in an ended-but-not-torn-down stream never gets
   restacked by anyone, so the dead-band rendering in STA-152 becomes the only
   thing that surfaces it.
3. **Global, but silent about other streams.** Restack what is reachable,
   report skips only for branches in the caller's own stream. Cheapest change,
   but it hides real staleness.

Shape 2 is the one that follows from decision 5. Flagging rather than deciding:
this is a grilling question, not a research answer, and it depends on whether
`stream` is a filter on state or only a rendering tag, which STA-149 settles.

Graduated onto the map as
[STA-161](https://linear.app/tiny-dog-tech/issue/STA-161/are-sync-and-restack-stream-scoped-or-global),
blocked by STA-149, blocking the requirements document.

---

## Found while investigating

### A. The remote state ref is permanently wedged, silently

**New bug. Reproduced in this repo, today.**

`StateStore::push` (`crates/stacc-state/src/store.rs:256-259`) pushes
`refs/stacc/data:refs/stacc/data` through `Git::push`
(`crates/stacc-git/src/lib.rs:567-569`), which is a plain non-forced
`git push`. Both call sites treat failure as a warning on **stderr**:

```rust
if let Err(err) = store.push(&repo.remote) {
    eprintln!("warning: could not push state to `{}`: {err}", repo.remote);
}
```

(`operations.rs:3120-3122` and `operations.rs:3450-3452`.)

`StateStore::fetch` (`store.rs:262-265`) exists and has **zero production
callers**. The only `fetch` in command code is `fast_forward_trunk`'s trunk
fetch (`operations.rs:3731`).

So once the remote `refs/stacc/data` advances past the local one, nothing can
ever reconcile it. Every subsequent push is rejected as non-fast-forward, and
the rejection is a stderr warning that `--json` consumers are explicitly told
to ignore.

This is not hypothetical:

```
$ git ls-remote origin "refs/stacc/*"
199f4cc10e6d979ad4d3ca5fa7bdc72d9c0b8277	refs/stacc/data
$ git rev-parse refs/stacc/data
8fcddced848ad7c437bb5e9e683858e5b0f93d4c
$ git merge-base --is-ancestor 199f4cc1... refs/stacc/data
fatal: Not a valid commit name 199f4cc10e6d979ad4d3ca5fa7bdc72d9c0b8277
```

The remote tip is not merely ahead, it is a commit this clone has never
fetched. Every state push from this checkout is being rejected right now, and
the only trace is a stderr line. STA-137's report noticed exactly this warning
and guessed it might be a separate concurrency issue. It is, and this is it.

Severity for streams specifically is **low**, and this is worth being precise
about: linked worktrees share one ref store (verified in item 4, and
`refs/stacc/data` resolves fine from this linked worktree), so streams
coordinate entirely through the *local* ref and the local CAS. The remote ref
is not on the streams path. It matters for the multi-machine story and it
matters because a permanently-failing write that only warns is a bad shape to
leave lying under a feature that multiplies writers.

**Recommendation.** Filed as
[STA-160](https://linear.app/tiny-dog-tech/issue/STA-160/state-ref-push-is-best-effort-and-nothing-ever-fetches-it-so-a),
independent of streams. Two parts: a fetch-then-CAS-retry on the state push,
and promoting a persistent push failure out of stderr into the JSON envelope.
Do not fold into the streams design.

### B. `undo` is global across streams, twice over

**New design decision. Feeds STA-159 directly.**

`undo` (`operations.rs:2878-2939`) has two properties that are fine with one
writer and wrong with several:

1. **The version chain is shared.** `version_back(steps)`
   (`store.rs:246-253`) walks `refs/stacc/data~<steps>`. Under parallel
   streams that chain interleaves every stream's writes, so `stacc undo` with
   `--steps 1` in stream A may target a version written by stream B. "One
   version back" is not "one of my operations back".
2. **The payload is whole-state replacement.** `store.save(&target_state)`
   (`operations.rs:2939`) writes the target snapshot over current state. Any
   branch stream B tracked after that snapshot vanishes from state. Its git
   ref survives, so it becomes an untracked branch with an open PR, which is
   adjacent to the unreachable shape STA-158 describes.

The tip-restore loop does consult the worktree guard
(`operations.rs:2915-2918`) and reports `worktree_skipped`, so B's *files* are
safe. It is B's *tracking* that is lost.

STA-159 asks what `undo` restores after a `stream end`. This is the prior
question: what does `undo` mean at all when the chain is shared? Options range
from stream-scoping the walk (needs a stream tag on each version, so a
state-shape change) to renaming the concept so it is honestly global.

**Recommendation.** Add this to STA-159 rather than opening a ticket, since
STA-159 already owns the undo semantics and is already blocked on STA-149. It
should be answered before, not after, the teardown question.

### C. Harness worktrees are git-locked

**New constraint on the teardown design. Feeds STA-149 and STA-151.**

```
$ git worktree list --porcelain
worktree /Users/jilliankozyra/projects/stacc
HEAD 44ae7357e545bc3719883d910cc4b7c22a10af29
branch refs/heads/main

worktree /Users/jilliankozyra/projects/stacc/.claude/worktrees/sta-156-tracker-doc
HEAD 41993764417f73b0f129e548e9d0dd625a3d8414
branch refs/heads/jillian/sta-156-docs-record-the-linear-issue-tracker-and-wayfinding
locked claude session sta-156-tracker-doc (pid 35960 start Wed Aug  5 03:58:25 2026)
```

Claude Code locks the worktrees it creates, with a reason naming the session
and pid. `git worktree remove` refuses a locked worktree unless forced, and
`git worktree prune` skips it.

Decision 4 has `stream end` remove the worktree directory. If STA-149 answers
"adopter", `stream end` is removing a directory it did not create, whose lock
it did not take, whose lock reason names another process. It has to either
`git worktree unlock` first (silently overriding the harness's claim) or refuse
and tell the human. `stacc` currently has no worktree add/remove/lock
primitive at all: `Git::worktrees` (`lib.rs:602-622`) is read-only.

This does not change the concurrency answer; it is a fact the ownership and
layout tickets need.

### D. Dropped-tip retention is shared across streams

Note only, no action. `refs/stacc/dropped/*` is capped at `UNDO_RETENTION = 50`
globally (`store.rs:24,304-312`), and the namespace lives in the shared ref
store. N streams dropping branches drain one shared 50-slot window, so the
effective per-stream recovery depth is `50/N`. Ordering is by recorded drop
time, not commit date (`store.rs:298-312`), so the window still evicts oldest
drop first, which is the right behavior. At any realistic N this is fine.

---

## What the requirements document should carry

1. State-ref concurrency is **solved**, not open work. Correct
   `plans/stacc.md:116`, which still lists it under v2.
2. `sync` and `restack` scoping under multiple streams is **open**, and it is
   the concurrency question streams actually creates (item 5).
3. `undo` semantics under a shared version chain are **open** and belong to
   STA-159 (finding B).
4. STA-137 and STA-62 are **prerequisites**, not streams work. Both are small.
5. The `stream end` teardown must reckon with git worktree locks (finding C).
6. The remote-state-push bug (finding A) is **independent**. Do not let it into
   the streams scope.

## Sources

All `file:line` references are to this repository at commit `4199376`.

* `crates/stacc-state/src/store.rs` (CAS, backoff, retention, push/fetch)
* `crates/stacc-git/src/lib.rs` (`update_ref`, `push`, `git_dir`, `worktrees`,
  `branch_checked_out_elsewhere`, `write_tree` index)
* `crates/stacc/src/commands/operations.rs` (`fast_forward_trunk`, `undo`,
  `guard_worktree`, `cleanup_merged_ref`, `persist_restack`, state push sites)
* `crates/stacc-core/src/ops.rs` (restack per-branch worktree skip)
* `crates/stacc-core/src/recovery.rs` (continuation record)
* `crates/stacc/src/error.rs` (error envelope, `contention` discriminator)
* `crates/stacc/assets/agent/skill-content.md` (the documented agent contract)
* `docs/plans/2026-06-07-001-feat-parallel-agent-foundation-plan.md` (F1, KTD-1,
  KTD-5, U6)
* `plans/stacc.md:116` (the stale v2 line)
* Live probes: `git rev-parse --git-dir`, `git rev-parse --git-common-dir`,
  `git worktree list --porcelain`, `git ls-remote origin "refs/stacc/*"`, run
  from `.claude/worktrees/sta-156-tracker-doc` on 2026-08-06.
