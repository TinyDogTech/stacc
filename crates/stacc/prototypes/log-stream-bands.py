#!/usr/bin/env python3
"""PROTOTYPE, delete me. Answers STA-152: what does `stacc log` print once streams exist?

Throwaway. Ports the lane algorithm from crates/stacc/src/commands/log.rs
(post_order / pre_order / seekers_of / column_for / free_lane / connector_row /
node_row / cont_row) so the band variants are checked against the REAL graph
renderer, not against hand-drawn ASCII.

Run:  python3 crates/stacc/prototypes/log-stream-bands.py
      python3 crates/stacc/prototypes/log-stream-bands.py --only A1
"""

import argparse
import json
import sys

CURRENT = "◉"  # ◉  the branch checked out HERE
BRANCH = "○"  # ○
HEAD_GLYPH = "◈"  # ◈  checked out in ANOTHER stream's worktree
HEAD_GLYPH_ON = False
TRUNK = "main"

# --- Fixture ---------------------------------------------------------------
# Deliberately covers every case STA-152 asks about:
#   - two live streams, one stacc-locked, one foreign-locked (Claude Code)
#   - a stream holding TWO independent trunk-based roots (decision 5 permits it)
#   - a dead stream (worktree directory gone), with a needs-restack branch
#   - branches in the main checkout (no stream)
#   - current branch, needs_restack, a draft PR, an approved PR

# name -> (base, stream, pr number, pr label, needs_restack)
BRANCHES = {
    "jillian/sta-140-parsing": (TRUNK, "pickups", 140, "open", False),
    "jillian/sta-141-render": ("jillian/sta-140-parsing", "pickups", 141, "open", False),
    "jillian/sta-150-auth": (TRUNK, "sen-auth", 150, "approved", False),
    "jillian/sta-151-tokens": ("jillian/sta-150-auth", "sen-auth", 151, "draft", False),
    "jillian/sta-155-schema": (TRUNK, "sen-auth", 155, "open", False),
    "jillian/sta-120-spike": (TRUNK, "old-spike", 120, "open", True),
    "jillian/sta-160-docs": (TRUNK, None, 160, "open", False),
}

# stream -> (recorded path, HEAD branch, alive, lock)
STREAMS = {
    "pickups": (".stacc/worktrees/pickups", "jillian/sta-141-render", True, "stacc"),
    "sen-auth": (".claude/worktrees/sen-auth", "jillian/sta-150-auth", True, "foreign"),
    "old-spike": (".stacc/worktrees/old-spike", "jillian/sta-120-spike", False, "stacc"),
}

CURRENT_BRANCH = "jillian/sta-141-render"

# Order streams render in: live first, dead last, each alphabetical.
def stream_order():
    live = sorted(n for n in STREAMS if STREAMS[n][2])
    dead = sorted(n for n in STREAMS if not STREAMS[n][2])
    return live + dead


def base_of(name):
    return BRANCHES[name][0] if name in BRANCHES else None


def children_map(names):
    """base -> sorted children, restricted to `names` (mirrors log.rs child_map)."""
    out = {}
    for n in sorted(names):
        b = base_of(n)
        if b in names or b == TRUNK:
            out.setdefault(b, []).append(n)
    for kids in out.values():
        kids.sort()
    return out


# --- Lane algorithm, ported from log.rs ------------------------------------

def post_order(node, children, out):
    for kid in children.get(node, []):
        post_order(kid, children, out)
    out.append(node)


def pre_order(node, children, out):
    out.append(node)
    for kid in children.get(node, []):
        pre_order(kid, children, out)


def free_lane(lanes):
    for c, l in enumerate(lanes):
        if l is None:
            return c
    lanes.append(None)
    return len(lanes) - 1


def free_lane_after(lanes, min_col):
    for c in range(min_col + 1, len(lanes)):
        if lanes[c] is None:
            return c
    lanes.append(None)
    return len(lanes) - 1


def render_width(lanes, node_col):
    active = [c for c, l in enumerate(lanes) if l is not None]
    return max(max(active) if active else 0, node_col) + 1


def node_row(lanes, node_col, glyph, label):
    s = []
    for c in range(render_width(lanes, node_col)):
        if c == node_col:
            s.append(glyph)
        elif c < len(lanes) and lanes[c] is not None:
            s.append("│")  # │
        else:
            s.append(" ")
        s.append(" ")
    return "".join(s) + label


