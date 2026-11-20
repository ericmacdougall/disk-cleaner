#!/usr/bin/env python3
"""Summarise file writes in an strace log of the app and its child processes (Linux, PR gate
F1). Report only: the manifest comparison is the gate; this lists what the trace saw so a
write outside the snapshot roots is visible too.

usage: trace_report.py LOG [--declared PATH]... [--ignore PREFIX]...

A write is a successful open with O_WRONLY, O_RDWR, O_CREAT or O_TRUNC, or a successful
creat, mkdir, rename, link, symlink, unlink, rmdir, truncate, chmod, chown or utimensat.
Each path is printed once, as "declared" (under a declared path), "ignored" (a prefix such
as /dev or /proc) or "OTHER". Relative paths are printed as they appear.
"""

from __future__ import annotations

import re
import sys
from collections import defaultdict

WRITE_FLAGS = ("O_WRONLY", "O_RDWR", "O_CREAT", "O_TRUNC")
CALLS = {
    "creat", "mkdir", "mkdirat", "rename", "renameat", "renameat2", "link", "linkat",
    "symlink", "symlinkat", "unlink", "unlinkat", "rmdir", "truncate", "chmod", "fchmodat",
    "chown", "lchown", "fchownat", "utimensat", "utimes", "utime",
}
LINE = re.compile(r'^(?:\[pid\s+\d+\]\s+|\d+\s+)?(\w+)\((.*)\)\s+=\s+(-?\d+)')
STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    declared: list[str] = []
    ignore: list[str] = []
    rest = argv[1:]
    while rest:
        if len(rest) < 2 or rest[0] not in ("--declared", "--ignore"):
            print(__doc__, file=sys.stderr)
            return 2
        (declared if rest[0] == "--declared" else ignore).append(rest[1])
        rest = rest[2:]
    seen: dict[str, set[str]] = defaultdict(set)
    with open(argv[0], errors="replace") as f:
        for raw in f:
            m = LINE.match(raw.strip())
            if not m or int(m.group(3)) < 0:
                continue
            call, body = m.group(1), m.group(2)
            if call in ("open", "openat", "openat2"):
                if not any(flag in body for flag in WRITE_FLAGS):
                    continue
            elif call not in CALLS:
                continue
            for p in STRING.findall(body):
                seen[p].add(call)
    groups: dict[str, list[str]] = {"declared": [], "ignored": [], "OTHER": []}
    for p in sorted(seen):
        kind = (
            "declared" if any(p == d or p.startswith(d.rstrip("/") + "/") for d in declared)
            else "ignored" if any(p.startswith(i) for i in ignore)
            else "OTHER"
        )
        groups[kind].append(f"{kind} {','.join(sorted(seen[p]))}: {p}")
    for kind in ("OTHER", "declared", "ignored"):
        for line in groups[kind]:
            print(line)
    print(f"trace: {len(seen)} paths written; {len(groups['declared'])} declared, "
          f"{len(groups['ignored'])} ignored, {len(groups['OTHER'])} other")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
