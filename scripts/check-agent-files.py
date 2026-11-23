#!/usr/bin/env python3
"""Independent check that a disk-cleaner plan removes no agent data.

    check-agent-files.py --home DIR --plan PLAN.json [--scan SCAN.json]
    check-agent-files.py --self-test

The check knows nothing of the tool's code: it carries its own protected-root list, carve-out
positions, allowlist and name patterns, walks the home itself and reads the plan only for the
paths of its removing items. It fails (exit 1) when a removing item is equal to, above or below an
agent file, or when a removing item under `.agent-harness/work/<session>/` or inside a carve-out
checkout is neither such a checkout nor an allowlisted directory. Nothing is filtered by hand:
every hit is printed for triage. Roots are resolved to their real paths and walked there (a
symlinked `.agent-harness` is followed to its target), and every path, agent file and removal
alike, is compared by its real path.

Allowlisted directories are judged by what they are, never by name alone: a `target/` is Cargo
build output only when the scan (`--scan`) records the Cargo rule's identity for it AND the check
itself finds `Cargo.toml`, and neither `pom.xml` nor `build.sbt`, beside it on disk. A Maven or
sbt `target/` (test reports, packaged jars) is not allowlisted; without `--scan` no `target/` is.

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
# Directory names that only one kind of tool makes: the name tells what the directory is.
ALLOWLIST = {
    "node_modules", ".venv", "venv", "__pycache__", ".pytest_cache", ".mypy_cache",
    ".ruff_cache", ".tox", ".gradle", ".turbo", ".parcel-cache",
}
# `target` is Cargo's and Maven's or sbt's: allowlisted only as Cargo build output (see
# cargo_target).
CARGO_ID = "cargo-target"
NOT_CARGO_MARKERS = ("pom.xml", "build.sbt")
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


def layout(spelled):
    """The carve-out layout of a root, from its home-relative spelling (not its real path, which
    may be named anything)."""
    base = os.path.basename(spelled)
    if base == ".agent-harness":
        return "harness"
    if base in (".claude", ".codex"):
        return "worktrees"
    return None


def protected_roots(home):
    """Every protected root as (real path, layout). A root that is a symlink (or lies below one)
    is compared, walked and carved at its real path, so the spelling of the home or of the scan
    root never decides what is protected."""
    spelled = {os.path.join(home, r) for r in ROOTS}
    try:
        for e in os.listdir(home):
            if e.startswith(PREFIXES):
                spelled.add(os.path.join(home, e))
    except OSError:
        pass
    out = {}
    for sp in sorted(spelled):
        real = os.path.realpath(sp)
        lay = layout(sp)
        if out.get(real) is None:
            out[real] = lay
    return sorted(out.items())


def under(path, root):
    return path == root or path.startswith(root + os.sep)


def is_checkout(d):
    return os.path.exists(os.path.join(d, ".git"))


def carve_nodes(roots):
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

    for r, lay in roots:
        if lay == "harness":
            for s in kids(os.path.join(r, "work")):
                for x in kids(s):
                    if is_checkout(x):
                        if not top_agent(x):
                            out.append(x)
                    else:
                        for y in kids(x):
                            if is_checkout(y):
                                out.append(y)
        elif lay == "worktrees":
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


def scan_identities(scan):
    """Real path -> (rule identity, other matching identities) of every scan node, or None when no
    scan was given (then no `target/` is allowlisted)."""
    if scan is None:
        return None
    out = {}
    for n in scan.get("nodes", []):
        p = n.get("real_path") or n.get("path")
        if p:
            out[os.path.realpath(p)] = (n.get("rule_id"), tuple(n.get("rule_also") or ()))
    return out


def cargo_target(d, ident):
    """Is `d` Cargo build output? The scan must record the Cargo rule's identity for it, with no
    other rule matching, and the disk must agree: Cargo.toml beside it, no pom.xml or build.sbt."""
    if os.path.basename(d) != "target" or ident is None:
        return False
    if ident.get(os.path.realpath(d)) != (CARGO_ID, ()):
        return False
    parent = os.path.dirname(d)
    return os.path.isfile(os.path.join(parent, "Cargo.toml")) and not any(
        os.path.exists(os.path.join(parent, m)) for m in NOT_CARGO_MARKERS
    )


def allowlisted(d, ident):
    return os.path.basename(d) in ALLOWLIST or cargo_target(d, ident)


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


def walk_files(top):
    """Real paths of the regular files below `top` (symlinks are not followed)."""
    out = []
    for dp, dn, fn in os.walk(top):
        dn[:] = [d for d in dn if not os.path.islink(os.path.join(dp, d))]
        for f in fn:
            p = os.path.join(dp, f)
            if not os.path.islink(p):
                out.append(os.path.realpath(p))
    return out


def agent_files(home, ident=None):
    home = os.path.realpath(home)
    roots = protected_roots(home)
    root_paths = [r for r, _ in roots]
    harness = [r for r, lay in roots if lay == "harness"]
    carves = carve_nodes(roots)
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

    # The home and every root at its real path (a symlinked root is skipped by the home walk).
    files = set(walk_files(home))
    for r in root_paths:
        if os.path.isdir(r):
            files.update(walk_files(r))
        elif os.path.isfile(r):
            files.add(r)
    found = []
    for p in sorted(files):
        dp, f = os.path.split(p)
        if log_shape(f):
            found.append(p)
            continue
        if not any(under(p, r) for r in root_paths):
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
        exempt = False
        for h in harness:
            wbase = os.path.join(h, "work")
            d = dp
            while under(d, wbase) and d.count(os.sep) > wbase.count(os.sep) + 1:
                if allowlisted(d, ident) and dep_clean(d):
                    exempt = True
                    break
                d = os.path.dirname(d)
        if not exempt:
            found.append(p)
    return found


def dep_has_agent_file(d):
    for dp, _dn, fn in os.walk(d):
        for f in fn:
            if agent_name(f) and not in_package(os.path.join(dp, f), d):
                return True
    return False


def misplaced(home, removals, ident):
    """Removing items below a work session (`.agent-harness/work/<s>/...`) or inside a carve-out
    checkout that are neither a carve-out checkout nor an allowlisted directory holding no agent
    file. The tool may remove nothing else there: any other directory can hold agent results."""
    roots = protected_roots(os.path.realpath(home))
    carves = set(carve_nodes(roots))
    works = [os.path.join(r, "work") for r, lay in roots if lay == "harness"]
    out = []
    for r in removals:
        below_session = any(
            under(r, w) and r.count(os.sep) >= w.count(os.sep) + 2 for w in works
        )
        in_carve = any(under(r, c) and r != c for c in carves)
        if not (below_session or in_carve) or r in carves:
            continue
        if allowlisted(r, ident) and not dep_has_agent_file(r):
            continue
        out.append(r)
    return out


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


def check(home, plan, scan=None):
    ident = scan_identities(scan)
    files = agent_files(home, ident)
    removals = [os.path.realpath(p) for p in removing_paths(plan)]
    bad = hits(removals, files)
    for r in misplaced(home, removals, ident):
        if r not in bad:
            bad.append(r)
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

        def run(label, removals, want_fail, scan=None):
            nonlocal ok
            plan = {"items": [{"path": p, "action": "remove-regenerable"} for p in removals]}
            _files, _rem, bad = check(home, plan, scan)
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

        # Class N: `target` is allowlisted only as Cargo build output. A Maven or sbt target/
        # (test reports, packaged jars) in a task dir or inside a carve-out checkout must fail.
        for kind, marker in (("maven", "pom.xml"), ("sbt", "build.sbt"), ("cargo", "Cargo.toml")):
            task = os.path.join(work, kind + "-task")
            write(os.path.join(task, "session.jsonl"))
            write(os.path.join(task, marker))
            write(os.path.join(task, "target", "surefire-reports", "TEST-a.xml"))
            co = os.path.join(work, kind + "-clone")
            os.makedirs(co)
            git(co, "init", "-q")
            write(os.path.join(co, marker))
            write(os.path.join(co, ".gitignore"), "target/\n")
            git(co, "add", ".")
            git(co, "commit", "-q", "-m", "init")
            write(os.path.join(co, "target", "surefire-reports", "TEST-a.xml"))
            targets = [os.path.join(task, "target"), os.path.join(co, "target")]
            # What the scan records: Cargo's identity for a Cargo target/, Maven's otherwise.
            ids = {"nodes": [
                {"path": t, "rule": "target", "rule_id": CARGO_ID if kind == "cargo" else "maven-target"}
                for t in targets
            ]}
            bad = kind != "cargo"
            run(kind + " target/ in a task dir", targets[:1], bad, ids)
            run(kind + " target/ inside a carve-out checkout", targets[1:], bad, ids)
            if kind == "cargo":
                # Fail closed: no scan, a scan recording another identity or a second matching
                # rule, a scan lying about a Maven target/, or a pom.xml beside the Cargo.toml.
                run("cargo target/ without --scan", targets, True)
                lied = {"nodes": [dict(n, rule_id="maven-target") for n in ids["nodes"]]}
                run("cargo target/ the scan calls maven-target", targets, True, lied)
                also = {"nodes": [dict(n, rule_also=["maven-target"]) for n in ids["nodes"]]}
                run("cargo target/ the scan says two rules matched", targets, True, also)
                write(os.path.join(task, "pom.xml"))
                run("target/ beside both Cargo.toml and pom.xml", targets[:1], True, ids)
                os.remove(os.path.join(task, "pom.xml"))
                run("cargo target/ again, Cargo.toml alone", targets[:1], False, ids)
            else:
                lie = {"nodes": [dict(n, rule_id=CARGO_ID) for n in ids["nodes"]]}
                run(kind + " target/ the scan calls cargo-target", targets, True, lie)
        # Any other removal inside a carve-out checkout or below a session fails, even one that
        # holds no agent-named file (build output can hold agent results under any name).
        write(os.path.join(clone, "out", "results.tsv"))
        run("out/ inside a carve-out checkout", [os.path.join(clone, "out")], True)
        write(os.path.join(work, "task", "dist", "report.html"))
        run("dist/ in a task dir", [os.path.join(work, "task", "dist")], True)

    # `.agent-harness` a symlink to a directory outside the home: the check must follow the root
    # to its real path, or it would see no agent file there and pass anything.
    with tempfile.TemporaryDirectory() as tmp:
        base = os.path.realpath(tmp)
        home = os.path.join(base, "home")
        real = os.path.join(base, "ah-data")
        os.makedirs(home)
        os.makedirs(real)
        os.symlink(real, os.path.join(home, ".agent-harness"))
        rwork = os.path.join(real, "work", "s")
        agentdata = os.path.join(rwork, "agentdata")
        os.makedirs(agentdata)
        git(agentdata, "init", "-q")
        write(os.path.join(agentdata, "src", "a.rs"))
        write(os.path.join(agentdata, "run.wal"))
        git(agentdata, "add", ".")
        git(agentdata, "commit", "-q", "-m", "init")
        rclone = os.path.join(rwork, "clone")
        os.makedirs(rclone)
        git(rclone, "init", "-q")
        write(os.path.join(rclone, "src", "b.rs"))
        git(rclone, "add", ".")
        git(rclone, "commit", "-q", "-m", "init")
        write(os.path.join(rwork, "task", "session.jsonl"))
        write(os.path.join(rwork, "task", "node_modules", "a.js"))

        def run2(label, removals, want_fail):
            nonlocal ok
            plan = {"items": [{"path": p, "action": "backup-then-remove"} for p in removals]}
            files, _rem, bad = check(home, plan)
            good = bool(bad) == want_fail and len(files) > 0
            ok &= good
            print(("ok   " if good else "FAIL ") + label + f" (agent files: {len(files)}, hits: {len(bad)})")

        run2("symlinked harness: clean carve-out checkout at its real path", [rclone], False)
        run2("symlinked harness: node_modules in a task dir", [os.path.join(rwork, "task", "node_modules")], False)
        run2("symlinked harness: agent-data checkout at its real path", [agentdata], True)
        run2(
            "symlinked harness: agent-data checkout spelled through the link",
            [os.path.join(home, ".agent-harness", "work", "s", "agentdata")],
            True,
        )
        run2("symlinked harness: the task dir", [os.path.join(rwork, "task")], True)
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
    scan = None
    if "--scan" in args:
        with open(args["--scan"]) as f:
            scan = json.load(f)
    else:
        print("no --scan: no target/ is treated as Cargo build output")
    files, removals, bad = check(args["--home"], plan, scan)
    print(f"agent files: {len(files)}; removing items: {len(removals)}; hits: {len(bad)}")
    for b in bad:
        print("HIT", b)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
