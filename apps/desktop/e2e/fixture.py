#!/usr/bin/env python3
"""Build the end-to-end fixture tree (PR gate F1). Deterministic: names and sizes come from
seed 16452. Every name is made up.

usage: fixture.py ROOT [--wide N] [--many M]

ROOT/
  proj-web/            a git repository, committed, then one tracked file touched so the
                       index is stat-dirty (a `git status` that may write would refresh it)
    node_modules/      regenerable: a few packages
    dist/              build output
  proj-old/            a git repository whose last change is a year old, with a target/ folder
  wide/                N files in one folder (default 5,000), for scrolling
  loose/               loose files of several sizes
  many/appNNN/         M small projects with node_modules (default 150), for paging
"""

from __future__ import annotations

import os
import random
import subprocess
import sys
import time
from pathlib import Path

SEED = 16452
YEAR = 365 * 86400


def write(p: Path, size: int, rng: random.Random) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(rng.randbytes(size))


def git(repo: Path, *args: str, when: int | None = None) -> None:
    env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
    if when is not None:
        env["GIT_AUTHOR_DATE"] = env["GIT_COMMITTER_DATE"] = f"@{when} +0000"
    subprocess.run(["git", "-C", str(repo), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", *args], check=True, env=env, stdout=subprocess.DEVNULL)


def age(root: Path, seconds: int) -> None:
    t = time.time() - seconds
    for dirpath, dirnames, filenames in os.walk(root):
        for n in filenames + dirnames:
            os.utime(Path(dirpath) / n, (t, t), follow_symlinks=False)
    os.utime(root, (t, t))


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    root = Path(argv[0])
    wide = int(argv[argv.index("--wide") + 1]) if "--wide" in argv else 5000
    many = int(argv[argv.index("--many") + 1]) if "--many" in argv else 150
    rng = random.Random(SEED)
    root.mkdir(parents=True, exist_ok=True)

    web = root / "proj-web"
    write(web / "src" / "index.js", 2048, rng)
    write(web / "package.json", 300, rng)
    (web / ".gitignore").parent.mkdir(parents=True, exist_ok=True)
    (web / ".gitignore").write_text("node_modules/\ndist/\n")
    git(web, "init", "-q", "-b", "main")
    git(web, "add", "-A")
    git(web, "commit", "-q", "-m", "fixture")
    for pkg in ("left-pad", "tiny-lib", "big-lib"):
        for i in range(20):
            write(web / "node_modules" / pkg / f"file{i}.js", rng.randrange(1024, 65536), rng)
    for i in range(10):
        write(web / "dist" / f"bundle{i}.js", rng.randrange(4096, 131072), rng)
    # Stat-dirty: same content, newer mtime than the index records.
    idx = web / "src" / "index.js"
    t = time.time() + 5
    os.utime(idx, (t, t))

    old = root / "proj-old"
    write(old / "src" / "main.rs", 4096, rng)
    write(old / "Cargo.toml", 200, rng)
    (old / ".gitignore").write_text("target/\n")
    then = int(time.time()) - YEAR
    git(old, "init", "-q", "-b", "main")
    git(old, "add", "-A")
    git(old, "commit", "-q", "-m", "old fixture", when=then)
    for i in range(30):
        write(old / "target" / "debug" / f"obj{i}.o", rng.randrange(8192, 262144), rng)
    age(old, YEAR)

    for i in range(wide):
        write(root / "wide" / f"w{i:05}.bin", rng.randrange(0, 8192), rng)
    for i, size in enumerate([0, 1, 4095, 4096, 1 << 20, 3 << 20]):
        write(root / "loose" / f"loose{i}.dat", size, rng)
    # Enough small projects that one developer-view class has more than one page (100 items).
    for i in range(many):
        app = root / "many" / f"app{i:03}"
        write(app / "package.json", 120, rng)
        write(app / "node_modules" / "dep" / "index.js", rng.randrange(1024, 8192), rng)
    count = sum(len(f) + len(d) for _, d, f in os.walk(root))
    print(f"fixture: {count} entries under {root} (seed {SEED}, wide {wide}, many {many})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
