#!/usr/bin/env python3
"""What a screen reader gets from the PruneDisk size tree, read through AT-SPI (CT-F4, CT-F5, gate F6).

Runs inside a Linux desktop session that has the app open on the Size tree tab after a scan of `/`
that includes /home/tester/tree (the a11y tester kit), with an AT-SPI bus and xdotool (`. /tmp/run/env`
first). It reads the treegrid through the Table, Text and attribute interfaces, then drives the
keyboard with xdotool and records the focus and active-descendant events a screen reader (Orca)
follows.

  report   print the treegrid: roles (number and name), names, Text of each cell, attributes
  check    run the keyboard checks and exit 1 when any fails:
           - Tab order: the column headers come before the rows, and every header is named
           - after each key of the treegrid pattern the reader is told about a row (or a cell) of
             the treegrid with its name or text, level, posinset, setsize and rowindex
           - Left on a collapsed child moves to its parent row
           - focus stays on the selected row while the mouse wheel scrolls it far out of the
             rendered window and back, and Down then moves to the next row
           - each focused object's AT-SPI role number is the one its computed-role maps to

usage: treegrid_atspi.py report|check [--app prunedisk] [--rows 6] [--expect-shift N]

Exit codes of check:
  0  every check passed ("RESULT: PASS")
  4  wrong roles only: every finding is a role finding, printed as
     "  FAIL: <step>: a <computed-role> reaches the reader as '<name>' (R), expected E"
  1  other findings (keys, focus, attributes, names), with or without role findings
  3  could not check: no such application on the bus, or no tree table (open the Size tree
     tab after a scan); "CANNOT CHECK: ..." is printed
  2  bad usage (argparse)
A Python traceback also exits 1, so a crash can never look like exit 4.

--expect-shift N adds N to every expected role number, for the standing control in CI: on a build
whose roles are right, a shift of 1 must give exit 4 (the keyboard checks still pass, every role
check fails), which shows the role comparison can fail.
"""
import argparse
import json
import subprocess
import sys
import time

import pyatspi
from gi.repository import GLib

p = argparse.ArgumentParser()
p.add_argument("mode", choices=["report", "check"])
p.add_argument("--app", default="prunedisk")
p.add_argument("--rows", type=int, default=6, help="rows to print in the report")
p.add_argument("--expect-shift", type=int, default=0, help="add N to every expected role (CI control)")
a = p.parse_args()
WRONG_ONLY, CANNOT = 4, 3
ROLE_FINDING = "reaches the reader as"


def cannot(msg):
    print("CANNOT CHECK:", msg)
    sys.exit(CANNOT)

# What a screen reader is told: Orca follows focus and active-descendant events, not a polled state.
EVENTS = []


def on_event(e):
    if e.type.startswith("object:state-changed:focused") and e.detail1 == 1:
        EVENTS.append(("focus", e.source))
    elif e.type.startswith("object:active-descendant-changed") and e.any_data is not None:
        EVENTS.append(("active-descendant", e.any_data))


pyatspi.Registry.registerEventListener(on_event, "object:state-changed:focused", "object:active-descendant-changed")


def pump(seconds):
    end = time.time() + seconds
    ctx = GLib.MainContext.default()
    while time.time() < end:
        while ctx.pending():
            ctx.iteration(False)
        time.sleep(0.02)


def xdo(*args):
    subprocess.run(["xdotool", *args], check=True)
    pump(0.8)


def attrs(o):
    try:
        return dict(x.split(":", 1) for x in (o.getAttributes() or []) if ":" in x)
    except Exception:
        return {}


def text(o):
    try:
        t = o.queryText()
        return t.getText(0, t.characterCount)
    except Exception:
        return None


def deep_text(o, depth=0):
    """Text of an object, or of its descendants when it has none, as a reader assembles it."""
    own = text(o)
    if own is not None and own.replace("￼", "").strip():
        return own.replace("￼", "").strip()
    parts = []
    if depth < 4:
        for i in range(o.childCount):
            c = o.getChildAtIndex(i)
            if c is not None:
                parts.append(deep_text(c, depth + 1))
    return " ".join(x for x in parts if x)


STATES = {pyatspi.STATE_FOCUSED: "focused", pyatspi.STATE_SELECTED: "selected", pyatspi.STATE_EXPANDED: "expanded",
          pyatspi.STATE_EXPANDABLE: "expandable", pyatspi.STATE_FOCUSABLE: "focusable"}
