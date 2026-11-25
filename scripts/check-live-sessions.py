#!/usr/bin/env python3
"""Independent oracle: no checkout of a live agent-harness session is offered for removal.

    check-live-sessions.py --plan PLAN.json [--home DIR] [--window-days N] [--now UNIX]
                           [--ground-truth CMD]
    check-live-sessions.py --self-test

Run it while the plan is made (the evidence is read now, not from the scan). It shares no code
with the tool and lists the live sessions under <home>/.agent-harness/work/ from its own
sources:

  * the Claude Code registry (<home>/.claude/sessions/<digits>.json): only the pid, sessionId
    and cwd fields are read; the entry counts while its pid is alive and was started before the
    entry was written (a reused pid does not count). `.key` files are never opened;
  * running agent processes of this user (claude, codex, kimi, cursor-agent, or node/bun running
    a Claude Code or Codex entry point): their working directory, open files, a session id as a
    whole argument, and the cwd recorded in a transcript they hold open;
  * files below a session directory modified within the window;
  * entries named after a session at depth 3 or less in .claude, .codex or the harness root
    (outside work/) modified within the window (WAL directories and the like);
  * transcripts (.claude/projects/**/*.jsonl, .codex/sessions/**/rollout-*.jsonl) modified
    within the window, by the cwd they record; that cwd (outside /, the home, the harness root
    and work/) also holds every item it equals, contains or lies inside, whatever its action;
  * `--ground-truth CMD`: a source the tool does not read (on the harness hosts, the live-owner
    listing). A session name that appears in its output as a whole token is live. If the command
    fails, the check cannot be made: exit 2.

A transcript is read for its working directory only, from its first 64 KiB or 20 lines, whichever
is smaller: the top-level `cwd` (Claude Code) or `payload.cwd` of a `session_meta` line (Codex,
also in .codex/archived_sessions). Any other shape is counted and printed as a gap class
(`transcript-cwd-unknown`, like an unreadable file or a registry file without a pid); nothing else
of it is kept or printed. A Claude Code transcript with no cwd within the bound is placed by its
project directory name: every existing directory whose encoding (each non-alphanumeric character
to `-`) equals it, found by a walk from / that reads names only; no match, or an incomplete walk
(`transcript-slug-incomplete`), is a gap.
`--slug-candidates -- NAME...` prints that decoding. Output names sessions, gap counts and item
paths only.
`--transcript-cwd FILE...` prints each file's result under that contract (the tool's tests compare).

Exit 1, naming each one, when an oracle-live session has an item the plan removes, or when the
plan lists an oracle-live session (one owning plan items) as idle. Exit 0 otherwise.
Python 3 standard library only.
"""
import argparse
import contextlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import time

HEAD_BYTES = 64 * 1024
HEAD_LINES = 20
AGENT_NAMES = {"claude", "codex", "kimi", "cursor-agent"}
REMOVING = {"remove-regenerable", "backup-then-remove", "recover-backup-then-remove"}
WALK_BUDGET = 200_000
SLUG_MAX_DIRS = 2048  # directories listed while decoding one project directory name
SLUG_MAX_ENTRIES = 4_000_000  # entries looked at in all by one such walk (names only)
SLUG_INCOMPLETE = "transcript-slug-incomplete"
TEMP_ROOTS = ["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"]


def transcript_cwd(path):
    """(cwd, None) or (None, gap class) for a transcript, within the bound.

    The contract the tool follows too: the cwd field only, top-level for Claude Code, and
    `payload.cwd` of a `"type": "session_meta"` line for Codex. Any other shape within the
    first 64 KiB or 20 lines (another line kind, a first line longer than the bound, text that
    is not JSON) is the gap `transcript-cwd-unknown`; a file that cannot be read is
    `transcript-unreadable`.
    """
    try:
        with open(path, "rb") as f:
            head = f.read(HEAD_BYTES)
    except OSError:
        return None, "transcript-unreadable"
    lines = head.split(b"\n")
    if len(head) >= HEAD_BYTES:
        lines = lines[:-1]  # the last line was cut by the byte bound
    for line in lines[:HEAD_LINES]:
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if not isinstance(obj, dict):
            continue
        cwd = obj.get("cwd")
        if isinstance(cwd, str) and cwd.startswith("/"):
            return cwd, None
        payload = obj.get("payload")
        if obj.get("type") == "session_meta" and isinstance(payload, dict):
            cwd = payload.get("cwd")
            if isinstance(cwd, str) and cwd.startswith("/"):
                return cwd, None
    return None, "transcript-cwd-unknown"


def claude_slug(name):
    """Claude Code's project directory name: every non-alphanumeric character becomes `-`."""
    return re.sub(r"[^A-Za-z0-9]", "-", name)


