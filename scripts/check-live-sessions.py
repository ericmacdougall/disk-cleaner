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
    within the window, by the cwd they record;
  * `--ground-truth CMD`: a source the tool does not read (on the harness hosts, the live-owner
    listing). A session name that appears in its output as a whole token is live. If the command
    fails, the check cannot be made: exit 2.

A transcript is read for its top-level `cwd` only, from its first 64 KiB or 20 lines, whichever
is smaller; nothing else of it is kept or printed. Output names sessions and item paths only.

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


def transcript_cwd(path):
    """The top-level cwd a transcript records, within the bound; None when there is none."""
    try:
        with open(path, "rb") as f:
            head = f.read(HEAD_BYTES)
    except OSError:
        return None
    for line in head.split(b"\n")[:HEAD_LINES]:
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if isinstance(obj, dict) and isinstance(obj.get("cwd"), str):
            return obj["cwd"]
    return None


class Oracle:
    def __init__(self, home, now, window_days, processes=True):
        self.processes = processes
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
                continue
            if not isinstance(raw, dict):
                continue
            pid, cwd = raw.get("pid"), raw.get("cwd")
            sid = raw.get("sessionId")
            del raw  # only the three fields are kept
            if not isinstance(pid, int) or not pid_alive(pid):
                continue
            start = pid_start(pid)
            if start is not None and start > mtime + 1:
                continue  # the pid was reused after the entry was written
            s = self.session_of(cwd if isinstance(cwd, str) else None)
            if s:
                self.mark(s, "registry pid %d" % pid)
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
                elif p and p.endswith(".jsonl"):
                    s = self.session_of(transcript_cwd(p))
                    if s:
                        self.mark(s, "agent pid %d holds a transcript" % pid)

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
                    s = self.session_of(transcript_cwd(p))
                    if s:
                        self.mark(s, "recent transcript")

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
        if s in oracle.live and it.get("action") in REMOVING:
            offered.append((s, it.get("path")))
    tool_live = {
        s.get("session") for s in plan.get("totals", {}).get("sessions", []) if s.get("live")
    }
    missed = sorted(s for s in oracle.live if s in owned and s not in tool_live)
    return offered, missed


def main(argv, processes=True):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--plan")
    ap.add_argument("--home", default=os.path.expanduser("~"))
    ap.add_argument("--window-days", type=int, default=7)
    ap.add_argument("--now", type=float, default=None)
    ap.add_argument("--ground-truth")
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args(argv)
    if a.self_test:
        return self_test()
    if not a.plan:
        ap.error("--plan is required")
    with open(a.plan) as f:
        plan = json.load(f)
    now = a.now if a.now is not None else time.time()
    oracle = Oracle(a.home, now, a.window_days, processes)
    try:
        live = oracle.run(a.ground_truth)
    except (subprocess.SubprocessError, OSError) as e:
        print("cannot run the ground-truth command (%s): no verdict" % type(e).__name__)
        return 2
    print("oracle: %d of %d sessions live" % (len(live), len(oracle.sessions)))
    for s in sorted(live):
        print("  live  %s  (%s)" % (s, live[s]))
    offered, missed = check(plan, oracle)
    mapped = [i for i in plan.get("items", []) if oracle.session_of(i.get("path"))]
    print("plan: %d of %d items lie in a session directory" % (len(mapped), len(plan.get("items", []))))
    for s, p in offered:
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

        for s in ["old", "reg", "dead", "tr", "recent", "wal", "gt", "abc", "abcd"]:
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
        put(".claude/wals/wal/main.jsonl", ago=400 * 86400)
        os.utime(os.path.join(home, ".claude/wals/wal"), (now - 3600, now - 3600))
        # A transcript recording `old` but read past the bound: line 21.
        put(".claude/projects/-p/late.jsonl",
            "{}\n" * HEAD_LINES + json.dumps({"cwd": work + "/old"}) + "\n", ago=3600)

        def item(s, action):
            return {"path": work + "/%s/wt" % s, "action": action}

        plan = {
            "items": [item("old", "backup-then-remove"), item("reg", "keep"),
                      item("dead", "backup-then-remove"), item("tr", "backup-then-remove"),
                      item("recent", "keep"), item("abcd", "backup-then-remove")],
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
        want = {"reg", "tr", "recent", "wal", "gt", "abc"}
        if set(live) != want:
            problems.append("live sessions %s, expected %s" % (sorted(live), sorted(want)))
        if code != 1:
            problems.append("exit %d, expected 1" % code)
        if "OFFERED: live session tr" not in out:
            problems.append("the offered checkout of tr was not named")
        if "MISSED: the plan lists live session recent as idle" not in out:
            problems.append("the missed session was not named")
        if "abcd" in "".join(l for l in out.splitlines() if l.startswith(("OFFERED", "MISSED"))):
            problems.append("abcd was flagged through its prefix abc")
        if canary in out:
            problems.append("the canary reached the output")
        # A clean plan passes.
        plan["items"] = [i for i in plan["items"] if i["path"].split("/")[-2] not in want]
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