KEEP = ("level", "posinset", "setsize", "rowindex", "colindex", "rowcount", "colcount", "sort")


def info(o):
    try:
        return _info(o)
    except Exception as e:  # the object was destroyed (a row re-rendered or scrolled away)
        return {"role": None, "role_name": "<gone: %s>" % e, "name": "", "text": "", "states": [], "attrs": {}}


def _info(o):
    st = o.getState()
    return {
        "role": int(o.getRole()),
        "role_name": o.getRoleName(),
        "name": o.name,
        "text": deep_text(o),
        "states": [n for s, n in STATES.items() if st.contains(s)],
        "attrs": {k: v for k, v in attrs(o).items() if k in KEEP},
    }


def find(o, pred, depth=0):
    if o is None or depth > 60:
        return None
    try:
        if pred(o):
            return o
        for i in range(o.childCount):
            r = find(o.getChildAtIndex(i), pred, depth + 1)
            if r is not None:
                return r
    except Exception:
        return None
    return None


def app():
    d = pyatspi.Registry.getDesktop(0)
    for i in range(d.childCount):
        x = d.getChildAtIndex(i)
        if x is not None and a.app in (x.name or "").lower():
            return x
    cannot("no accessible application like %r" % a.app)


def grid():
    g = find(app(), lambda o: o.getRole() == pyatspi.ROLE_TREE_TABLE)
    if g is None:
        cannot("no tree table: open the Size tree tab after a scan")
    return g


def focused():
    """The deepest object in the focused state (the web view widget also carries it)."""
    best = [None, -1]

    def walk(o, depth):
        if o is None or depth > 60:
            return
        try:
            if o.getState().contains(pyatspi.STATE_FOCUSED) and depth > best[1]:
                best[:] = [o, depth]
            for i in range(o.childCount):
                walk(o.getChildAtIndex(i), depth + 1)
        except Exception:
            pass

    walk(app(), 0)
    return best[0]


def inside(o, g):
    try:
        while o is not None:
            if o == g:
                return True
            o = o.parent
    except Exception:
        return False
    return False


CELL_ROLES = (pyatspi.ROLE_TABLE_CELL, pyatspi.ROLE_COLUMN_HEADER, pyatspi.ROLE_ROW_HEADER)
# What each ARIA role of the treegrid must arrive as over AT-SPI (Core-AAM 1.2).
EXPECTED_ROLE = {
    "treegrid": pyatspi.ROLE_TREE_TABLE,
    "row": pyatspi.ROLE_TABLE_ROW,
    "gridcell": pyatspi.ROLE_TABLE_CELL,
    "columnheader": pyatspi.ROLE_COLUMN_HEADER,
}


def grid_rows(g):
    """The treegrid's rows in order: objects whose children are cells (WebKitGTK may flatten rowgroups)."""
    rows = []

    def walk(o, depth):
        if o is None or depth > 3:
            return
        kids = [o.getChildAtIndex(i) for i in range(o.childCount)]
        if kids and any(k is not None and k.getRole() in CELL_ROLES for k in kids):
            rows.append(o)
            return
        for k in kids:
            walk(k, depth + 1)

    for i in range(g.childCount):
        walk(g.getChildAtIndex(i), 0)
    return rows


def report():
    g = grid()
    out = {"treegrid": info(g)}
    try:
        t = g.queryTable()
        heads = []
        for c in range(t.nColumns):
            h = t.getColumnHeader(c)
            heads.append({"name": h.name, "text": deep_text(h)} if h is not None else None)
        cells = []
        for r in range(min(t.nRows, a.rows)):
            cells.append([deep_text(t.getAccessibleAt(r, c)) if t.getAccessibleAt(r, c) else None for c in range(t.nColumns)])
        out["table"] = {"nRows": t.nRows, "nColumns": t.nColumns, "column_headers": heads, "cells": cells}
    except NotImplementedError:
        out["table"] = "no Table interface"
    rows = grid_rows(g)
    out["exposed_rows"] = len(rows)
    out["rows"] = [dict(info(r), cells=[info(r.getChildAtIndex(c)) for c in range(r.childCount)]) for r in rows[: a.rows + 1]]
    f = focused()
    out["focused"] = info(f) if f is not None else None
    print(json.dumps(out, indent=1, ensure_ascii=False))


