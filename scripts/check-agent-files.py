#!/usr/bin/env python3
"""Independent check that a disk-cleaner plan removes no agent data.

    check-agent-files.py --home DIR --plan PLAN.json
    check-agent-files.py --self-test

The check knows nothing of the tool's code: it carries its own protected-root list, carve-out
positions and name patterns, walks the home itself and reads the plan only for the paths of its
removing items. It fails (exit 1) when a removing item is equal to, above or below an agent file.
Nothing is filtered by hand: every hit is printed for triage.

Agent files are:
  * files inside a protected root, except files inside a carve-out-position checkout and files
    inside an allowlisted dependency or cache directory that holds no agent-pattern file outside
    installed-package directories;
  * inside a carve-out-position checkout: files whose names match the agent patterns, outside
    installed-package directories and not listed by `git ls-files` of that checkout;
  * Claude or Codex log name shapes anywhere (`rollout-*.jsonl`, a UUID-named `.jsonl`).
Python 3 standard library only.
"""
import json
import os
import re
import subprocess
import sys
import tempfile

# Protected roots (home-relative) and home-level name prefixes. Hard-coded on purpose.
ROOTS = [
    ".claude", ".codex", ".agent-harness", ".prunedisk", ".disk-cleaner", ".kimi",
    ".kimi-code", ".gemini", ".cursor", ".config/claude", ".local/share/claude",
    "Library/Application Support/Claude",
]
PREFIXES = (".claude", ".codex", ".kimi", ".agent-harness", ".prunedisk")
ALLOWLIST = {
    "node_modules", ".venv", "venv", "__pycache__", ".pytest_cache", ".mypy_cache",
    ".ruff_cache", ".tox", ".gradle", ".turbo", ".parcel-cache", "target",
}
REMOVING = {"remove-regenerable", "backup-then-remove", "recover-backup-then-remove"}
UUID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.I)
WAL = re.compile(r"(^|[._-])wal([._-]|$)", re.I)


def agent_name(name):
    n = name.lower()
    return n.endswith(".jsonl") or "receipt" in n or bool(WAL.search(n))


def log_shape(name):
    n = name.lower()
    return (n.startswith("rollout-") and n.endswith(".jsonl")) or (
        n.endswith(".jsonl") and bool(UUID.match(name[: -len(".jsonl")]))
    )


def protected_roots(home):
    roots = {os.path.join(home, r) for r in ROOTS}
    try:
        for e in os.listdir(home):
            if e.startswith(PREFIXES):
                roots.add(os.path.join(home, e))
    except OSError:
        pass
    return sorted(roots)


def under(path, root):
    return path == root or path.startswith(root + os.sep)


def is_checkout(d):
    return os.path.exists(os.path.join(d, ".git"))


def carve_nodes(home, roots):
    """Checkouts in carve-out positions: work/<s>/X (not holding agent-pattern files at its top
    level), work/<s>/<t>/X for a task dir t that is not a checkout, .claude|.codex/worktrees/X."""
    out = []

    def kids(d):
        try:
            return sorted(
                os.path.join(d, e) for e in os.listdir(d) if os.path.isdir(os.path.join(d, e))
            )
        except OSError:
            return []

    def top_agent(d):
        try:
            return any(
                agent_name(e) and os.path.isfile(os.path.join(d, e)) for e in os.listdir(d)
            )
        except OSError:
            return True

    for r in roots:
        base = os.path.basename(r)
        if base == ".agent-harness":
            for s in kids(os.path.join(r, "work")):
                for x in kids(s):
                    if is_checkout(x):
                        if not top_agent(x):
                            out.append(x)
                    else:
                        for y in kids(x):
                            if is_checkout(y):
                                out.append(y)
        elif base in (".claude", ".codex"):
            for x in kids(os.path.join(r, "worktrees")):
                if is_checkout(x):
                    out.append(x)
    return out


def in_package(path, top):
    """Is `path` inside an installed package below `top` (a package.json in a directory between
    them, or a directory directly below site-packages)?"""
    d = os.path.dirname(path)
    while d != top and under(d, top):
        if os.path.exists(os.path.join(d, "package.json")):
            return True
        if os.path.basename(os.path.dirname(d)) == "site-packages":
            return True
        d = os.path.dirname(d)
    return False


def tracked(checkout):
    try:
        r = subprocess.run(
            ["git", "-C", checkout, "ls-files", "-z"], capture_output=True, check=True
        )
        return {
            os.path.join(checkout, p.decode("utf-8", "surrogateescape"))
            for p in r.stdout.split(b"\0")
            if p
        }
    except (OSError, subprocess.CalledProcessError):
        return set()  # unknown: nothing is listed, so more files count as agent files