def cont_row(lanes, node_col, content):
    s = []
    for c in range(render_width(lanes, node_col)):
        s.append("│" if c < len(lanes) and lanes[c] is not None else " ")
        s.append(" ")
    return "".join(s) + content


def connector_row(lanes, node_col, branch_cols, end):
    span_end = max(list(branch_cols) + [node_col])
    active = [c for c, l in enumerate(lanes) if l is not None]
    max_active = max(active) if active else node_col
    width = max(span_end, max_active) + 1
    row = [" "] * (2 * width - 1)
    row[2 * node_col] = "├"  # ├
    for i in range(2 * node_col + 1, 2 * span_end):
        row[i] = "─"  # ─
    for col in branch_cols:
        row[2 * col] = end
    for c in range(width):
        if c == node_col or c in branch_cols:
            continue
        if not (c < len(lanes) and lanes[c] is not None):
            continue
        row[2 * c] = "┼" if node_col < c < span_end else "│"  # ┼ │
    return "".join(row)


def render(roots, children, printed, full=False, reverse=False, tag=None):
    """The log.rs render, generalised to a forest. `printed` is the set of nodes
    on this canvas; a lane seeking a base outside it closes, which is what lets a
    band render without drawing its edge to trunk."""
    order = []
    for r in roots:
        (pre_order if reverse else post_order)(r, children, order)
    lanes, out = [], []
    for node in order:
        seekers = [c for c, l in enumerate(lanes) if l == node]
        node_col = seekers[0] if seekers else free_lane(lanes)

        if not reverse and len(seekers) > 1:
            out.append(connector_row(lanes, node_col, seekers[1:], "┘"))  # ┘
            for l in seekers[1:]:
                lanes[l] = None

        if node == CURRENT_BRANCH:
            glyph = CURRENT
        elif HEAD_GLYPH_ON and node in {v[1] for v in STREAMS.values() if v[2]}:
            glyph = HEAD_GLYPH  # checked out in ANOTHER stream's worktree
        else:
            glyph = BRANCH
        out.append(node_row(lanes, node_col, glyph, label(node, tag)))

        if reverse:
            kids = children.get(node, [])
            extra = []
            if not kids:
                lanes[node_col] = None
            else:
                lanes[node_col] = kids[0]
                for kid in kids[1:]:
                    lane = free_lane_after(lanes, node_col)
                    lanes[lane] = kid
                    extra.append(lane)
            if extra:
                out.append(connector_row(lanes, node_col, extra, "┐"))  # ┐
        else:
            b = base_of(node)
            lanes[node_col] = b if b in printed else None

        if full:
            for m in meta(node):
                out.append(cont_row(lanes, node_col, m))
            out.append(cont_row(lanes, node_col, ""))
    return out


# --- Labels ----------------------------------------------------------------

PR_COL = 42


def label(name, tag=None):
    if name == TRUNK:
        return TRUNK
    _, stream, num, state, restack = BRANCHES[name]
    text = name
    if name == CURRENT_BRANCH:
        text += " (current)"
    pad = " " * max(1, PR_COL - len(text))
    line = f"{text}{pad}#{num} {state}"
    if restack:
        line += "  needs restack"
    if tag == "suffix" and stream:
        line += f"  [{stream}]"
    if tag == "suffix" and not stream:
        line += "  [-]"
    return line


def meta(name):
    """The expanded block. The PR line that lives here today is dropped: the
    branch row now carries `#NNN state`, so repeating it is pure duplication."""
    if name == TRUNK:
        return ["3 weeks ago"]
    _, _, num, state, restack = BRANCHES[name]
    return ["9 hours ago", "0a6da4e - feat(stacc): [STA-1xx] do the thing", "CI pass"]


HEADER_STYLE = "full"


def band_header(stream, style="full"):
    path, head, alive, lock = STREAMS[stream]
    if not alive:
        return f"▸ stream: {stream}  (worktree gone)  end it: stacc stream end {stream}"
    mark = "" if lock == "stacc" else "   ⚠ foreign lock"
    if style == "leaf":
        path = path.rsplit("/", 1)[-1]
    if HEADER_STYLE == "no_head":
        return f"▸ stream: {stream}   {path}{mark}"
    return f"▸ stream: {stream}   {path}   HEAD: {head.rsplit('/', 1)[-1]}{mark}"


def members(stream):
    return {n for n, v in BRANCHES.items() if v[1] == stream}


def roots_of(names):
    return sorted(n for n in names if base_of(n) not in names)


