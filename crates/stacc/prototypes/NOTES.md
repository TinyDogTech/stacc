# PROTOTYPE NOTES: `stacc log` stream bands (STA-152)

Throwaway. Delete once the answer is folded into the requirements document.

**Question.** What does `stacc log` print once streams exist? The
[handoff](../../../docs/brainstorms/2026-08-05-stacc-streams-handoff.md) sketched a
banded shape and adopted three defaults without asking. Prototype the variants and
react to them rather than approving the sketch.

**Run.** `python3 crates/stacc/prototypes/log-stream-bands.py`
(`--only A3`, or `--json` for the two envelope shapes).

The script ports the real lane algorithm from `crates/stacc/src/commands/log.rs`
(`post_order` / `pre_order` / `seekers_of` / `column_for` / `free_lane` /
`connector_row` / `node_row` / `cont_row`) so every variant is checked against the
actual renderer rather than hand-drawn ASCII. Fixture covers two live streams (one
stacc-locked, one foreign-locked), a stream with two independent trunk-based roots,
a dead stream, main-checkout branches, `current`, `needs_restack`, and a draft PR.

## Findings

1. **The sketch only works in reverse orientation, which is not the default.**
   `stacc log` renders forward (trunk at the bottom, `render_forward`, `log.rs:344`);
   `--reverse` is opt-in. The sketch draws trunk on top. Variants A1 and A2 are the
   sketch forced into the default orientation and both read badly: A1 puts a band
   header above branches while trunk sits at the far end below, A2 puts the header
   adjacent to trunk but you read the whole band before learning whose it is.

2. **Bands are not an addition to the graph, they replace it at the top level.**
   In every A variant the `├─` joining a band to trunk is a frame, not an edge: each
   stream is rendered as an independent sub-canvas with its own lanes, and the edge
   from a stream root to trunk is implied. Drawing those edges is what the existing
   renderer exists to do.

3. **The strongest argument for bands is one nobody made: lane explosion.** A stream's
   trunk-based roots each claim a lane. Variant B (forward, no bands) already renders
   `├─┘─┘─┘─┘` across five columns with only four roots, and every metadata line in
   the full form carries that five-column gutter (see `B-full`). Streams makes many
   trunk-based roots the normal case, so the flat graph degrades precisely where
   streams gets used. Bands cap lane width at per-stream instead of per-repo.

4. **Bands plus the full form overflow a screen.** The full form is the default (`log.rs:153`).
   `A-full` is roughly 60 rows for four streams and seven branches. The sketch is
   implicitly the short form (one row per branch), but the short form is offline by
   contract, so its inline `#141 open` cannot be fetched. Either the sketch's form
   does not exist today, or the band view changes what the default form is.

5. **A single stream does not collapse on its own.** `A-solo` still draws a band. The
   collapse the handoff assumed has to be an explicit rule, and it matters: one stream
   is the common case for a human working alone.

6. **The tag suffix repeats redundantly.** Stream membership is closed under descent
   (decision 5), so `[pickups]` prints on every row of a chain to say something the
   chain's shape already says.

7. **The dead band reads as "end me" if the header carries the remedy**, not just a
   marker: `▸ stream: old-spike  (worktree gone)  ended? run `stacc stream end old-spike``.
   Detection is free from `git worktree list --porcelain` (`stacc-git/src/lib.rs:598`).

8. **The gutter variant (C) preserves real edges** and is the only variant that does
   while still grouping visually, but its width is the longest stream name and the
   trunk join row lands under an unrelated gutter label.

## Verdict

Settled 2026-08-06. Render it with `--only CHOSEN` (and `--only CHOSEN-rev`).

