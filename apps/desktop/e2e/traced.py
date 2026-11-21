#!/usr/bin/env python3
"""The end of a traced session (PR review F11): nothing the session started may outlive it.

A traced launch (check.sh's wrapper, idle-launch.sh, observer-controls.sh) runs
  PRUNEDISK_TRACE_SESSION=LOG strace -DD -o LOG <trace_report.py --strace-args> PROGRAM
so the program keeps the launcher's process id and strace is a detached grandchild, and every
process of the session carries the session's log in its environment.

usage:
  traced.py settle LOG [--wait-pid PID] [--grace SECONDS] [--record FILE] [--label TEXT]
      waits for PID to end (when given), then up to SECONDS (2) for every process of the session
      to end: those whose environment names LOG and those traced by the strace writing LOG.
      Each one still alive then is a survivor: it is printed (and appended to FILE) with its
      state, parent, open descriptor count and tracer, and killed (SIGKILL, never a signal to
      strace, which would let its processes go untraced). The strace writing LOG must then end
      by itself within 10 s. Exit 0 when nothing survived, 1 when anything did.
  traced.py tracers LOG
      the process ids of the strace writing LOG (for a control that kills it)
  traced.py sweep DIR [--grace SECONDS] [--record FILE]
      the same for every session whose log is in DIR (the end of a suite); also stops
      `traced.py settle` watchers of DIR still waiting for a program that never ended.
Linux only; standard library only.
"""

from __future__ import annotations

import os
import signal
import sys
import time

MARK = "PRUNEDISK_TRACE_SESSION"
POLL = 0.05


def read(path: str, binary: bool = False):
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError:
        return None
    return data if binary else data.decode("utf-8", "replace")


class Proc:
    def __init__(self, pid: int):
        self.pid = pid
        st = read(f"/proc/{pid}/status") or ""
        fields = dict(line.split(":\t", 1) for line in st.splitlines() if ":\t" in line)
        self.name = fields.get("Name", "?").strip()
        self.state = fields.get("State", "?").strip()
        self.ppid = int(fields.get("PPid", "0") or 0)
        self.tracer = int(fields.get("TracerPid", "0") or 0)
        env = read(f"/proc/{pid}/environ", binary=True) or b""
        self.session = None
        for item in env.split(b"\0"):
            if item.startswith(MARK.encode() + b"="):
                self.session = item[len(MARK) + 1:].decode("utf-8", "surrogateescape")
        argv = (read(f"/proc/{pid}/cmdline", binary=True) or b"").split(b"\0")
        self.argv = [a.decode("utf-8", "surrogateescape") for a in argv if a]
        stat = read(f"/proc/{pid}/stat") or ""
        self.start = stat.rsplit(")", 1)[-1].split()[19] if ")" in stat else None
        try:
            self.fds = len(os.listdir(f"/proc/{pid}/fd"))
        except OSError:
            self.fds = -1

    @property
    def alive(self) -> bool:
        return bool(self.state) and not self.state.startswith(("Z", "X", "?"))

    def strace_log(self) -> str | None:
        """The -o file of a strace process."""
        if not self.argv or os.path.basename(self.argv[0]) != "strace":
            return None
        for i, a in enumerate(self.argv[:-1]):
            if a == "-o":
                return self.argv[i + 1]
        return None

    def line(self, label: str, now: float, t0: float) -> str:
        return (f"survivor {label}: pid={self.pid} ppid={self.ppid} state={self.state.split()[0] if self.state else '?'} "
                f"name={self.name} fds={self.fds} tracer={self.tracer} session={self.session} "
                f"after={int((now - t0) * 1000)}ms")


def processes() -> list[Proc]:
    out = []
    for d in os.listdir("/proc"):
        if d.isdigit() and int(d) not in (os.getpid(), os.getppid()):
            p = Proc(int(d))
            if p.state:
                out.append(p)
    return out


def session_of(p: Proc, logs) -> str | None:
    """The session log `p` belongs to, if any of `logs` (a set, or a folder prefix)."""
    def match(x):
        return x is not None and (x in logs if isinstance(logs, set) else x.startswith(logs))
    if match(p.session) and p.strace_log() is None and not is_watcher(p):
        return p.session
    return None


def is_watcher(p: Proc) -> bool:
    return any(os.path.basename(a) == "traced.py" for a in p.argv[:3]) and "settle" in p.argv