def slug_candidates(slug, max_dirs=SLUG_MAX_DIRS):
    """(sorted candidate dirs, incomplete): every existing directory whose encoding equals
    `slug`, by a walk from / that reads names only. Incomplete when the walk hit its bound, a
    listing cap, or a directory it had to list could not be listed."""
    cands, incomplete = [], False
    if not slug.startswith("-") or len(slug) > 4096:
        return cands, incomplete
    stack, listed, seen = [("/", 0)], 0, 0
    while stack:
        d, at = stack.pop()
        if at == len(slug):
            cands.append(d)
            continue
        if listed >= max_dirs:
            incomplete = True
            break
        listed += 1
        try:
            it = os.scandir(d)
        except OSError:
            incomplete = True
            continue
        with it:
            for e in it:
                seen += 1
                if seen > SLUG_MAX_ENTRIES:
                    incomplete = True
                    break
                enc = "-" + claude_slug(e.name)
                if not slug.startswith(enc, at):
                    continue
                end = at + len(enc)
                if end < len(slug) and slug[end] != "-":
                    continue
                try:
                    isdir = e.is_dir()
                except OSError:
                    isdir = False
                if isdir:
                    stack.append((os.path.join(d, e.name), end))
    return sorted(cands), incomplete


def claude_project_slug(path):
    """The project directory name of a Claude Code transcript, or None."""
    name = os.path.basename(path)
    if not name.endswith(".jsonl") or name.startswith("rollout-"):
        return None
    parts = os.path.normpath(path).split(os.sep)
    for i in range(len(parts) - 2, 0, -1):
        if parts[i - 1] == "projects":
            return parts[i]
    return None


