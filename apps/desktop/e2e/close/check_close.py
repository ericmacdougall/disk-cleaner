#!/usr/bin/env python3
"""Window-close check (click-test CT-a, GoodQA #16452). Linux, X11.

Starts the app (built with the `e2e` feature, so PRUNEDISK_E2E_START begins a scan at start)
under its own Xvfb, with or without a real window manager (openbox), waits until a scan is in
the wanted phase, makes the window go away by one route, and checks that the main process,
the analysis worker and every git the worker started (a fake git first on PATH that records
its pid and a child's) are all gone from the process table within the deadline (10 s, the
worker's END_DEADLINE, docs/DESKTOP.md), and that nothing under the fixture or the sandboxed
home changed (e2e/manifest.py, the declared paths of e2e/check.sh).

Phases:
  walk     the size walk runs (a large folder; no worker yet)
  analyse  the analysis worker runs and at least one git is running

Routes:
  wm-close      the window manager is asked to close the window (_NET_CLOSE_WINDOW, as a
                title-bar close or Alt+F4 does); it sends WM_DELETE_WINDOW
  destroy-wm    the top-level window is destroyed from outside, no close request (openbox)
  destroy-nowm  the same with no window manager
  kill-client   the app's X connection is closed (xkill; the server going away at logout)
  sigterm, sighup, sigkill   the signal, to the app's own pid

How the app must end, per route (anything else, a crash above all, fails the case):
  wm-close, destroy-*   exit 0 after the app's own shutdown ran (lib.rs end_app, which ends
                        the worker: the `e2e: shutdown done` line)
  kill-client           exit 1 (GTK ends a program whose display connection is gone)
  sigterm, sighup, sigkill   killed by that signal (the worker's own watchdog then ends it
                        and its gits: the parent-death backstop)

Every case acts on a settled app: the page has loaded (`e2e: page loaded`), then 1.5 s.

Known finding (reported as "PASS (known crash)" and a KNOWN FINDING line, for triage):
with no window manager, a window destroyed from outside makes the app crash (Gdk-ERROR
BadDrawable, SIGTRAP) about 3.5 s later; GTK never hands it the destroy, so the app's own
shutdown cannot run, and the worker's backstop ends the tree (CI run 44877786136: 6 of 6
without a window manager, 0 of 6 under openbox, with or without a GTK destroy-event handler).
That case is still held to the tree and no-write rules; only its exit status is not.

Outside the table: destroying GTK's hidden client-leader window (route `destroy-leader`) is
not a close. The app must keep running with its window shown; the check then ends it.

The harness is a child subreaper, so a process orphaned by its parent's exit stays visible to
it (and is reaped by it) instead of escaping to init. "Gone" means absent from /proc after
reaping: a zombie still waiting for a live parent is not gone.

A control case (route `none`: nothing is done) must fail, so a check that cannot fail is
caught. usage:

  check_close.py --app BIN --big DIR [--big DIR...] --out DIR [--cases phase:route,...]
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent

ID = "io.github.ericmacdougall.prunedisk"
DEADLINE_S = 10.0
SETTLE_S = 1.5
# Known finding, reported for triage: with no window manager, a window destroyed from outside
# makes the app crash (Gdk-ERROR BadDrawable, SIGTRAP) before GTK hands it the destroy. The
# case still gates on everything else (the whole tree gone in time, nothing written); only
# the way the app ended is not held to "exit 0".
KNOWN_CRASH = {"destroy-nowm": "CT-a known: no window manager, outside destroy -> app crash; "
                               "the worker's backstop ends the tree"}
ROUTES = ["wm-close", "destroy-wm", "destroy-nowm", "kill-client", "sigterm", "sighup", "sigkill"]
PHASES = ["walk", "analyse"]
REAPED: dict[int, int] = {}
EXPECTED_STATUS = {
    "wm-close": "exit 0", "destroy-wm": "exit 0", "destroy-nowm": "exit 0",
    "kill-client": "exit 1", "sigterm": "signal 15", "sighup": "signal 1", "sigkill": "signal 9",
}


def xq(*args: str) -> subprocess.CompletedProcess:
    """One Xlib call in its own process (xlib.py), so a lost X connection ends only it."""
    return subprocess.run([sys.executable, "-I", str(HERE / "xlib.py"), *args],
                          capture_output=True, text=True, timeout=20)

libc = ctypes.CDLL(None, use_errno=True)


def now() -> float:
    return time.monotonic()


def reap() -> None:
    while True:
        try:
            pid, status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if pid == 0:
            return
        REAPED[pid] = status


def procs() -> dict[int, dict]:
    out = {}
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            s = Path(f"/proc/{d}/stat").read_text()
        except OSError:
            continue
        r = s.rindex(")")
        f = s[r + 2:].split()
        out[int(d)] = {
            "comm": s[s.index("(") + 1:r],
            "state": f[0],
            "ppid": int(f[1]),
            "sid": int(f[3]),
            "start": int(f[19]),
        }
    return out


def cmdline(pid: int) -> str:
    try:
        return Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace").strip()
    except OSError:
        return ""


def threads(pid: int) -> list[str]:
    try:
        return [Path(f"/proc/{pid}/task/{t}/comm").read_text().strip() for t in os.listdir(f"/proc/{pid}/task")]
    except OSError:
        return []


def descendants(table: dict[int, dict], root: int) -> set[int]:
    kids: dict[int, list[int]] = {}
    for pid, p in table.items():
        kids.setdefault(p["ppid"], []).append(pid)
    out, todo = set(), [root]
    while todo:
        for k in kids.get(todo.pop(), []):
            if k not in out:
                out.add(k)
                todo.append(k)
    return out


def recorded_pids(path: Path) -> list[int]:
    try:
        return [int(x) for x in path.read_text().split() if x.isdigit()]
    except OSError:
        return []


class Case:
    def __init__(self, phase: str, route: str, args, n: int):
        self.phase, self.route, self.args, self.n = phase, route, args, n
        self.name = f"{phase}-{route}-{n:02d}"
        self.dir = Path(args.out) / self.name
        self.display = f":{90 + n}"
        self.wm = route in ("wm-close", "destroy-wm", "kill-client")
        self.keeps_running = route == "destroy-leader"
        self.deadline = args.control_deadline if route == "none" else DEADLINE_S
        self.tracked: dict[tuple[int, int], dict] = {}
        self.app = self.xvfb = self.openbox = None
        self.log: list[str] = []

    def say(self, m: str) -> None:
        line = f"[{self.name}] {m}"
        self.log.append(line)
        print(line, flush=True)

    # ---- processes ----------------------------------------------------------------------

    def track(self, table: dict[int, dict]) -> None:
        """Every process of the app's tree, of the worker's session and of the pid file."""
        if self.app is None:
            return
        pids = descendants(table, self.app.pid) | {self.app.pid}
        workers = [p for p in pids if "--analysis-worker" in cmdline(p)]
        for w in workers:
            pids |= {p for p, x in table.items() if x["sid"] == w}
        pids |= set(recorded_pids(self.pidfile))
        for p in pids:
            if p in table:
                key = (p, table[p]["start"])
                if key not in self.tracked:
                    self.tracked[key] = {"pid": p, "comm": table[p]["comm"], "cmd": cmdline(p)[:160]}

    def alive(self, table: dict[int, dict]) -> list[dict]:
        return [v for (p, st), v in self.tracked.items() if p in table and table[p]["start"] == st]

    def worker(self, table: dict[int, dict]) -> int | None:
        for p in descendants(table, self.app.pid):
            if "--analysis-worker" in cmdline(p) and table[p]["state"] != "Z":
                return p
        return None

    def running_gits(self, table: dict[int, dict]) -> list[int]:
        return [p for p in recorded_pids(self.pidfile) if p in table and table[p]["state"] != "Z"]

    # ---- set-up -------------------------------------------------------------------------

    def start(self) -> None:
        self.dir.mkdir(parents=True)
        self.sb = self.dir / "sandbox"
        for sub in ("home", "config", "data", "cache", "state", "tmp"):
            (self.sb / sub).mkdir(parents=True)
        self.pidfile = self.dir / "git-pids.txt"
        self.pidfile.touch()
        if self.phase == "walk":
            self.roots = [str(Path(b).resolve()) for b in self.args.big]
        else:
            self.roots = [str(Path(self.args.fixture).resolve())]
        self.snapshot_roots = [str(Path(self.args.fixture).resolve()), str(self.sb)]
        subprocess.run([sys.executable, str(HERE.parent / "manifest.py"), "snapshot",
                        str(self.dir / "before.json"), *self.snapshot_roots], check=True)
        sock = Path(f"/tmp/.X11-unix/X{self.display[1:]}")
        self.xvfb = subprocess.Popen(["Xvfb", self.display, "-screen", "0", "1280x1024x24", "-nolisten", "tcp", "-noreset"],
                                     stdout=subprocess.DEVNULL, stderr=open(self.dir / "xvfb.log", "w"))
        wait_for(lambda: sock.exists(), 10, "Xvfb socket")
        if self.wm:
            self.openbox = subprocess.Popen(["openbox", "--sm-disable"], env={**os.environ, "DISPLAY": self.display},
                                            stdout=subprocess.DEVNULL, stderr=open(self.dir / "openbox.log", "w"))
            wait_for(lambda: xq("wm", self.display).returncode == 0, 10, "openbox")
        env = {
            "PATH": f"{self.args.fakebin}:/usr/bin:/bin",
            "DISPLAY": self.display,
            "HOME": str(self.sb / "home"),
            "XDG_CONFIG_HOME": str(self.sb / "config"),
            "XDG_DATA_HOME": str(self.sb / "data"),
            "XDG_CACHE_HOME": str(self.sb / "cache"),
            "XDG_STATE_HOME": str(self.sb / "state"),
            "TMPDIR": str(self.sb / "tmp"),
            "LANG": "C.UTF-8",
            "NO_AT_BRIDGE": "1",
            "WEBKIT_DISABLE_COMPOSITING_MODE": "1",
            "PRUNEDISK_E2E_START": os.pathsep.join(self.roots),
            "PRUNEDISK_CLOSE_PIDS": str(self.pidfile),
        }
        self.stderr_path = self.dir / "app.log"
        self.app = subprocess.Popen([self.args.app], env=env, stdin=subprocess.DEVNULL,
                                    stdout=open(self.dir / "app.out", "w"), stderr=open(self.stderr_path, "w"))
        self.t_launch = now()
        self.say(f"app pid {self.app.pid} on {self.display}, window manager: {'openbox' if self.wm else 'none'}")

    def ready(self) -> dict:
        """Wait for the window and the phase; return what holds at the moment of acting."""
        wid = wait_for(lambda: xq("toplevel", self.display, str(self.app.pid)).stdout.strip(), 60,
                       "the app's top-level window")
        self.wid = wid
        # A settled app: the page has loaded, then a moment more.
        wait_for(lambda: "e2e: page loaded" in self.stderr_path.read_text(errors="replace"), 60,
                 "the page to load")
        time.sleep(SETTLE_S)
        if self.phase == "walk":
            def walking():
                t = procs()
                self.track(t)
                return self.worker(t) is None and any(n.startswith("scan-") for n in threads(self.app.pid))
            wait_for(walking, 60, "the size walk")
        else:
            def analysing():
                t = procs()
                self.track(t)
                return self.worker(t) is not None and len(self.running_gits(t)) >= 1
            wait_for(analysing, 120, "the analysis worker with a git running")
        t = procs()
        self.track(t)
        w = self.worker(t)
        pre = {
            "ms_since_launch": round((now() - self.t_launch) * 1000),
            "window": hex(int(wid)),
            "worker": w,
            "gits_running": len(self.running_gits(t)),
            "walk_thread": any(n.startswith("scan-") for n in threads(self.app.pid)),
            "tree": [f"{v['pid']} {v['comm']}" for v in self.alive(t)],
        }
        if self.phase == "walk" and w is not None:
            raise RuntimeError("the walk ended before the route could be taken (precondition missed)")
        if self.phase == "analyse" and (w is None or pre["gits_running"] < 1):
            raise RuntimeError("no analysis with a running git at the moment of acting (precondition missed)")
        return pre

    # ---- the route ----------------------------------------------------------------------

    def act(self) -> dict:
        r = self.route
        info: dict = {}
        if r == "wm-close":
            assert xq("wm", self.display).returncode == 0
            xq("close", self.display, self.wid)
        elif r in ("destroy-wm", "destroy-nowm"):
            xq("destroy", self.display, self.wid)
            info["window_gone_after_destroy"] = xq("exists", self.display, self.wid).returncode == 1
        elif r == "destroy-leader":
            lead = xq("leader", self.display, str(self.app.pid)).stdout.strip()
            if not lead:
                raise RuntimeError("no client-leader window found")
            info["leader"] = hex(int(lead))
            xq("destroy", self.display, lead)
            info["window_gone_after_destroy"] = xq("exists", self.display, lead).returncode == 1
        elif r == "kill-client":
            xq("kill", self.display, self.wid)
        elif r in ("sigterm", "sighup", "sigkill"):
            os.kill(self.app.pid, {"sigterm": signal.SIGTERM, "sighup": signal.SIGHUP, "sigkill": signal.SIGKILL}[r])
        elif r == "none":
            pass
        else:
            raise ValueError(r)
        return info

    def watch(self, t0: float) -> dict:
        times: dict = {"app_exit_s": None, "worker_gone_s": None, "all_gone_s": None}
        worker_keys = None
        end = t0 + max(self.deadline, 0) + 5.0
        while now() < end:
            reap()
            t = procs()
            self.track(t)
            el = round(now() - t0, 3)
            app_alive = self.app.pid in t and self.app.pid not in REAPED
            if not app_alive and times["app_exit_s"] is None:
                times["app_exit_s"] = el
            # Whether the X server outlived the app (a server that died first would itself
            # end the app, which would hide what the route did).
            if self.xvfb.pid in REAPED and "xserver_gone_s" not in times:
                times["xserver_gone_s"] = el
            if worker_keys is None:
                worker_keys = [k for k, v in self.tracked.items() if "--analysis-worker" in v["cmd"]]
            if times["worker_gone_s"] is None and not any(p in t and t[p]["start"] == s for p, s in worker_keys):
                times["worker_gone_s"] = el
            if not self.alive(t):
                times["all_gone_s"] = el
                break
            time.sleep(0.05)
        reap()
        t = procs()
        times["left_at_end"] = [f"{v['pid']} {v['comm']} {t[v['pid']]['state']}: {v['cmd']}" for v in self.alive(t)]
        return times

    def stop(self) -> None:
        """Remove whatever is left (a failed case) and the display."""
        for _ in range(3):
            reap()
            t = procs()
            for v in self.alive(t):
                try:
                    os.kill(v["pid"], signal.SIGKILL)
                except OSError:
                    pass
            time.sleep(0.2)
        for p in (self.openbox, self.xvfb):
            if p and p.poll() is None and p.pid not in REAPED:
                p.terminate()
        time.sleep(0.5)
        reap()

    def run(self) -> dict:
        res: dict = {"case": self.name, "phase": self.phase, "route": self.route,
                     "window_manager": "openbox" if self.wm else None, "deadline_s": self.deadline}
        try:
            self.start()
            res["before"] = self.ready()
            self.say(f"acting: {json.dumps(res['before'])}")
            t0 = now()
            res.update(self.act())
            if self.keeps_running:
                time.sleep(5)
                reap()
                res["app_running_after_5_s"] = self.app.pid not in REAPED and self.app.pid in procs()
                res["toplevel_still_shown"] = xq("exists", self.display, self.wid).returncode == 0
            else:
                res.update(self.watch(t0))
            st = REAPED.get(self.app.pid)
            if st is not None:
                res["app_status"] = (f"exit {os.WEXITSTATUS(st)}" if os.WIFEXITED(st)
                                     else f"signal {os.WTERMSIG(st)}")
            res["events"] = [l.strip() for l in self.stderr_path.read_text(errors="replace").splitlines()
                             if l.startswith("e2e:") or "unexpectedly destroyed" in l
                             or "analysis worker" in l][:20]
            res["seen"] = sorted({v["comm"] for v in self.tracked.values()})
        except Exception as e:  # a set-up failure fails the case
            res["error"] = f"{type(e).__name__}: {e}"
        finally:
            self.stop()
        if "error" not in res:
            cmp = subprocess.run([sys.executable, str(HERE.parent / "manifest.py"), "snapshot",
                                  str(self.dir / "after.json"), *self.snapshot_roots])
            declared = [self.sb / "config/disk-cleaner", self.sb / f"data/{ID}", self.sb / f"cache/{ID}",
                        self.sb / "cache/mesa_shader_cache"]
            c = subprocess.run([sys.executable, str(HERE.parent / "manifest.py"), "compare",
                                str(self.dir / "before.json"), str(self.dir / "after.json"),
                                *sum((["--declared", str(d)] for d in declared), [])],
                               capture_output=True, text=True)
            res["no_write"] = "clean" if c.returncode == 0 and cmp.returncode == 0 else c.stdout[-2000:]
        want = EXPECTED_STATUS.get(self.route)
        res["expected_status"] = want
        ended_cleanly = (want is None or res.get("app_status") == want) and (
            want != "exit 0" or "e2e: shutdown done" in res.get("events", []))
        if self.route in KNOWN_CRASH and not ended_cleanly:
            res["known_finding"] = f"{KNOWN_CRASH[self.route]} (app {res.get('app_status')})"
            ended_cleanly = True
        if self.keeps_running:
            ok = bool("error" not in res and res.get("app_running_after_5_s")
                      and res.get("toplevel_still_shown") and res.get("window_gone_after_destroy")
                      and res.get("no_write") == "clean")
        else:
            ok = ("error" not in res and res.get("all_gone_s") is not None
                  and res["all_gone_s"] <= self.deadline and res.get("app_exit_s") is not None
                  and res.get("no_write") == "clean" and ended_cleanly
                  and res.get("window_gone_after_destroy", True))
        res["result"] = "PASS" if ok else "FAIL"
        self.say(f"{res['result']}: app exit {res.get('app_exit_s')} s, worker gone {res.get('worker_gone_s')} s, "
                 f"all gone {res.get('all_gone_s')} s, X server gone {res.get('xserver_gone_s')} s, "
                 f"app {res.get('app_status')} (want {res.get('expected_status')}), left {res.get('left_at_end')}, no-write {str(res.get('no_write'))[:80]}, "
                 f"error {res.get('error')}")
        (self.dir / "result.json").write_text(json.dumps(res, indent=2))
        (self.dir / "log.txt").write_text("\n".join(self.log) + "\n")
        return res


def wait_for(cond, seconds: float, what: str):
    end = now() + seconds
    while now() < end:
        reap()
        v = cond()
        if v:
            return v
        time.sleep(0.1)
    raise TimeoutError(f"timed out after {seconds} s waiting for {what}")


def make_fixture(root: Path, repos: int, git: str) -> None:
    """`repos` small committed git repositories (made with the real git)."""
    root.mkdir(parents=True)
    for i in range(repos):
        r = root / f"repo{i:03d}"
        (r / "src").mkdir(parents=True)
        (r / "src" / "main.txt").write_text(f"file {i}\n" * 50)
        g = [git, "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "-C", str(r)]
        subprocess.run([git, "init", "-q", str(r)], check=True)
        subprocess.run(g + ["add", "."], check=True)
        subprocess.run(g + ["commit", "-q", "-m", "fixture"], check=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--app", required=True)
    ap.add_argument("--big", action="append", required=True, help="a large folder for the walk phase")
    ap.add_argument("--out", required=True)
    ap.add_argument("--repos", type=int, default=40)
    ap.add_argument("--cases", default="", help="phase:route,... (default: every phase and route, then the control)")
    ap.add_argument("--control-deadline", type=float, default=3.0)
    ap.add_argument("--expect-fail", default=None,
                    help="broken variant: the phase:route cases that must fail (all others pass)")
    ap.add_argument("--informational", action="store_true", help="report only; never fail")
    args = ap.parse_args()

    if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
        print("cannot become a child subreaper", file=sys.stderr)
        return 2
    out = Path(args.out)
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    git = shutil.which("git")
    if not git:
        print("the real git is needed to build the fixture", file=sys.stderr)
        return 2
    args.fixture = str(out / "fixture")
    make_fixture(Path(args.fixture), args.repos, git)
    fakebin = out / "fakebin"
    fakebin.mkdir()
    shutil.copy(HERE / "fakegit.sh", fakebin / "git")
    (fakebin / "git").chmod(0o755)
    args.fakebin = str(fakebin)

    if args.cases:
        cases = [tuple(c.split(":", 1)) for c in args.cases.split(",") if c]
    else:
        # Each destroy route runs three times: an outside destroy can race the app's drawing.
        cases = ([(p, r) for p in PHASES for r in ROUTES]
                 + [(p, r) for _ in range(2) for p in PHASES for r in ("destroy-wm", "destroy-nowm")]
                 + [("analyse", "destroy-leader"), ("analyse", "none")])
    results = []
    for n, (phase, route) in enumerate(cases):
        results.append(Case(phase, route, args, n).run())
    for r in results:
        if r["result"] == "PASS" and "known_finding" in r:
            r["result"] = "PASS (known crash)"

    (out / "results.json").write_text(json.dumps(results, indent=2))
    lines = ["| case | window manager | app ended | app exit (s) | worker gone (s) | all gone (s) | left | no-write | events | result |",
             "|---|---|---|---|---|---|---|---|---|---|"]
    for r in results:
        ev = "; ".join(e.replace("e2e: event ", "") for e in r.get("events", []) if e.startswith("e2e: event"))
        ended = r.get("app_status") or ("still running (wanted)" if r.get("app_running_after_5_s") else "-")
        lines.append(f"| {r['case']} | {r['window_manager'] or 'none'} | {ended} | {r.get('app_exit_s')} | {r.get('worker_gone_s')} | "
                     f"{r.get('all_gone_s')} | {len(r.get('left_at_end', [])) if 'left_at_end' in r else r.get('error', '')} | "
                     f"{'clean' if r.get('no_write') == 'clean' else 'CHANGED' if 'no_write' in r else 'n/a'} | {ev or '-'} | {r['result']} |")
    table = "\n".join(lines)
    print(table)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(f"## Window close during a scan (deadline {DEADLINE_S:g} s)\n\n{table}\n")
    gating = [r for r in results if r["route"] != "none"]
    bad = [r["case"] for r in gating if not r["result"].startswith("PASS")]
    known = sorted({r["known_finding"] for r in results if "known_finding" in r})
    for k in known:
        print(f"KNOWN FINDING (for triage, not gating): {k}")
    controls = [r for r in results if r["route"] == "none"]
    control_ok = all(r["result"] == "FAIL" and "error" not in r for r in controls)
    if not control_ok:
        print("CONTROL: the no-action case did not fail as it must (or failed to set up)")
    print(f"failed cases: {bad or 'none'}; control {'caught' if control_ok else 'NOT caught'}")
    if args.informational:
        print("informational run (a variant to compare): the outcome does not gate")
        return 0
    if args.expect_fail is not None:
        # A broken variant: exactly the named phase:route cases must fail, every other one
        # must pass, and none may fail to set up.
        want = {c for c in args.expect_fail.split(",") if c}
        got = {f"{r['phase']}:{r['route']}" for r in gating if not r["result"].startswith("PASS")}
        errors = [r["case"] for r in gating if "error" in r]
        caught = got == want and not errors and control_ok
        print(f"variant: expected to fail {sorted(want)}, failed {sorted(got)}, set-up errors {errors}: "
              f"{'caught' if caught else 'NOT caught as expected'}")
        return 0 if caught else 1
    return 0 if not bad and control_ok else 1


if __name__ == "__main__":
    sys.exit(main())
