#!/usr/bin/env python3
"""Bring the app to the state treegrid_atspi.py checks: a scan of `/` shown on the Size tree tab.

Through AT-SPI, as a screen reader user would: ticks the "/" volume on the start screen, presses
Scan, waits for the scan to finish (the "Size tree" tab appears), opens that tab and waits until
the tree table has rows. Exit 0 when the tree is shown, 1 (with the accessible tree's top level on
stderr) when a step times out.

usage: treegrid_setup.py [--app prunedisk] [--timeout S]
"""
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

R = Atspi.Role
APP = sys.argv[sys.argv.index("--app") + 1] if "--app" in sys.argv else "prunedisk"
TIMEOUT = float(sys.argv[sys.argv.index("--timeout") + 1]) if "--timeout" in sys.argv else 300


def app():
    d = Atspi.get_desktop(0)
    for i in range(d.get_child_count()):
        a = d.get_child_at_index(i)
        if a is not None and APP in (a.get_name() or ""):
            return a
    return None


def walk(o, depth=0):
    if o is None or depth > 60:
        return
    yield o
    try:
        n = o.get_child_count()
    except Exception:
        return
    for i in range(n):
        try:
            yield from walk(o.get_child_at_index(i), depth + 1)
        except Exception:
            continue


def find(pred):
    a = app()
    return next((o for o in walk(a) if pred(o)), None) if a is not None else None


def wait_for(what, pred, timeout):
    end = time.time() + timeout
    while time.time() < end:
        o = find(pred)
        if o is not None:
            return o
        time.sleep(1)
    print(f"treegrid setup: FAIL: no {what} after {timeout:.0f} s", file=sys.stderr)
    a = app()
    for o in (walk(a) if a is not None else []):
        try:
            print(f"  {o.get_role_name()!r} {(o.get_name() or '')[:70]!r}", file=sys.stderr)
        except Exception:
            pass
    sys.exit(1)


def press(o, what):
    try:
        o.do_action(0)
    except Exception as e:
        print(f"treegrid setup: FAIL: could not press {what}: {e}", file=sys.stderr)
        sys.exit(1)
    print(f"treegrid setup: pressed {what}")


def role_name(o):
    try:
        return o.get_role(), o.get_name() or ""
    except Exception:
        return None, ""


root = wait_for("check box for the / volume",
                lambda o: role_name(o)[0] == R.CHECK_BOX and role_name(o)[1].startswith("/ ("), 90)
if not root.get_state_set().contains(Atspi.StateType.CHECKED):
    press(root, "the / volume")
scan = wait_for("enabled Scan button", lambda o: role_name(o) in ((R.PUSH_BUTTON, "Scan"), (getattr(R, "BUTTON", R.PUSH_BUTTON), "Scan"))
                and o.get_state_set().contains(Atspi.StateType.ENABLED), 30)
press(scan, "Scan")
tab = wait_for("Size tree tab (scan finished)", lambda o: role_name(o) == (R.PAGE_TAB, "Size tree"), TIMEOUT)
press(tab, "the Size tree tab")


def computed_role(o):
    try:
        return (o.get_attributes() or {}).get("computed-role", "")
    except Exception:
        return ""


def tree_with_rows(o):
    # Rows by WebKit's computed-role, not by role number: judging the numbers is the probe's job,
    # and a WebKitGTK that numbers them wrong must reach the probe and fail there.
    if role_name(o)[0] != R.TREE_TABLE:
        return False
    return sum(1 for x in walk(o) if computed_role(x) == "row") >= 3


wait_for("tree table with rows", tree_with_rows, 60)
print("treegrid setup: the Size tree shows the scan")