class Oracle:
    def __init__(self, home, now, window_days, processes=True, slug_max_dirs=SLUG_MAX_DIRS):
        self.processes = processes
        self.slug_max_dirs = slug_max_dirs
        self.slugs = {}
        self.home = os.path.realpath(home)
        self.work = os.path.join(self.home, ".agent-harness", "work")
        self.now = now
        self.window = window_days * 86400
        try:
            self.sessions = sorted(
                d for d in os.listdir(self.work) if os.path.isdir(os.path.join(self.work, d))
            )
        except OSError:
            self.sessions = []
        self.live = {}  # session -> first source
        self.gaps = {}  # gap class -> count
        self.recent_gaps = {}  # those modified within the window (or of unknown age)
        self.recent_cwds = []  # (cwd, transcript) outside the ancestor-exception list
        self.loci = []  # (path, source) of processes and registry entries inside a session
        self.agent_dirs = [self.work] + [
            os.path.join(self.home, d, "worktrees") for d in (".claude", ".codex")
        ]

    def gap(self, cls, path=None):
        self.gaps[cls] = self.gaps.get(cls, 0) + 1
        try:
            fresh = path is None or self.recent(os.lstat(path).st_mtime)
        except OSError:
            fresh = True
        if fresh:
            self.recent_gaps[cls] = self.recent_gaps.get(cls, 0) + 1

    def holds_nothing_below(self, path):
        """The one ancestor-exception list: /, the home and every ancestor of it, the harness
        root, work/ (anything at or above the harness work dir), and the system temp roots
        (/tmp, /var/tmp, /private/tmp, /private/var/tmp, $TMPDIR) name no place and hold
        nothing."""
        p = os.path.normpath(path)
        for w in {self.work, os.path.realpath(self.work)}:
            if p == w or p == "/" or w.startswith(p.rstrip(os.sep) + os.sep):
                return True
        places = set()
        for t in TEMP_ROOTS + ([os.environ["TMPDIR"]] if os.environ.get("TMPDIR") else []):
            places.update({os.path.normpath(t), os.path.realpath(t)})
        for h in {self.home, os.path.realpath(self.home)}:
            while True:
                places.add(h)
                up = os.path.dirname(h)
                if up == h:
                    break
                h = up
        return p in places or os.path.realpath(p) in places

    def held_by_recent(self, path):
        """The recent transcript cwd that equals, contains or lies inside `path`, or None."""
        p = os.path.normpath(path or "")
        if not p.startswith("/"):
            return None
        for c, _ in self.recent_cwds:
            for a, b in ((p, c), (os.path.realpath(p), os.path.realpath(c))):
                if a == b or a.startswith(b.rstrip(os.sep) + os.sep) or \
                        b.startswith(a.rstrip(os.sep) + os.sep):
                    return c
        return None

    def related(self, a, b):
        """Does path a equal, contain or lie inside path b?"""
        for x, y in ((os.path.normpath(a), os.path.normpath(b)),
                     (os.path.realpath(a), os.path.realpath(b))):
            if x == y or x.startswith(y.rstrip(os.sep) + os.sep) or \
                    y.startswith(x.rstrip(os.sep) + os.sep):
                return True
        return False

    def own_activity(self, path):
        """The newest mtime of a node's own files (its .git directory left out), its git admin
        files and its .git file's ctime; bounded."""
        newest, seen = None, 0
        for root, dirs, files in os.walk(path):
            if ".git" in dirs:
                dirs.remove(".git")
            for n in files + dirs:
                seen += 1
                if seen > WALK_BUDGET:
                    return self.now  # too big to judge: active (fail safe)
                try:
                    m = os.lstat(os.path.join(root, n)).st_mtime
                except OSError:
                    continue
                newest = m if newest is None else max(newest, m)
        dot = os.path.join(path, ".git")
        admin = []
        if os.path.isdir(dot) and not os.path.islink(dot):
            admin = [os.path.join(dot, n) for n in ("index", "HEAD", "FETCH_HEAD")]
        elif os.path.isfile(dot):
            try:
                st = os.lstat(dot)
                newest = st.st_ctime if newest is None else max(newest, st.st_ctime)
                with open(dot, "rb") as f:
                    line = f.read(4096).decode("utf-8", "replace").splitlines()[0]
                gd = line.split("gitdir:", 1)[1].strip()
                gd = gd if os.path.isabs(gd) else os.path.join(path, gd)
                admin = [os.path.join(gd, n) for n in ("HEAD", "index", "logs/HEAD")]
            except (OSError, IndexError):
                return self.now  # unknown admin files: active (fail safe)
        for a in admin:
            try:
                m = os.stat(a).st_mtime
            except OSError:
                continue
            newest = m if newest is None else max(newest, m)
        return newest

    def in_agent_dir(self, path):
        p = os.path.normpath(path or "")
        return any(
            p.startswith(b + os.sep) for d in self.agent_dirs for b in {d, os.path.realpath(d)}
        )

    def cwds_of(self, path):
        """The working directories a transcript records: its cwd, or, for a Claude Code
        transcript with none within the bound, every candidate of its project directory name.
        No match, or an incomplete walk, is a gap."""
        cwd, gap = transcript_cwd(path)
        if not gap:
            return [cwd]
        slug = claude_project_slug(path)
        if slug is None:
            self.gap(gap, path)
            return []
        if slug not in self.slugs:
            self.slugs[slug] = slug_candidates(slug, self.slug_max_dirs)
        cands, incomplete = self.slugs[slug]
        if incomplete:
            self.gap(SLUG_INCOMPLETE, path)
        if not cands and not incomplete:
            self.gap(gap, path)
        return cands

    def recent(self, mtime):
        return mtime >= self.now - self.window

    def mark(self, session, source):
        if session in self.sessions:
            self.live.setdefault(session, source)

    def session_of(self, path):
        if not path:
            return None
        p = os.path.normpath(path)
        for base in {self.work, os.path.realpath(self.work)}:
            if p.startswith(base + os.sep):
                return p[len(base) + 1:].split(os.sep)[0]
        return None

    # -- the Claude Code registry
    def registry(self):
        d = os.path.join(self.home, ".claude", "sessions")
        try:
            names = os.listdir(d)
        except OSError:
            return
        for n in names:
            if not re.fullmatch(r"[0-9]+\.json", n):
                continue  # never a .key file
            path = os.path.join(d, n)
            try:
                mtime = os.stat(path).st_mtime
                with open(path, "rb") as f:
                    raw = json.loads(f.read(HEAD_BYTES))
            except (OSError, ValueError):
                self.gap("registry-entry-unknown", path)
                continue
            if not isinstance(raw, dict) or not isinstance(raw.get("pid"), int):
                self.gap("registry-entry-unknown", path)
                continue
            pid, cwd = raw.get("pid"), raw.get("cwd")
            sid = raw.get("sessionId")
            del raw  # only the three fields are kept
            if not pid_alive(pid):
                continue
            start = pid_start(pid)
            if start is not None and start > mtime + 1:
                continue  # the pid was reused after the entry was written
            s = self.session_of(cwd if isinstance(cwd, str) else None)
            if s:
                self.mark(s, "registry pid %d" % pid)
                self.loci.append((cwd, "registry pid %d" % pid))
            if isinstance(sid, str):
                self.mark(sid, "registry pid %d" % pid)

    # -- running agent processes
    def agents(self):
        for pid, args, cwd, files in agent_processes():
            for a in args[1:]:
                value = a.split("=", 1)[1] if a.startswith("-") and "=" in a else a
                for tok in {a, value, *value.split(":")}:
                    self.mark(tok, "agent pid %d names it" % pid)
            for p in [cwd] + files:
                s = self.session_of(p)
                if s:
                    self.mark(s, "agent pid %d works in it" % pid)
                    self.loci.append((p, "agent pid %d" % pid))
                elif p and p.endswith(".jsonl"):
                    for c in self.cwds_of(p):
                        s = self.session_of(c)
                        if s:
                            self.mark(s, "agent pid %d holds a transcript" % pid)
                            self.loci.append((c, "agent pid %d's transcript" % pid))

    # -- recent files below session dirs
    def session_files(self):
        for s in self.sessions:
            if s in self.live:
                continue
            seen = 0
            for root, dirs, files in os.walk(os.path.join(self.work, s)):
                for n in files + dirs:
                    seen += 1
                    try:
                        st = os.lstat(os.path.join(root, n))
                    except OSError:
                        continue
                    if self.recent(st.st_mtime):
                        self.mark(s, "recent file below it")
                        break
                if s in self.live or seen > WALK_BUDGET:
                    break

    # -- same-named entries
    def same_named(self):
        names = set(self.sessions)
        for base in (".claude", ".codex", ".agent-harness"):
            top = os.path.join(self.home, base)
            for root, dirs, files in os.walk(top):
                depth = root[len(top):].count(os.sep)
                if base == ".agent-harness" and depth == 0 and "work" in dirs:
                    dirs.remove("work")
                if depth >= 2:
                    dirs[:] = [d for d in dirs if d in names]
                if depth >= 3:
                    dirs[:] = []
                for n in dirs + files:
                    stem = n.split(".")[0]
                    hit = n if n in names else (stem if stem in names else None)
                    if not hit:
                        continue
                    try:
                        if self.recent(os.lstat(os.path.join(root, n)).st_mtime):
                            self.mark(hit, "recent same-named entry")
                    except OSError:
                        pass

    # -- recent transcripts
    def transcripts(self):
        roots = [
            (os.path.join(self.home, ".claude", "projects"), lambda n: n.endswith(".jsonl")),
            (
                os.path.join(self.home, ".codex", "sessions"),
                lambda n: n.startswith("rollout-") and n.endswith(".jsonl"),
            ),
            (
                os.path.join(self.home, ".codex", "archived_sessions"),
                lambda n: n.startswith("rollout-") and n.endswith(".jsonl"),
            ),
        ]
        for top, want in roots:
            for root, dirs, files in os.walk(top):
                if root[len(top):].count(os.sep) >= 4:
                    dirs[:] = []
                for n in files:
                    if not want(n):
                        continue
                    p = os.path.join(root, n)
                    try:
                        if not self.recent(os.stat(p).st_mtime):
                            continue
                    except OSError:
                        continue
                    for c in self.cwds_of(p):
                        s = self.session_of(c)
                        if s:
                            self.mark(s, "recent transcript")
                        if not self.holds_nothing_below(c):
                            self.recent_cwds.append((c, p))

    def ground_truth(self, cmd):
        out = subprocess.run(
            cmd, shell=True, capture_output=True, text=True, timeout=120, check=True
        ).stdout
        for s in self.sessions:
            pat = r"(?<![A-Za-z0-9_.-])" + re.escape(s) + r"(?![A-Za-z0-9_.-])"
            if re.search(pat, out):
                self.mark(s, "ground truth")

    def run(self, ground_truth=None):
        self.registry()
        if self.processes:
            self.agents()
        self.transcripts()
        self.same_named()
        self.session_files()
        if ground_truth:
            self.ground_truth(ground_truth)
        return self.live


