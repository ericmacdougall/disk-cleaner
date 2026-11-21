#!/usr/bin/env python3
"""Do screen readers get the right role for each element of the app's page? (AT-SPI)

The webview sends each element's role to the accessibility bus as a number from AT-SPI's role
list (AtspiRole); the screen reader turns it back into a name with the host's libatspi. A webview
that numbers the list differently from AT-SPI shifts every role after the difference: WebKitGTK
2.50.0 to 2.50.4 left Footer out of its copy of the list (WebKit bug 300090), so a heading
arrives as "document frame", a paragraph as "footer", a link as "form".

WebKit also gives each object its ARIA role as a string ("computed-role": heading, paragraph,
link, button, checkbox ...). This compares the number each object reports with the number the
host's libatspi has for that role's AT-SPI role, over the whole tree of the app's window.

usage: roles_atspi.py check APP [--timeout S]   exit 0 when every checked object has its role,
                                                1 when any is wrong or the page has none checked
       roles_atspi.py dump APP [--timeout S]    print number, role name, tag, name, attributes

The check also needs at least one heading, one button and one check box with the right number,
so a page that never loaded, or a tree read through a different bus, cannot pass.
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
REQUIRED = {"heading": R.HEADING, "button": BUTTON, "checkbox": R.CHECK_BOX}


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


def main(argv):
    if len(argv) < 3 or argv[1] not in ("check", "dump"):
        print(__doc__, file=sys.stderr)
        return 2
    mode, app_name = argv[1], argv[2]
    timeout = float(argv[argv.index("--timeout") + 1]) if "--timeout" in argv else 60
    end = time.time() + timeout
    app = None
    while time.time() < end:
        app = app_named(app_name)
        if app is not None and loaded(app):
            break
        time.sleep(1)
    if app is None:
        print(f"roles: FAIL: no accessible application named like {app_name!r}")
        return 1
    if mode == "dump":
        for o in walk(app):
            num, rname, name, attrs = info(o)
            print(f"{num:4d} {rname!r:28} tag={attrs.get('tag', '')!r:10} {name[:50]!r} {attrs}")
        return 0
    wrong, checked = [], 0
    seen = {k: 0 for k in REQUIRED}
    for o in walk(app):
        num, rname, name, attrs = info(o)
        aria = attrs.get("computed-role", "")
        want = EXPECTED.get(aria)
        if want is None:
            continue
        checked += 1
        if num != int(want):
            wrong.append(f"<{attrs.get('tag', '?')}> {aria} {name[:50]!r}: reported {num} ({rname}), "
                         f"must be {int(want)} ({Atspi.role_get_name(want)})")
        elif aria in seen:
            seen[aria] += 1
    print(f"roles: {checked} objects checked, {len(wrong)} wrong; with the right role: "
          + ", ".join(f"{k} {v}" for k, v in seen.items()))
    for w in wrong:
        print("roles: WRONG " + w)
    missing = [k for k, v in seen.items() if v == 0]
    if missing:
        print("roles: FAIL: no " + ", ".join(missing) + " with its right role on the page")
    if wrong or missing or checked == 0:
        return 1
    print("roles: every checked element has its role")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