def agent_files(home):
    home = os.path.realpath(home)
    roots = protected_roots(home)
    carves = carve_nodes(home, roots)
    tracked_cache = {}
    clean_dep = {}  # allowlisted dir -> holds no agent-pattern file outside packages

    def dep_clean(d):
        if d not in clean_dep:
            ok = True
            for dp, _dn, fn in os.walk(d):
                for f in fn:
                    p = os.path.join(dp, f)
                    if agent_name(f) and not in_package(p, d):
                        ok = False
            clean_dep[d] = ok
        return clean_dep[d]

    found = []
    for dp, dn, fn in os.walk(home):
        dn[:] = [d for d in dn if not os.path.islink(os.path.join(dp, d))]
        for f in fn:
            p = os.path.join(dp, f)
            if os.path.islink(p):
                continue
            if log_shape(f):
                found.append(p)
                continue
            if not any(under(p, r) for r in roots):
                continue
            carve = next((c for c in carves if under(p, c)), None)
            if carve is not None:
                if ".git" in os.path.relpath(p, carve).split(os.sep)[:1]:
                    continue
                if not agent_name(f) or in_package(p, carve):
                    continue
                if carve not in tracked_cache:
                    tracked_cache[carve] = tracked(carve)
                if p in tracked_cache[carve]:
                    continue
                found.append(p)
                continue
            # Inside an allowlisted dependency or cache directory that holds no agent file.
            # Only below work/<session>/ of the agent harness: not e.g. ~/.claude/plugins/x.
            wbase = os.path.join(home, ".agent-harness", "work")
            d = dp
            exempt = False
            while under(d, wbase) and d.count(os.sep) > wbase.count(os.sep) + 1:
                if os.path.basename(d) in ALLOWLIST and dep_clean(d):
                    exempt = True
                    break
                d = os.path.dirname(d)
            if not exempt:
                found.append(p)
    return found


def removing_paths(plan):
    return [i["path"] for i in plan.get("items", []) if i.get("action") in REMOVING]


def hits(removals, files):
    fileset = set(files)
    dirs = set()
    for f in files:
        d = os.path.dirname(f)
        while d and d not in dirs and d != os.path.dirname(d):
            dirs.add(d)
            d = os.path.dirname(d)
    out = []
    for r in removals:
        r = os.path.normpath(r)
        bad = r in fileset or r in dirs
        if not bad:
            bad = any(under(r, f) for f in fileset)
        if bad:
            out.append(r)
    return out


def check(home, plan):
    files = agent_files(home)
    removals = [os.path.realpath(p) for p in removing_paths(plan)]
    bad = hits(removals, files)
    return files, removals, bad


def write(path, text="x"):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def git(d, *args):
    subprocess.run(
        ["git", "-C", d, "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
        check=True,
        capture_output=True,
    )


def self_test():
    ok = True
    with tempfile.TemporaryDirectory() as tmp:
        home = os.path.realpath(tmp)
        work = os.path.join(home, ".agent-harness", "work", "s")
        write(os.path.join(home, ".claude", "projects", "p", "x.jsonl"))
        clone = os.path.join(work, "clone")
        os.makedirs(clone)
        git(clone, "init", "-q")
        write(os.path.join(clone, "fixtures", "x.jsonl"))
        write(os.path.join(clone, "src", "a.rs"))
        git(clone, "add", ".")
        git(clone, "commit", "-q", "-m", "init")
        write(os.path.join(work, "task", "notes.md"))
        write(os.path.join(work, "task", "package.json"), "{}")
        write(os.path.join(work, "task", "node_modules", "pkg", "package.json"), "{}")
        write(os.path.join(work, "task", "node_modules", "pkg", "test", "a.jsonl"))
        write(os.path.join(home, "work", "repo", "src", "lib.rs"))

        def run(label, removals, want_fail):
            nonlocal ok
            plan = {"items": [{"path": p, "action": "remove-regenerable"} for p in removals]}
            _files, _rem, bad = check(home, plan)
            good = bool(bad) == want_fail
            ok &= good
            print(("ok   " if good else "FAIL ") + label + (f" (hits: {len(bad)})" if bad else ""))

        # Correct removals pass.
        run("carve-out checkout with only tracked agent-named files", [clone], False)
        run("node_modules directly in a task dir", [os.path.join(work, "task", "node_modules")], False)
        run("unrelated directory outside every root", [os.path.join(home, "work", "repo")], False)
        write(os.path.join(home, ".claude", "plugins", "x", "package.json"), "{}")
        write(os.path.join(home, ".claude", "plugins", "x", "node_modules", "a.js"))
        # Planted bad removals fail.
        run("node_modules inside an agent home's plugins", [os.path.join(home, ".claude", "plugins", "x", "node_modules")], True)
        run("an agent home's project directory", [os.path.join(home, ".claude", "projects", "p")], True)
        run("a task dir with a top-level notes file", [os.path.join(work, "task")], True)
        run("the home itself", [home], True)
        write(os.path.join(clone, "run-receipt.json"))
        run("carve-out checkout with an untracked receipt", [clone], True)
        os.remove(os.path.join(clone, "run-receipt.json"))
        run("carve-out checkout again, clean", [clone], False)
        write(os.path.join(work, "task", "node_modules", "x.jsonl"))
        run("node_modules holding a planted x.jsonl", [os.path.join(work, "task", "node_modules")], True)
    print("self-test " + ("PASSED" if ok else "FAILED"))
    return 0 if ok else 1


def main(argv):
    if "--self-test" in argv:
        return self_test()
    args = dict(zip(argv[::2], argv[1::2]))
    if "--home" not in args or "--plan" not in args:
        print(__doc__)
        return 2
    with open(args["--plan"]) as f:
        plan = json.load(f)
    files, removals, bad = check(args["--home"], plan)
    print(f"agent files: {len(files)}; removing items: {len(removals)}; hits: {len(bad)}")
    for b in bad:
        print("HIT", b)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
