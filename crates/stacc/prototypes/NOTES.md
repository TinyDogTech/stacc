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

TBD, pending the decision session on STA-152.