# --- Variants --------------------------------------------------------------

def variant_A(reverse=False, header_above=True, full=False, path_style="full", focus=False):
    """Banded subgraphs: trunk once, each stream an indented sub-canvas.
    This is the handoff sketch made real. Note what it costs: the edge from each
    band to trunk is NOT drawn, it is implied by the band frame.

    `focus`: expand the metadata block only for the band holding the current
    branch; every other band stays one row per branch."""
    out = []
    if reverse:
        out.append(TRUNK)
        out.append("│")
    blocks = []
    for s in stream_order():
        names = members(s)
        ch = children_map(names)
        deep = full or (focus and CURRENT_BRANCH in names)
        body = render(roots_of(names), ch, names, full=deep, reverse=reverse)
        blocks.append((band_header(s, path_style), body))
    solo = {n for n, v in BRANCHES.items() if v[1] is None}
    if solo:
        ch = children_map(solo)
        deep = full or (focus and CURRENT_BRANCH in solo)
        blocks.append(("▸ (main checkout)", render(roots_of(solo), ch, solo, full=deep, reverse=reverse)))

    for i, (head, body) in enumerate(blocks):
        last = i == len(blocks) - 1
        stem = "└─ " if (last and reverse) else "├─ "
        gut = "   " if (last and reverse) else "│  "
        rows = ([f"{stem}{head}"] + [f"{gut}  {b}" for b in body]) if header_above else (
            [f"{gut}  {b}" for b in body] + [f"{stem}{head}"]
        )
        out.extend(rows)
        if not last:
            out.append("│")
    if not reverse:
        out.append("│")
        out.append(TRUNK)
    return out


def variant_B(reverse=False, full=False):
    """No bands. One graph exactly as today, each row tagged with its stream."""
    names = set(BRANCHES)
    ch = children_map(names | {TRUNK})
    return render([TRUNK], ch, names | {TRUNK}, full=full, reverse=reverse, tag="suffix")


def variant_C(reverse=False, full=False):
    """Left gutter: the graph is untouched, a stream column is prepended and the
    name printed once per contiguous run."""
    names = set(BRANCHES)
    ch = children_map(names | {TRUNK})
    order = []
    for r in [TRUNK]:
        (pre_order if reverse else post_order)(r, ch, order)
    rows = render([TRUNK], ch, names | {TRUNK}, full=full, reverse=reverse)
    width = max(len(s) for s in list(STREAMS) + ["(main)"]) + 2
    out, prev, ri = [], object(), 0
    for node in order:
        stream = BRANCHES[node][1] if node in BRANCHES else None
        name = stream or ("" if node == TRUNK else "(main)")
        shown = name if name != prev else ""
        prev = name
        out.append(f"{shown:<{width}}│ {rows[ri]}")
        ri += 1
        if full:
            while ri < len(rows) and not rows[ri].lstrip().startswith((CURRENT, BRANCH)):
                out.append(f"{'':<{width}}│ {rows[ri]}")
                ri += 1
    return out


def variant_D(reverse=False, full=False):
    """Section rules: one graph, real edges preserved, a header rule inserted
    where each stream's run starts. No indent, so lanes never shift."""
    names = set(BRANCHES)
    ch = children_map(names | {TRUNK})
    order = []
    (pre_order if reverse else post_order)(TRUNK, ch, order)
    rows = render([TRUNK], ch, names | {TRUNK}, full=full, reverse=reverse)
    out, seen, ri = [], set(), 0
    for node in order:
        stream = BRANCHES[node][1] if node in BRANCHES else None
        if stream and stream not in seen:
            seen.add(stream)
            out.append(f"── {band_header(stream)} " + "─" * 4)
        out.append(rows[ri])
        ri += 1
        if full:
            while ri < len(rows) and not rows[ri].lstrip().startswith((CURRENT, BRANCH)):
                out.append(rows[ri])
                ri += 1
    return out