def reported():
    """The object a screen reader was last told about: the latest focus or active-descendant event."""
    return EVENTS[-1] if EVENTS else ("none", None)


def row_index(o):
    try:
        while o is not None:
            r = attrs(o).get("rowindex")
            if r is not None:
                return r
            o = o.parent
    except Exception:
        return None
    return None


def cell_text(row, col):
    c = row.getChildAtIndex(col) if row is not None and row.childCount > col else None
    return deep_text(c) if c is not None else ""


def open_path(g, names):
    """Click each folder's row in turn and press Right to open it (the start of the wheel test)."""
    for n in names:
        row = next((r for r in grid_rows(g) if cell_text(r, 0).lstrip("▸▾@ ").strip() == n), None)
        if row is None:
            print("RESULT: FAIL")
            print(f"  FAIL: no rendered row named {n!r}")
            sys.exit(1)
        x, y, w, h = row.getChildAtIndex(0).queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        xdo("mousemove", str(x + 20), str(y + h // 2))
        xdo("click", "1")
        xdo("key", "Right")
        pump(1.0)


def check():
    fails = []
    g = grid()
    # Start with a real click on the first data row (grabFocus does not move focus inside the web view).
    first = grid_rows(g)[1]
    x, y, w, h = first.getChildAtIndex(0).queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    xdo("mousemove", str(x + 20), str(y + h // 2))
    EVENTS.clear()
    xdo("click", "1")
    pump(1.0)
    print("CLICKED FIRST ROW, reader told:", [(k, info(o)["role_name"], row_index(o)) for k, o in EVENTS])

    def walk_tab(key):
        seq = []
        for _ in range(10):
            EVENTS.clear()
            xdo("key", key)
            kind, f = reported()
            if f is None or not inside(f, grid()):
                seq.append({"left_grid_to": info(f) if f is not None else None, "event": kind})
                break
            seq.append(dict(info(f), event=kind, rowindex=row_index(f)))
        return seq

    forward = walk_tab("Tab")
    # Back to the row: click it again, then walk backwards.
    xdo("click", "1")
    backward = walk_tab("shift+Tab")
    print("TAB FROM THE ROW:")
    for o in forward:
        print("  ", json.dumps(o, ensure_ascii=False))
    print("SHIFT+TAB FROM THE ROW:")
    for o in backward:
        print("  ", json.dumps(o, ensure_ascii=False))
    if any(o.get("rowindex") == "1" for o in forward):
        fails.append("a column header comes after the rows in Tab order")
    headers = [o for o in forward + backward if o.get("rowindex") == "1"]
    if len(headers) < 6 or any(not (o["name"] or o["text"]) for o in headers):
        fails.append("not every column header is reachable by Tab and named: %r" % [o["name"] or o["text"] for o in headers])
    xdo("click", "1")
    g = grid()

    def step(label, *keys, expect=None, moves=True):
        """Press keys; judge the object the reader was told about (or, when focus stayed put and
        no event came, the polled focus). `expect` is the rowindex it must be on."""
        EVENTS.clear()
        for k in keys:
            xdo("key", k)
        kind, f = reported()
        # Polled too: which row is selected, and what is focused, whatever the events said.
        sel_rows = [row_index(r) for r in grid_rows(grid()) if r.getState().contains(pyatspi.STATE_SELECTED)]
        pf = focused()
        if f is None and not moves:
            kind, f = "polled", pf
        i = dict(info(f), event=kind) if f is not None else None
        print(label.ljust(22), json.dumps(i, ensure_ascii=False), "| selected rows", sel_rows,
              "| polled focus", pf and (info(pf)["role_name"], row_index(pf)))
        if expect is not None and row_index(f) != expect:
            fails.append(f"{label}: on row {row_index(f)}, expected {expect}")
        if f is None:
            fails.append(f"{label}: no focus or active-descendant event reached the reader")
        elif not inside(f, g):
            fails.append(f"{label}: focus left the treegrid ({i['role_name']} {i['name']!r})")
        elif f == g:
            fails.append(f"{label}: the reader was told about the treegrid container, not a row")
        else:
            if f.getRole() == pyatspi.ROLE_TABLE_CELL:
                missing = [k for k in ("rowindex", "colindex") if k not in i["attrs"]]
            else:
                missing = [k for k in ("rowindex", "level", "posinset", "setsize") if k not in i["attrs"]]
            if missing:
                fails.append(f"{label}: focused object lacks {missing}")
            # The role number must be the one the page's role maps to (Core-AAM). WebKitGTK 2.49
            # to 2.50.4 sends roles 73 (Paragraph) to 115 one too low (WebKit bug 300090, measured)
            # and, by its role list, those from 116 on one too high: a row (90) then reaches the
            # reader as "input method window" (89). Judged against computed-role, not
            # against hard-coded numbers, so it holds on any libatspi. --expect-shift moves the
            # expected number (the CI control); nothing else in the probe uses it.
            computed = attrs(f).get("computed-role")
            want = EXPECTED_ROLE.get(computed)
            if want is not None and int(f.getRole()) != int(want) + a.expect_shift:
                fails.append(f"{label}: a {computed} {ROLE_FINDING} {i['role_name']!r} ({i['role']}),"
                             f" expected {int(want) + a.expect_shift}")
            if not (i["name"] or i["text"]):
                fails.append(f"{label}: focused object has no name or text")
        return i

    # Rows: 2 is "/" (open at start), 3 is its first child.
    step("Down", "Down", expect="3")
    step("Up", "Up", expect="2")
    step("Left (close /)", "Left", expect="2", moves=False)
    step("Right (open /)", "Right", expect="2", moves=False)
    step("Right (to cell 1)", "Right", expect="2")
    step("Right (cell 2)", "Right", expect="2")
    step("Left (cell 1)", "Left", expect="2")
    step("Left (back to row)", "Left", expect="2")
    step("Down (child)", "Down", expect="3")
    step("Right (open child)", "Right", expect="3", moves=False)
    step("Down (grandchild)", "Down", expect="4")
    step("Left (to parent)", "Left", expect="3")
    step("Left (close child)", "Left", expect="3", moves=False)
    step("End", "End")
    step("Home", "Home", expect="2")
    step("Ctrl+End", "ctrl+End")
    step("Ctrl+Home", "ctrl+Home", expect="2")
    step("PageDown", "Next")
    step("PageUp", "Prior", expect="2")
    # A folder with 5,000 children: open it, then wheel its row far out of the rendered window.
    g = grid()
    open_path(g, ["/", "home", "tester", "tree", "wide"])
    g = grid()
    sel = step("wide selected", "Up", "Down")
    x, y, w, h = g.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    xdo("mousemove", str(x + w // 2), str(y + h // 2))
    EVENTS.clear()
    for _ in range(6):
        xdo("click", "--repeat", "40", "--delay", "5", "5")
    pump(1.5)
    want = (sel or {}).get("attrs", {}).get("rowindex")
    polled = focused()
    print("wheeled away: events", [(k, info(o)["role_name"], row_index(o)) for k, o in EVENTS],
          "polled", json.dumps(info(polled) if polled else None, ensure_ascii=False))
    if polled is None or not inside(polled, g) or polled == g or row_index(polled) != want:
        fails.append("wheeled away: focus is no longer on the selected row (%s)" % (polled and info(polled)["role_name"]))
    if any(o is not None and not inside(o, g) for _, o in EVENTS):
        fails.append("wheeled away: the reader was told focus moved out of the treegrid")
    EVENTS.clear()
    for _ in range(6):
        xdo("click", "--repeat", "40", "--delay", "5", "4")
    pump(1.5)
    polled = focused()
    print("wheeled back: events", [(k, info(o)["role_name"], row_index(o)) for k, o in EVENTS],
          "polled", json.dumps(info(polled) if polled else None, ensure_ascii=False))
    if polled is None or row_index(polled) != want:
        fails.append("wheeled back: focus is not on the selected row")
    after = step("Down after wheel", "Down")
    if want and (not after or after["attrs"].get("rowindex") != str(int(want) + 1)):
        fails.append("Down after the wheel did not move to the next row")
    roles = [f for f in fails if ROLE_FINDING in f]
    shifted = f" (expected roles shifted by {a.expect_shift})" if a.expect_shift else ""
    print("RESULT:", "PASS" if not fails else "FAIL")
    print(f"SUMMARY: {len(fails)} findings, {len(roles)} of them wrong roles{shifted}")
    for f in fails:
        print("  FAIL:", f)
    if not fails:
        sys.exit(0)
    sys.exit(WRONG_ONLY if len(roles) == len(fails) else 1)


report() if a.mode == "report" else check()
