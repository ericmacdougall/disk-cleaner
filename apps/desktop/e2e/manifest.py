#!/usr/bin/env python3
"""No-write proof for the end-to-end checks (PR gate F1).

snapshot OUT ROOT...      record every entry under each ROOT: kind, size, mode, mtime (ns),
                          SHA-256 for files, link target for links, sorted names for folders.
compare BEFORE AFTER [--declared PATH]...
                          print every entry added, removed or changed between two snapshots,
                          except under a declared path (the app's own state files, the
                          export the test asked for). Exit 1 when anything else changed.

Access times are not recorded (reading changes them). Links are not followed.
"""

from __future__ import annotations

import hashlib
import json
import os
import stat
import sys
from pathlib import Path


def entry(p: Path) -> dict:
    st = p.lstat()
    e = {"mode": stat.S_IMODE(st.st_mode), "mtime_ns": st.st_mtime_ns}
    if stat.S_ISLNK(st.st_mode):
        e["kind"] = "link"
        e["target"] = os.readlink(p)
    elif stat.S_ISDIR(st.st_mode):
        e["kind"] = "dir"
        try:
            e["names"] = sorted(os.listdir(p))
        except OSError as err:
            e["names"] = f"unlistable: {err.strerror}"
    elif stat.S_ISREG(st.st_mode):
        e["kind"] = "file"
        e["size"] = st.st_size
        h = hashlib.sha256()
        try:
            with open(p, "rb") as f:
                for block in iter(lambda: f.read(1 << 20), b""):
                    h.update(block)
            e["sha256"] = h.hexdigest()
        except OSError as err:
            e["sha256"] = f"unreadable: {err.strerror}"
    else:
        e["kind"] = "other"
    return e


def walk(root: Path) -> dict:
    out = {}
    if not root.exists() and not root.is_symlink():
        return out
    out[str(root)] = entry(root)
    if root.is_dir() and not root.is_symlink():
        for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
            for n in dirnames + filenames:
                p = Path(dirpath) / n
                try:
                    out[str(p)] = entry(p)
                except OSError as err:
                    out[str(p)] = {"kind": "error", "error": err.strerror}
    return out


def inside(p: Path, declared: list[str]) -> bool:
    """`p` is a declared path or inside one."""
    return any(p == Path(d) or Path(d) in p.parents for d in declared)


def on_the_way(p: Path, declared: list[str]) -> bool:
    """`p` is a folder holding a declared path."""
    return any(p in Path(d).parents for d in declared)


def declared_change(path: str, a: dict | None, b: dict | None, declared: list[str]) -> bool:
    """A change the app may make: anything inside a declared path, and, on the way to one, a
    folder created or removed, or a folder whose listing gained or lost only entries that are
    themselves declared or on the way (creating a declared file creates its folders)."""
    p = Path(path)
    if inside(p, declared):
        return True
    if not on_the_way(p, declared):
        return False
    if a is None or b is None:
        return (a or b).get("kind") == "dir"
    if a.get("kind") != "dir" or b.get("kind") != "dir":
        return False
    rest = lambda e: {k: v for k, v in e.items() if k not in ("names", "mtime_ns")}  # noqa: E731
    na, nb = a.get("names"), b.get("names")
    if rest(a) != rest(b) or not isinstance(na, list) or not isinstance(nb, list):
        return False
    return all(inside(p / n, declared) or on_the_way(p / n, declared) for n in set(na) ^ set(nb))


def main(argv: list[str]) -> int:
    if len(argv) >= 3 and argv[0] == "snapshot":
        snap = {}
        for r in argv[2:]:
            snap.update(walk(Path(r)))
        Path(argv[1]).write_text(json.dumps({"roots": argv[2:], "entries": snap}, indent=1))
        print(f"snapshot: {len(snap)} entries under {len(argv) - 2} roots")
        return 0
    if len(argv) >= 3 and argv[0] == "compare":
        before = json.loads(Path(argv[1]).read_text())["entries"]
        after = json.loads(Path(argv[2]).read_text())["entries"]
        declared: list[str] = []
        rest = argv[3:]
        while rest:
            if rest[0] != "--declared" or len(rest) < 2:
                print(__doc__, file=sys.stderr)
                return 2
            declared.append(rest[1])
            rest = rest[2:]
        changes, allowed = [], []
        for p in sorted(set(before) | set(after)):
            a, b = before.get(p), after.get(p)
            if a == b:
                continue
            what = "added" if a is None else "removed" if b is None else "changed"
            detail = "" if what != "changed" else " " + ",".join(k for k in sorted(set(a) | set(b)) if a.get(k) != b.get(k))
            line = f"{what}{detail}: {p}"
            (allowed if declared_change(p, a, b, declared) else changes).append(line)
        for line in allowed:
            print(f"declared {line}")
        for line in changes:
            print(f"UNDECLARED {line}")
        print(f"manifest: {len(before)} entries before, {len(after)} after; {len(allowed)} declared changes, {len(changes)} undeclared")
        return 1 if changes else 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
