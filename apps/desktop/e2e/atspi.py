#!/usr/bin/env python3
"""Drive the running app through its accessibility tree (AT-SPI), as e2e/appimage-dialog.sh does.

usage: atspi.py press APP BUTTON TIMEOUT   press the push button named BUTTON
       atspi.py find APP TEXT TIMEOUT      wait for any object whose name contains TEXT
       atspi.py dump APP                   print the tree (role and name per line)

APP is matched against the accessible application names (a substring). Exit 0 on success, 3 when
nothing matched within TIMEOUT seconds (the tree is printed to stderr then), 2 on bad usage.
"""
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

BUTTON_ROLES = ("push button", "button")  # the role's name differs between AT-SPI versions


def apps(name):
    d = Atspi.get_desktop(0)
    for i in range(d.get_child_count()):
        a = d.get_child_at_index(i)
        if a is not None and name in (a.get_name() or ""):
            yield a


def walk(o, depth=0):
    if o is None or depth > 60:
        return
    yield depth, o
    try:
        n = o.get_child_count()
    except Exception:  # an object can vanish while the tree is walked
        return
    for i in range(n):
        try:
            c = o.get_child_at_index(i)
        except Exception:
            continue
        yield from walk(c, depth + 1)


def first(app, test):
    for a in apps(app):
        for _, o in walk(a):
            try:
                if test(o):
                    return o
            except Exception:
                continue
    return None


def dump(app, out):
    for a in apps(app):
        for depth, o in walk(a):
            try:
                print("  " * depth + f"{o.get_role_name()} {o.get_name()!r}", file=out)
            except Exception as e:
                print("  " * depth + f"<gone: {e}>", file=out)


def main(argv):
    if len(argv) == 3 and argv[1] == "dump":
        dump(argv[2], sys.stdout)
        return 0
    if len(argv) != 5 or argv[1] not in ("press", "find"):
        print(__doc__, file=sys.stderr)
        return 2
    cmd, app, text, timeout = argv[1], argv[2], argv[3], float(argv[4])
    if cmd == "press":
        test = lambda o: o.get_role_name() in BUTTON_ROLES and (o.get_name() or "") == text  # noqa: E731
    else:
        test = lambda o: text in (o.get_name() or "")  # noqa: E731
    end = time.time() + timeout
    while time.time() < end:
        o = first(app, test)
        if o is not None:
            if cmd == "press":
                o.do_action(0)
                print(f"atspi: pressed {text!r}")
            else:
                print(f"atspi: found {o.get_role_name()} {o.get_name()!r}")
            return 0
        time.sleep(1)
    dump(app, sys.stderr)
    print(f"atspi: no {'button' if cmd == 'press' else 'object'} {text!r} in {app!r} within {timeout:g} s", file=sys.stderr)
    return 3


if __name__ == "__main__":
    sys.exit(main(sys.argv))