```
├─ ▸ stream: pickups   .stacc/worktrees/pickups
│    ◉ jillian/sta-141-render (current)          #141 open
│    │ 9 hours ago
│    │ 0a6da4e - feat(stacc): [STA-1xx] do the thing
│    │ CI pass
│    │
│    ○ jillian/sta-140-parsing                   #140 open
│      9 hours ago
│      0a6da4e - feat(stacc): [STA-1xx] do the thing
│      CI pass
│
├─ ▸ stream: sen-auth   .claude/worktrees/sen-auth   ⚠ foreign lock
│    ○ jillian/sta-151-tokens                    #151 draft
│    ◈ jillian/sta-150-auth                      #150 approved
│
│    ○ jillian/sta-155-schema                    #155 open
│
├─ ▸ stream: old-spike  (worktree gone)  end it: stacc stream end old-spike
│    ○ jillian/sta-120-spike                     #120 open  needs restack
│
├─ ▸ (main checkout)
│    ○ jillian/sta-160-docs                      #160 open
│
main

◉ checked out here    ◈ checked out in that stream's worktree
```

1. **Bands replace the trunk-rooted graph at the top level.** Trunk prints once;
   each stream is an independent canvas. The `├─` is a frame, not an edge. Chosen
   because lane width becomes per-stream rather than per-repo, which is the failure
   the flat graph has precisely where streams gets used.
2. **Forward stays the default**, header above its band. Bands must work in both
   orientations because `--reverse` already exists, so changing the default buys
   nothing.
3. **Frame whenever any stream exists, and never collapse.** Zero streams renders
   today's flat graph byte for byte; `stacc stream new` is the single visible moment
   the layout changes.
4. **The band holding the current branch expands** to the full metadata block; every
   other band is one row per branch. Roughly 28 rows for the four-stream fixture
   against 62 for the full block everywhere.
5. **The PR line leaves the metadata block.** The branch row carries `#NNN state`, so
   the block keeps age, `sha - subject`, CI, and needs-restack only.
6. **Header carries name, path, and a foreign-lock warning. Not HEAD.** Measured
   against this repo's real names the sketch's header runs 95 to 152 columns against
   an 80-column fallback (`--only H`).
7. **`◈` marks the branch another stream has checked out**, in place, beside `◉` for
   the branch checked out here. Costs no header width, survives a stream with several
   roots, and marks the branch a bulk `sync` cannot rewrite (STA-161).
8. **Dead bands sort last** and swap the path for `(worktree gone)  end it: stacc
   stream end <name>`. The remedy, not just a marker, since STA-147 left `stacc log`
   as the only place abandonment surfaces. No `◈` inside a dead band: no worktree
   holds anything.
9. **`--json` is additive and stays `schema_version` 3.** Each node gains an optional
   `stream`; a sibling `streams` array carries name, path, head, live, lock, and
   provenance. `stack` keeps its exact current shape, so installed agent contexts
   keep working and a stream-free repo emits identical JSON.

Settled without a question, stated as assumptions:

- **Path renders repo-relative** when the recorded path is under the repo root, and
  verbatim otherwise. Paths always come from state, never reconstructed from the
  convention (STA-151).
- **Provenance is `--json` only.** The header's density is spent on path and lock;
  lock is the actionable one because it gates `stream end`, provenance is not.
- **The lock warning is liveness-gated**, matching STA-151's amendment: a live pid
  warns, a dead pid does not (stacc reclaims it), an unparseable reason assumes live.
  So the warning appears exactly when `stream end` would refuse the directory.

Two things found while rendering the chosen shape:

- **Independent roots inside a band needed a separator.** With trunk outside the
  band's canvas the lanes close and get reused, so two trunk-based roots render in
  one column and read as a single chain. Fixed with a blank line between roots. The
  flat graph never had this problem because trunk anchored the join.
- **A band's canvas is a forest, not a tree.** The renderer currently always roots at
  trunk (`render_forward` / `render_reverse` take `ctx.trunk`). Bands need it to
  render several roots with no printed root, which is the one real change to the
  render layer. The lane code itself is untouched, and `lane` keeps meaning graph
  column, so bands do not fight `log.rs:461-492`.

Non-collisions confirmed:

- **STA-141** (per-check CI detail) composes rather than collides. It expands the
  metadata block; bands only decide which branches get an expanded block. The focus
  rule bounds its cost, since per-check rows would otherwise multiply across every
  stream.
- **The short form keeps its offline contract.** The collapsed one-row band is a full
  form row minus the metadata block, so it still fetches PR status. `stacc log short`
  stays offline and its rows carry no PR state. Two different one-row shapes, and the
  handoff sketch was neither.