def members(logs) -> tuple[list[Proc], list[Proc]]:
    """(the live processes of the sessions, the strace processes writing their logs)."""
    procs = processes()
    tracers = [p for p in procs if p.alive and (lambda x: x is not None and (x in logs if isinstance(logs, set) else x.startswith(logs)))(p.strace_log())]
    tpids = {t.pid for t in tracers}
    live = [p for p in procs if p.alive and p.pid not in tpids and (session_of(p, logs) or p.tracer in tpids)]
    return live, tracers


def emit(text: str, record: str | None) -> None:
    print(text, flush=True)
    if record:
        with open(record, "a") as f:
            f.write(text + "\n")


def ended(pid: int, start: str | None) -> bool:
    p = Proc(pid)
    return not p.alive or (start is not None and p.start != start)


def settle(logs, label: str, grace: float, record: str | None, wait_pid: int | None, wait_max: float) -> int:
    if wait_pid is not None:
        start = Proc(wait_pid).start
        deadline = time.monotonic() + wait_max
        while not ended(wait_pid, start):
            if time.monotonic() > deadline:
                emit(f"survivor {label}: pid={wait_pid} the program never ended ({wait_max:.0f} s)", record)
                break
            time.sleep(POLL)
    t0 = time.monotonic()
    live, tracers = members(logs)
    emit(f"closed {label}: at the end {len(live)} processes of the session alive, {len(tracers)} strace", record)
    for p in live:
        emit("  at-end " + p.line(label, t0, t0)[len("survivor "):], record)
    while live and time.monotonic() - t0 < grace:
        time.sleep(POLL)
        live, tracers = members(logs)
    now = time.monotonic()
    bad = 0
    for p in live:
        emit(p.line(label, now, t0), record)
        bad += 1
        try:
            os.kill(p.pid, signal.SIGKILL)
        except OSError:
            pass
    if not bad:
        emit(f"closed {label}: every process of the session ended within {int((now - t0) * 1000)} ms", record)
    # strace ends when its last process has; it is never signalled while one may still run.
    deadline = time.monotonic() + 10
    while True:
        live, tracers = members(logs)
        if not tracers:
            break
        if time.monotonic() > deadline:
            for t in tracers:
                emit(f"survivor {label}: strace pid={t.pid} still running 10 s after its processes ended "
                     f"(still traced: {[p.pid for p in live]})", record)
                bad += 1
                for p in live:
                    try:
                        os.kill(p.pid, signal.SIGKILL)
                    except OSError:
                        pass
                try:
                    os.kill(t.pid, signal.SIGKILL)
                except OSError:
                    pass
            break
        for p in live:  # a process started after the grace: also a survivor
            emit(p.line(label, time.monotonic(), t0), record)
            bad += 1
            try:
                os.kill(p.pid, signal.SIGKILL)
            except OSError:
                pass
        time.sleep(POLL)
    emit(f"done {label}: {bad} survivors", record)
    return 1 if bad else 0


def main(argv: list[str]) -> int:
    if len(argv) < 2 or argv[0] not in ("settle", "sweep", "tracers"):
        print(__doc__, file=sys.stderr)
        return 2
    cmd, target, rest = argv[0], argv[1], argv[2:]
    if cmd == "tracers":
        _, tracers = members({os.path.abspath(target)})
        print("\n".join(str(t.pid) for t in tracers))
        return 0 if tracers else 1
    opts = {"--grace": "2", "--record": None, "--label": None, "--wait-pid": None, "--wait-max": "3600"}
    while rest:
        if rest[0] not in opts or len(rest) < 2:
            print(__doc__, file=sys.stderr)
            return 2
        opts[rest[0]] = rest[1]
        rest = rest[2:]
    target = os.path.abspath(target)
    grace, wait_max = float(opts["--grace"]), float(opts["--wait-max"])
    if cmd == "settle":
        label = opts["--label"] or os.path.basename(target)
        wp = int(opts["--wait-pid"]) if opts["--wait-pid"] else None
        return settle({target}, label, grace, opts["--record"], wp, wait_max)
    prefix = target.rstrip("/") + "/"
    for p in processes():
        if is_watcher(p) and any(a.startswith(prefix) for a in p.argv):
            emit(f"sweep: stopping the watcher pid={p.pid} ({' '.join(p.argv[-6:])}): its program never ended", opts["--record"])
            try:
                os.kill(p.pid, signal.SIGKILL)
            except OSError:
                pass
    return settle(prefix, opts["--label"] or "suite", grace, opts["--record"], None, 0)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