def pid_alive(pid):
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


def pid_start(pid):
    """Process start time, unix seconds, or None."""
    try:
        with open("/proc/%d/stat" % pid) as f:
            fields = f.read().rsplit(")", 1)[1].split()
        with open("/proc/stat") as f:
            btime = next(int(l.split()[1]) for l in f if l.startswith("btime "))
        return btime + int(fields[19]) / os.sysconf("SC_CLK_TCK")
    except (OSError, IndexError, ValueError, StopIteration):
        pass
    try:
        out = subprocess.run(
            ["ps", "-o", "lstart=", "-p", str(pid)], capture_output=True, text=True, timeout=10
        ).stdout.strip()
        return time.mktime(time.strptime(out, "%a %b %d %H:%M:%S %Y")) if out else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def is_agent(args, comm):
    names = {os.path.basename(args[0]) if args else "", comm}
    if names & AGENT_NAMES:
        return True
    if names & {"node", "bun"}:
        joined = " ".join(args[1:3])
        return "claude-code" in joined or "@openai/codex" in joined or "/codex" in joined
    return False


def agent_processes():
    """(pid, argv, cwd, open files) of this user's agent processes."""
    me = os.getuid()
    out = []
    if os.path.isdir("/proc/self"):
        for e in os.listdir("/proc"):
            if not e.isdigit():
                continue
            d = "/proc/" + e
            try:
                if os.stat(d).st_uid != me:
                    continue
                with open(d + "/cmdline", "rb") as f:
                    args = [a.decode("utf-8", "replace") for a in f.read().split(b"\0") if a]
                with open(d + "/comm") as f:
                    comm = f.read().strip()
            except OSError:
                continue
            if not is_agent(args, comm):
                continue
            try:
                cwd = os.readlink(d + "/cwd")
            except OSError:
                cwd = None
            files = []
            try:
                for fd in os.listdir(d + "/fd"):
                    try:
                        t = os.readlink(d + "/fd/" + fd)
                    except OSError:
                        continue
                    if t.startswith("/"):
                        files.append(t)
            except OSError:
                pass
            out.append((int(e), args, cwd, files))
        return out
    def ps(fields):
        try:
            text = subprocess.run(
                ["ps", "-axo", "pid=,uid=," + fields + "="],
                capture_output=True, text=True, timeout=30,
            ).stdout
        except (OSError, subprocess.SubprocessError):
            return {}
        rows = {}
        for line in text.splitlines():
            parts = line.split(None, 2)
            if len(parts) == 3 and parts[0].isdigit() and parts[1] == str(me):
                rows[int(parts[0])] = parts[2]
        return rows

    # The executable name (`comm`; Claude Code's binary is a versioned file whose argv[0] is
    # not `claude`) and the arguments.
    comms, argvs = ps("comm"), ps("args")
    found = {}
    for pid, comm in comms.items():
        args = argvs.get(pid, "").split()
        if is_agent(args, os.path.basename(comm.strip())):
            found[pid] = args
    if not found:
        return out
    try:
        lsof = subprocess.run(
            ["lsof", "-n", "-P", "-w", "-F", "fn", "-a", "-p", ",".join(map(str, found))],
            capture_output=True,
            text=True,
            timeout=60,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        lsof = ""
    cur, fd, info = None, None, {p: [None, []] for p in found}
    for line in lsof.splitlines():
        tag, val = line[:1], line[1:]
        if tag == "p":
            cur = int(val) if val.isdigit() else None
        elif tag == "f":
            fd = val
        elif tag == "n" and cur in info and val.startswith("/"):
            if fd == "cwd":
                info[cur][0] = val
            else:
                info[cur][1].append(val)
    for pid, args in found.items():
        out.append((pid, args, info[pid][0], info[pid][1]))
    return out


def check(plan, oracle):
    """(offered violations, missed sessions)."""
    offered, owned = [], set()
    for it in plan.get("items", []):
        s = oracle.session_of(it.get("path"))
        if not s:
            continue
        owned.add(s)
        # r6, per node: in a live session an item is held by a locus or its own activity
        # within the window, and a checkout asks under 14 days of own activity.
        if s in oracle.live and it.get("action") in REMOVING:
            path = it.get("path")
            locus = next((l for l, _ in oracle.loci
                          if oracle.session_of(l) == s and not oracle.holds_nothing_below(l)
                          and oracle.related(path, l)), None)
            act = oracle.own_activity(path)
            if locus:
                offered.append((s, path))
            elif act is not None and act >= oracle.now - oracle.window:
                offered.append((s, path))
            elif os.path.exists(os.path.join(path, ".git")) and act is not None and \
                    act >= oracle.now - 14 * 86400:
                offered.append((s, path))
    tool_live = {
        s.get("session") for s in plan.get("totals", {}).get("sessions", []) if s.get("live")
    }
    missed = sorted(s for s in oracle.live if s in owned and s not in tool_live)
    # Rule 5: a recent transcript's cwd holds every item it equals, contains or lies inside,
    # whatever the removing action.
    for it in plan.get("items", []):
        if it.get("action") in REMOVING:
            c = oracle.held_by_recent(it.get("path"))
            if c:
                offered.append(("(recent session %s)" % c, it.get("path")))
    # Recent agent data that could not be read may record any agent session: no removing item
    # under an agent directory may be offered then.
    if oracle.recent_gaps:
        for it in plan.get("items", []):
            if it.get("action") in REMOVING and oracle.in_agent_dir(it.get("path")):
                offered.append(("(recent agent-data gaps)", it.get("path")))
    return offered, missed


def main(argv, processes=True):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--plan")
    ap.add_argument("--home", default=os.path.expanduser("~"))
    ap.add_argument("--window-days", type=int, default=7)
    ap.add_argument("--now", type=float, default=None)
    ap.add_argument("--ground-truth")
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--no-processes", action="store_true",
                    help="read no running process (for fixture homes)")
    ap.add_argument("--slug-max-dirs", type=int, default=SLUG_MAX_DIRS)
    ap.add_argument(
        "--slug-candidates", action="store_true",
        help="for each NAME (after --) print NAME<TAB>candidates (or `none`, `incomplete`): "
        "the contract test shared with the tool",
    )
    ap.add_argument("names", nargs="*", metavar="NAME")
    ap.add_argument(
        "--transcript-cwd", nargs="+", metavar="FILE",
        help="print FILE<TAB>cwd (or `gap`) for each file: the contract test shared with the tool",
    )
    a = ap.parse_args(argv)
    if a.self_test:
        return self_test()
    if a.slug_candidates:
        for n in a.names:
            cands, incomplete = slug_candidates(n, a.slug_max_dirs)
            print("\t".join([n] + (["incomplete"] if incomplete else cands or ["none"])))
        return 0
    if a.transcript_cwd:
        for f in a.transcript_cwd:
            cwd, _ = transcript_cwd(f)
            print("%s\t%s" % (f, cwd or "gap"))
        return 0
    if not a.plan:
        ap.error("--plan is required")
    with open(a.plan) as f:
        plan = json.load(f)
    now = a.now if a.now is not None else time.time()
    oracle = Oracle(a.home, now, a.window_days, processes and not a.no_processes,
                    a.slug_max_dirs)
    try:
        live = oracle.run(a.ground_truth)
    except (subprocess.SubprocessError, OSError) as e:
        print("cannot run the ground-truth command (%s): no verdict" % type(e).__name__)
        return 2
    print("oracle: %d of %d sessions live" % (len(live), len(oracle.sessions)))
    for s in sorted(live):
        print("  live  %s  (%s)" % (s, live[s]))
    for g in sorted(oracle.gaps):
        print("  gap   %d %s (each may hide a live session)" % (oracle.gaps[g], g))
    offered, missed = check(plan, oracle)
    mapped = [i for i in plan.get("items", []) if oracle.session_of(i.get("path"))]
    for label in ("session live; idle", "agent worktree; idle"):
        asks = [i for i in plan.get("items", [])
                if i.get("action") == "ask" and (i.get("reason") or "").startswith(label)]
        print("idle asks: %d \"%s N days\" (%d bytes)"
              % (len(asks), label, sum(i.get("bytes_total") or 0 for i in asks)))
    print("plan: %d of %d items lie in a session directory" % (len(mapped), len(plan.get("items", []))))
    for s, p in offered:
        if s.startswith("(recent session "):
            print("OFFERED: a removing item held by a recent agent session's working "
                  "directory %s: %s" % (s[len("(recent session "):-1], p))
        elif s.startswith("("):
            print("OFFERED: a removing item under an agent directory while recent agent data "
                  "is unreadable: %s" % p)
        else:
            print("OFFERED: live session %s has a removing item: %s" % (s, p))
    for s in missed:
        print("MISSED: the plan lists live session %s as idle" % s)
    if offered or missed:
        return 1
    print("ok: no checkout of a live session is offered")
    return 0