def side_by_side():
    """Option 1 (glyph in place) against option 4 (HEAD dropped entirely), the
    same repo state rendered both ways."""
    left = with_head_glyph(variant_A)
    global HEADER_STYLE
    HEADER_STYLE = "no_head"
    try:
        right = variant_A()
    finally:
        HEADER_STYLE = "full"

    out = ["OPTION 1: header drops HEAD, the branch is marked in place with ◈",
           "-" * 78]
    out += left
    out += ["", "◉ checked out in THIS worktree    ◈ checked out in that stream's worktree",
            "", "",
            "OPTION 4: header drops HEAD, nothing marks it anywhere in the graph",
            "-" * 78]
    out += right
    out += ["", "(to learn sen-auth is sitting on sta-150-auth you must run `stacc stream list`)",
            "", "",
            "THE ONLY DIFFERENCE, the sen-auth band, aligned:", "-" * 78,
            "  option 1                                    option 4"]
    l = [r for r in left if "sen-auth" in r or "sta-15" in r]
    r = [x for x in right if "sen-auth" in x or "sta-15" in x]
    for a, b in zip(l, r):
        out.append(f"  {a[:42]:<42}  {b[:42]}")
    return out


def with_head_glyph(fn, **kw):
    """Render with the HEAD field dropped from every header and the branch each
    other stream has checked out marked with its own glyph instead."""
    global HEAD_GLYPH_ON, HEADER_STYLE
    HEAD_GLYPH_ON, HEADER_STYLE = True, "no_head"
    try:
        return fn(**kw)
    finally:
        HEAD_GLYPH_ON, HEADER_STYLE = False, "full"


def canonical(reverse=False):
    """THE CHOSEN SHAPE, every decision from the STA-152 session applied:

    frame whenever any stream exists and never collapse; zero streams renders
    today's flat graph untouched; forward by default with the header above its
    band; the band holding the current branch expands to the full metadata block
    and every other band is one row per branch; the header carries name, path,
    and a foreign-lock warning but NOT HEAD; the branch another stream has
    checked out is marked with a glyph in place; dead bands sort last and carry
    the remedy; independent roots inside a band are separated by a blank line so
    they do not read as one chain.
    """
    global HEAD_GLYPH_ON, HEADER_STYLE
    HEAD_GLYPH_ON, HEADER_STYLE = True, "no_head"
    try:
        blocks = []
        for s in stream_order():
            names = members(s)
            ch = children_map(names)
            deep = CURRENT_BRANCH in names
            # Each trunk-based root is its own canvas, so two independent roots
            # never share a column and read as one stack.
            body = []
            for i, root in enumerate(roots_of(names)):
                if i:
                    body.append("")
                body += render([root], ch, names, full=deep, reverse=reverse)
            blocks.append((band_header(s), body))
        solo = {n for n, v in BRANCHES.items() if v[1] is None}
        if solo:
            ch = children_map(solo)
            body = []
            for i, root in enumerate(roots_of(solo)):
                if i:
                    body.append("")
                body += render([root], ch, solo, full=CURRENT_BRANCH in solo, reverse=reverse)
            blocks.append(("▸ (main checkout)", body))

        out = [TRUNK, "│"] if reverse else []
        for i, (head, body) in enumerate(blocks):
            last = i == len(blocks) - 1
            stem = "└─ " if (last and reverse) else "├─ "
            gut = "   " if (last and reverse) else "│  "
            out.append(f"{stem}{head}")
            rows = [f"{gut}  {b}".rstrip() for b in body]
            while rows and rows[-1] == gut.rstrip():
                rows.pop()  # the expanded block's trailing spacer, not a separator
            out += rows
            if not (last and reverse):
                out.append("│")
        if not reverse:
            out.append(TRUNK)
        out += ["", "◉ checked out here    ◈ checked out in that stream's worktree"]
        return out
    finally:
        HEAD_GLYPH_ON, HEADER_STYLE = False, "full"


def variant_collapsed(full=False, focus=False, header=True, stream="pickups"):
    """The collapsed case: one group, so no frame and no indent, but the stream's
    facts still need a home. `header` prints them as a single unindented line
    above the flat graph."""
    names = members(stream)
    ch = children_map(names | {TRUNK})
    out = [band_header(stream)] if header else []
    return out + render([TRUNK], ch, names | {TRUNK}, full=full or focus)


def single_stream(fn, with_main=False, **kw):
    """The collapse case: exactly one stream. `with_main` also keeps a branch in
    the main checkout, so two bands would render rather than one."""
    global BRANCHES, STREAMS
    keep = {k: v for k, v in BRANCHES.items()
            if v[1] == "pickups" or (with_main and v[1] is None)}
    saved_b, saved_s = BRANCHES, STREAMS
    BRANCHES, STREAMS = keep, {"pickups": saved_s["pickups"]}
    try:
        return fn(**kw)
    finally:
        BRANCHES, STREAMS = saved_b, saved_s


