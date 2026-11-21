#!/usr/bin/env python3
"""Do screen readers get the right role for each element of the app's page? (AT-SPI)

The webview sends each element's role to the accessibility bus as a number from AT-SPI's role
list (AtspiRole); the screen reader turns it back into a name with the host's libatspi. A webview
that numbers the list differently from AT-SPI shifts the roles after the difference: WebKitGTK
2.49 to 2.50.4 left Footer out of its copy of the list (WebKit bug 300090), so a heading arrives
as "document frame", a paragraph as "footer", a link as "form".

WebKit also gives each object its ARIA role as a string ("computed-role": heading, paragraph,
link, button, checkbox ...). This compares the number each object reports with the number the
host's libatspi has for that role's AT-SPI role, over the whole tree of the app's window.

usage: roles_atspi.py check APP [--timeout S] [--expect-shift N]
       roles_atspi.py dump APP [--timeout S]    print number, role name, tag, name, attributes

Exit codes of check:
  0  every checked object has its role, and a heading, a button and a check box were among them
  4  wrong roles: at least one object reported a number other than the expected one; each is
     printed as a "roles: WRONG <tag> <computed-role> ...: reported R (...), must be E (...)" line
  3  could not check: no such application on the bus, the page never showed a heading, or no
     object with a known computed-role (nothing was judged, so nothing passed or failed)
  2  bad usage
Any other code (a Python traceback exits 1) is a crash of the probe.

--expect-shift N adds N to every expected role number before comparing, for the standing control
in CI: on a build whose roles are right, a shift of 1 must give exit 4 with a WRONG line for every
checked object (a probe that cannot see a wrong number would pass it instead).
"""
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

R = Atspi.Role
BUTTON = R.PUSH_BUTTON if hasattr(R, "PUSH_BUTTON") else R.BUTTON
# ARIA computed role -> AT-SPI role, as WebKit maps them (AccessibilityObjectAtspi.cpp, atspiRole).
# Only roles whose mapping is the same on every WebKitGTK with AT-SPI support (2.38 and later).
EXPECTED = {
    "heading": R.HEADING,
    "paragraph": R.PARAGRAPH,
    "link": R.LINK,
    "button": BUTTON,
    "checkbox": R.CHECK_BOX,
    "list": R.LIST,
    "listitem": R.LIST_ITEM,
    "group": R.PANEL,
    "status": R.STATUS_BAR,
    "article": R.ARTICLE,
    "textbox": R.ENTRY,
    "radio": R.RADIO_BUTTON,
}
REQUIRED = ("heading", "button", "checkbox")
WRONG, CANNOT, USAGE = 4, 3, 2


def app_named(name):
    d = Atspi.get_desktop(0)
    for i in range(d.get_child_count()):
        a = d.get_child_at_index(i)
        if a is not None and name in (a.get_name() or ""):
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


def info(o):
    try:
        attrs = o.get_attributes() or {}
    except Exception:
        attrs = {}
    return int(o.get_role()), o.get_role_name(), (o.get_name() or ""), dict(attrs)


def loaded(app):
    """The webview has filled the tree: some object carries WebKit's computed-role heading."""
    return any(info(o)[3].get("computed-role") == "heading" for o in walk(app))


def role_label(n):
    try:
        return Atspi.role_get_name(Atspi.Role(n))
    except Exception:
        return "not an AT-SPI role"


def option(argv, flag, default, kind):
    if flag not in argv:
        return default
    try:
        return kind(argv[argv.index(flag) + 1])
    except (IndexError, ValueError):
        print(f"roles: bad value for {flag}", file=sys.stderr)
        sys.exit(USAGE)


def main(argv):
    if len(argv) < 3 or argv[1] not in ("check", "dump"):
        print(__doc__, file=sys.stderr)
        return USAGE
    mode, app_name = argv[1], argv[2]
    timeout = option(argv, "--timeout", 60.0, float)
    shift = option(argv, "--expect-shift", 0, int)
    end = time.time() + timeout
    app = ready = None
    while time.time() < end:
        app = app_named(app_name)
        if app is not None and loaded(app):
            ready = app
            break
        time.sleep(1)
    if ready is None:
        why = "no accessible application" if app is None else "no heading on the page of the application"
        print(f"roles: CANNOT CHECK: {why} named like {app_name!r} after {timeout:.0f} s")
        return CANNOT
    if mode == "dump":
        for o in walk(ready):
            num, rname, name, attrs = info(o)
            print(f"{num:4d} {rname!r:28} tag={attrs.get('tag', '')!r:10} {name[:50]!r} {attrs}")
        return 0
    wrong, checked = [], 0
    right = {k: 0 for k in REQUIRED}
    for o in walk(ready):
        num, rname, name, attrs = info(o)
        aria = attrs.get("computed-role", "")
        if aria not in EXPECTED:
            continue
        want = int(EXPECTED[aria]) + shift
        checked += 1
        if num != want:
            wrong.append(f"<{attrs.get('tag', '?')}> {aria} {name[:50]!r}: reported {num} ({rname}), "
                         f"must be {want} ({role_label(want)})")
        elif aria in right:
            right[aria] += 1
    shifted = f" (expected roles shifted by {shift})" if shift else ""
    print(f"roles: {checked} objects checked{shifted}, {len(wrong)} wrong; with the right role: "
          + ", ".join(f"{k} {v}" for k, v in right.items()))
    for w in wrong:
        print("roles: WRONG " + w)
    if checked == 0:
        print("roles: CANNOT CHECK: no object with a known computed-role")
        return CANNOT
    if wrong:
        return WRONG
    missing = [k for k, v in right.items() if v == 0]
    if missing:
        print("roles: CANNOT CHECK: no " + ", ".join(missing) + " on the page")
        return CANNOT
    print("roles: every checked element has its role")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