def self_test():
    # The real processes are not read: their open transcripts are the user's, not fixtures.
    canary = "CANARY-oracle-0b7d"
    now = time.time() + 400 * 86400
    with tempfile.TemporaryDirectory() as tmp:
        home = os.path.join(os.path.realpath(tmp), "home")
        work = os.path.join(home, ".agent-harness", "work")

        def put(rel, text="x", ago=400 * 86400):
            p = os.path.join(home, rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                f.write(text)
            os.utime(p, (now - ago, now - ago))
            return p

        def age_all():
            for root, dirs, files in os.walk(home):
                for n in dirs + files:
                    p = os.path.join(root, n)
                    if not os.path.islink(p) and not stat_fifo(p):
                        os.utime(p, (now - 400 * 86400, now - 400 * 86400))

        for s in ["old", "reg", "dead", "tr", "recent", "wal", "gt", "abc", "abcd", "cx", "cxa",
                  "sub", "so"]:
            put(".agent-harness/work/%s/wt/src/lib.rs" % s)
        age_all()
        # Registry: this process (alive, started before the entry) for `reg`; a reaped child's
        # pid for `dead`; a .key FIFO beside them that would block if opened.
        put(
            ".claude/sessions/%d.json" % os.getpid(),
            json.dumps({"pid": os.getpid(), "sessionId": "x", "cwd": work + "/reg/wt",
                        "name": canary}),
            ago=3600,
        )
        child = subprocess.Popen(["true"])
        child.wait()
        put(".claude/sessions/%d.json" % child.pid,
            json.dumps({"pid": child.pid, "cwd": work + "/dead/wt"}), ago=3600)
        key = os.path.join(home, ".claude", "sessions", "%d.key" % os.getpid())
        if hasattr(os, "mkfifo"):
            os.mkfifo(key)
        put(".claude/projects/-p/t.jsonl",
            json.dumps({"cwd": work + "/tr/wt", "secret": canary}) + "\n", ago=3600)
        put(".agent-harness/work/recent/notes.md", ago=3600)
        # r6 per node: in live sessions, an idle sibling with no locus may be offered; a
        # checkout with 10 days of own activity may not (it asks under 14 days).
        put(".agent-harness/work/reg/old/src/lib.rs", ago=400 * 86400)
        put(".agent-harness/work/recent/ten/.git/HEAD", "ref: refs/heads/main\n",
            ago=400 * 86400)
        put(".agent-harness/work/recent/ten/src/lib.rs", ago=10 * 86400)
        put(".claude/wals/wal/main.jsonl", ago=400 * 86400)
        os.utime(os.path.join(home, ".claude/wals/wal"), (now - 3600, now - 3600))
        # A transcript recording `old` but read past the bound: line 21.
        put(".claude/projects/-p/late.jsonl",
            "{}\n" * HEAD_LINES + json.dumps({"cwd": work + "/old"}) + "\n", ago=3600)
        # Codex rollouts: the cwd is `payload.cwd` of the session_meta line (no top-level cwd),
        # in sessions/YYYY/MM/DD/ and in archived_sessions/. A Claude subagent transcript.
        def rollout(cwd):
            return (json.dumps({"timestamp": "t", "type": "session_meta",
                                "payload": {"id": "0", "cwd": cwd, "instructions": canary}})
                    + "\n" + json.dumps({"type": "response_item", "payload": {"text": canary}})
                    + "\n")
        put(".codex/sessions/2026/01/01/rollout-1.jsonl", rollout(work + "/cx/wt"), ago=3600)
        put(".codex/archived_sessions/rollout-2.jsonl", rollout(work + "/cxa"), ago=3600)
        put(".claude/projects/-p/0000/subagents/agent-1.jsonl",
            json.dumps({"type": "user", "cwd": work + "/sub/wt"}) + "\n", ago=3600)
        # Gap shapes: a non-session_meta Codex line naming `old` under payload, an oversized
        # first line, garbage; and a registry file without a pid.
        put(".codex/sessions/2026/01/01/rollout-3.jsonl",
            json.dumps({"type": "response_item", "payload": {"cwd": work + "/old"}}) + "\n",
            ago=3600)
        put(".claude/projects/-p/big.jsonl",
            json.dumps({"cwd": work + "/old", "pad": "x" * HEAD_BYTES}), ago=3600)
        put(".claude/projects/-p/garbage.jsonl", "not json\n", ago=3600)
        put(".claude/sessions/77.json", json.dumps({"sessionId": "x"}), ago=3600)
        # The AC-10 shape: a session-owner first line and a second line past the bound; the
        # project directory name places it in `so`.
        put(".claude/projects/%s/0001.jsonl" % claude_slug(work + "/so/wt"),
            json.dumps({"type": "session-owner", "sessionId": "0001"}) + "\n"
            + json.dumps({"pad": "x" * (70 * 1024), "cwd": work + "/so/wt"}) + "\n", ago=3600)

        # Rule 5: a recent transcript's cwd outside the sessions holds what lies inside it; one
        # at the home (the ancestor-exception list) holds nothing.
        put(".claude/projects/-q/r5.jsonl", json.dumps({"cwd": home + "/proj"}) + "\n", ago=3600)
        put(".claude/projects/-q/r5h.jsonl", json.dumps({"cwd": home}) + "\n", ago=3600)

        def item(s, action):
            return {"path": work + "/%s/wt" % s, "action": action}

        plan = {
            "items": [item("old", "backup-then-remove"), item("reg", "keep"),
                      item("dead", "backup-then-remove"), item("tr", "backup-then-remove"),
                      item("recent", "keep"), item("abcd", "backup-then-remove"),
                      {"path": home + "/proj/x/node_modules", "action": "remove-regenerable"},
                      {"path": home + "/other/y", "action": "backup-then-remove"},
                      {"path": work + "/reg/old", "action": "backup-then-remove"},
                      {"path": work + "/recent/ten", "action": "backup-then-remove"}],
            "totals": {"sessions": [{"session": "reg", "live": True},
                                    {"session": "recent", "live": False}]},
        }
        pf = os.path.join(tmp, "plan.json")
        with open(pf, "w") as f:
            json.dump(plan, f)
        args = ["--plan", pf, "--home", home, "--now", str(now),
                "--ground-truth", "echo 'owner gt running; abc; xabcdx'"]
        buf = io.StringIO()
        if hasattr(__import__("signal"), "alarm"):
            import signal
            signal.alarm(30)  # a blocked FIFO open fails the test instead of hanging it
        with contextlib.redirect_stdout(buf):
            code = main(args, processes=False)
        if hasattr(__import__("signal"), "alarm"):
            import signal
            signal.alarm(0)
        out = buf.getvalue()
        o = Oracle(home, now, 7, processes=False)
        live = o.run("echo 'owner gt running; abc; xabcdx'")
        problems = []
        want = {"reg", "tr", "recent", "wal", "gt", "abc", "cx", "cxa", "sub", "so"}
        if set(live) != want:
            problems.append("live sessions %s, expected %s" % (sorted(live), sorted(want)))
        gaps = {"transcript-cwd-unknown": 4, "registry-entry-unknown": 1}
        if o.gaps != gaps:
            problems.append("gaps %s, expected %s" % (o.gaps, gaps))
        if "gap   4 transcript-cwd-unknown" not in out:
            problems.append("the transcript gaps were not printed")
        if code != 1:
            problems.append("exit %d, expected 1" % code)
        if ("OFFERED: a removing item held by a recent agent session's working directory "
                "%s/tr/wt: %s/tr/wt" % (work, work)) not in out:
            problems.append("the offered checkout of tr was not named")
        if "OFFERED: live session reg has a removing item: %s/reg/old" % work in out:
            problems.append("an idle sibling in a live session with no locus was flagged")
        if "OFFERED: live session recent has a removing item: %s/recent/ten" % work not in out:
            problems.append("a 10-day-old checkout of a live session was not flagged")
        if "MISSED: the plan lists live session recent as idle" not in out:
            problems.append("the missed session was not named")
        if "abcd" in "".join(l for l in out.splitlines()
                             if l.startswith(("OFFERED: live session", "MISSED"))):
            problems.append("abcd was flagged through its prefix abc")
        if canary in out:
            problems.append("the canary reached the output")
        if o.recent_gaps != gaps:
            problems.append("recent gaps %s, expected %s" % (o.recent_gaps, gaps))
        if ("OFFERED: a removing item under an agent directory while recent agent data is "
                "unreadable: %s/old/wt" % work) not in out:
            problems.append("an offered work-dir item beside recent gaps was not flagged")
        if ("OFFERED: a removing item held by a recent agent session's working directory "
                "%s/proj: %s/proj/x/node_modules" % (home, home)) not in out:
            problems.append("an item inside a recent transcript's cwd was not flagged")
        if "other/y" in out:
            problems.append("a transcript cwd at the home held an item below it")
        # The shared exception list: temp roots, the home and its ancestors name no place.
        for c in ["/", "/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp", home,
                  os.path.dirname(home), work]:
            if not o.holds_nothing_below(c):
                problems.append("%s is not on the exception list" % c)
        for c in [home + "/x", "/tmp/x", work + "/s"]:
            if o.holds_nothing_below(c):
                problems.append("%s is on the exception list" % c)
        # A clean plan passes once the unreadable agent data is older than the window (then it
        # is only counted).
        for rel in [".claude/projects/-p/late.jsonl", ".claude/projects/-p/big.jsonl",
                    ".claude/projects/-p/garbage.jsonl",
                    ".codex/sessions/2026/01/01/rollout-3.jsonl", ".claude/sessions/77.json",
                    ".claude/projects/-q/r5.jsonl"]:
            os.utime(os.path.join(home, rel), (now - 30 * 86400, now - 30 * 86400))
        plan["items"] = [i for i in plan["items"] if i["path"].split("/")[-2] not in want
                         or i["path"].endswith("/reg/old")]
        plan["totals"]["sessions"].append({"session": "recent", "live": True})
        with open(pf, "w") as f:
            json.dump(plan, f)
        with contextlib.redirect_stdout(io.StringIO()):
            if main(args, processes=False) != 0:
                problems.append("a clean plan did not pass")
            if main(["--plan", pf, "--home", home, "--now", str(now),
                     "--ground-truth", "false"], processes=False) != 2:
                problems.append("a failing ground-truth command did not stop the check")
    for p in problems:
        print("self-test FAILED: " + p)
    if not problems:
        print("self-test ok")
    return 1 if problems else 0


def stat_fifo(p):
    import stat
    try:
        return stat.S_ISFIFO(os.lstat(p).st_mode)
    except OSError:
        return False


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