def no_streams(fn, **kw):
    """The zero-stream case: must stay byte-identical to what ships today."""
    global BRANCHES, STREAMS
    keep = {k: (v[0], None, v[2], v[3], v[4]) for k, v in BRANCHES.items()
            if v[1] in (None, "pickups")}
    saved_b, saved_s = BRANCHES, STREAMS
    BRANCHES, STREAMS = keep, {}
    try:
        return fn(**kw)
    finally:
        BRANCHES, STREAMS = saved_b, saved_s


# --- Header composition ----------------------------------------------------

# Realistic worst case for THIS repo: stream names are Linear issue ids
# (STA-148), branch names are Linear slugs, and an adopted worktree sits at
# whatever path the state ref recorded.
REAL = [
    ("sta-152", ".stacc/worktrees/sta-152",
     "jillian/sta-152-stacc-log-stream-band-rendering", True, "stacc"),
    ("sta-156", ".claude/worktrees/sta-156-tracker-doc",
     "jillian/sta-156-docs-record-the-linear-issue-tracker-and-wayfinding", True, "foreign"),
    ("sta-120", "/Users/jilliankozyra/scratch/wt-auth-spike",
     "jillian/sta-120-spike", False, "stacc"),
]


def header_variants():
    out = ["    " + "".join(str((i // 10) % 10) for i in range(1, 81)),
           "    " + "".join(str(i % 10) for i in range(1, 81)),
           ""]

    def emit(title, fn):
        out.append(title)
        for row in REAL:
            line = "├─ ▸ " + fn(*row)
            flag = "" if len(line) <= 80 else f"   <-- {len(line)} cols, overflows"
            out.append(line + flag)
        out.append("")

    emit("H1  name, full recorded path, HEAD, lock",
         lambda n, p, h, live, lock: (
             f"stream: {n}   {p}   HEAD: {h}" + ("" if lock == "stacc" else "  ⚠ foreign lock")
             if live else f"stream: {n}  (worktree gone)  end it: stacc stream end {n}"))

    emit("H2  name, path, HEAD with the branch prefix stripped, lock",
         lambda n, p, h, live, lock: (
             f"stream: {n}   {p}   HEAD: {h.rsplit('/', 1)[-1]}"
             + ("" if lock == "stacc" else "  ⚠ foreign lock")
             if live else f"stream: {n}  (worktree gone)  end it: stacc stream end {n}"))

    emit("H3  name, path, lock. No HEAD field at all",
         lambda n, p, h, live, lock: (
             f"stream: {n}   {p}" + ("" if lock == "stacc" else "   ⚠ foreign lock")
             if live else f"stream: {n}  (worktree gone)  end it: stacc stream end {n}"))

    emit("H4  name and lock only; path on a second, dimmed line",
         lambda n, p, h, live, lock: (
             f"stream: {n}" + ("" if lock == "stacc" else "   ⚠ foreign lock")
             if live else f"stream: {n}   (worktree gone)  end it: stacc stream end {n}"))
    out.append("    (H4 continues each header with an indented second row:)")
    out.append("│     .claude/worktrees/sta-156-tracker-doc   HEAD: sta-156-docs-record-the...")
    return out


# --- JSON shapes -----------------------------------------------------------

def json_additive():
    """Option 1: each node gains an optional `stream`; a sibling `streams` array
    carries the band metadata. Purely additive, no schema_version bump."""
    return {
        "trunk": TRUNK,
        "streams": [
            {"name": s, "path": STREAMS[s][0], "head": STREAMS[s][1],
             "live": STREAMS[s][2], "lock": STREAMS[s][3]}
            for s in stream_order()
        ],
        "stack": [
            {"name": "jillian/sta-140-parsing", "base": TRUNK, "stream": "pickups",
             "change": {"number": 140, "state": "open"},
             "children": [{"name": "jillian/sta-141-render", "base": "jillian/sta-140-parsing",
                           "stream": "pickups", "current": True,
                           "change": {"number": 141, "state": "open"}}]},
            {"name": "jillian/sta-160-docs", "base": TRUNK,
             "change": {"number": 160, "state": "open"}},
        ],
        "schema_version": 3,
    }


def json_nested():
    """Option 2: `stack` is re-rooted under `streams`, matching the banded
    picture. Breaks every consumer that walks `stack` from the top."""
    return {
        "trunk": TRUNK,
        "streams": [
            {"name": "pickups", "path": ".stacc/worktrees/pickups",
             "head": "jillian/sta-141-render", "live": True, "lock": "stacc",
             "stack": [{"name": "jillian/sta-140-parsing", "base": TRUNK,
                        "change": {"number": 140, "state": "open"},
                        "children": [{"name": "jillian/sta-141-render",
                                      "base": "jillian/sta-140-parsing", "current": True,
                                      "change": {"number": 141, "state": "open"}}]}]},
        ],
        "stack": [{"name": "jillian/sta-160-docs", "base": TRUNK,
                   "change": {"number": 160, "state": "open"}}],
        "schema_version": 4,
    }


# --- Driver ----------------------------------------------------------------

VARIANTS = {
    "CHOSEN": ("CHOSEN  the shape settled in the STA-152 session (forward, the default)",
               canonical),
    "CHOSEN-rev": ("CHOSEN-rev  the same under --reverse", lambda: canonical(reverse=True)),
    "A1": ("A1  banded subgraphs, header ABOVE its band (forward, trunk at bottom)",
           lambda: variant_A(reverse=False, header_above=True)),
    "A2": ("A2  banded subgraphs, header BELOW its band, next to trunk (forward)",
           lambda: variant_A(reverse=False, header_above=False)),
    "A3": ("A3  banded subgraphs, reverse (--reverse, trunk on top). This is the sketch.",
           lambda: variant_A(reverse=True, header_above=True)),
    "A4": ("A4  A3 with leaf-only paths in the header",
           lambda: variant_A(reverse=True, header_above=True, path_style="leaf")),
    "B": ("B   no bands, per-row stream tag (forward, the default orientation)",
          lambda: variant_B()),
    "B-rev": ("B-rev  same, reverse", lambda: variant_B(reverse=True)),
    "C": ("C   left gutter column (forward)", lambda: variant_C()),
    "D": ("D   section rules inside one graph, lanes untouched (forward)",
          lambda: variant_D()),
    "A-full": ("A-full  banded subgraphs with the full-form metadata block",
               lambda: variant_A(reverse=True, header_above=True, full=True)),
    "A-focus": ("A-focus  full metadata for the CURRENT stream only, one row elsewhere",
                lambda: variant_A(reverse=False, header_above=True, focus=True)),
    "B-full": ("B-full  tags with the full-form metadata block",
               lambda: variant_B(full=True)),
    "A-solo": ("A-solo  ONE stream, nothing in the main checkout (forward, focus)",
               lambda: single_stream(variant_A, focus=True)),
    "A-solo-main": ("A-solo-main  ONE stream PLUS a branch in the main checkout: two bands",
                    lambda: single_stream(variant_A, with_main=True, focus=True)),
    "A-none": ("A-none  ZERO streams: must be byte-identical to today's output",
               lambda: no_streams(variant_A, focus=True)),
    "A-collapsed": ("A-collapsed  ONE group: no frame, but a header line keeps the stream facts",
                    lambda: variant_collapsed(focus=True)),
    "A-collapsed-bare": ("A-collapsed-bare  ONE group, no frame and no header: facts are simply gone",
                         lambda: variant_collapsed(focus=True, header=False)),
    "A-collapsed-dead": ("A-collapsed-dead  ONE group, but the stream is dead: collapsing hides the warning",
                         lambda: variant_collapsed(focus=True, stream="old-spike")),
    "H": ("H   band header composition, against this repo's real name lengths",
          header_variants),
    "H-glyph": ("H-glyph  no HEAD field: mark another stream's checked-out branch in place",
                lambda: with_head_glyph(variant_A, focus=True)),
    "H-vs": ("H-vs  option 1 (mark in place) against option 4 (drop HEAD entirely)",
             side_by_side),
    "B-solo": ("B-solo  ONE stream, tags", lambda: single_stream(variant_B)),
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", help="render just this variant key")
    ap.add_argument("--json", action="store_true", help="print the two JSON shapes")
    args = ap.parse_args()

    if args.json:
        print("=== JSON option 1: additive (stream field + streams array) ===")
        print(json.dumps(json_additive(), indent=2))
        print("\n=== JSON option 2: stack nested under streams (schema bump) ===")
        print(json.dumps(json_nested(), indent=2))
        return

    keys = [args.only] if args.only else list(VARIANTS)
    for k in keys:
        title, fn = VARIANTS[k]
        print("=" * 78)
        print(title)
        print("=" * 78)
        for line in fn():
            print(line.rstrip())
        print()


if __name__ == "__main__":
    sys.exit(main())
