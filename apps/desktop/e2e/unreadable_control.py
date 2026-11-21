#!/usr/bin/env python3
"""The kernel check behind the write observer's "unreadable" calls (docs/DESKTOP.md, "A call
strace could not read"), on the host that runs it: its kernel, its strace, the observer's own
strace options.

  unreadable_control.py KILLRACE OUT

KILLRACE is e2e/killrace.c built (static, so any host runs it). Each run starts it under
`strace -DD -o LOG <trace_report.py --strace-args>` with 16 threads that each loop mkdirat on
"<tid>-<i>", and kills it after 30 ms, two ways: SIGKILL from another process, and the
WebKit-like kill (a parent kills its child, which has a parent-death SIGKILL, and exits). Then:
  - every call strace printed under its stub name ("???(" or "????(") must not have run: the
    folder of that thread's next index must not exist;
  - trace_report.py must pass every such call as "unreadable", with no UNPARSED line, and
    count as many as this script found;
  - at least one stub must appear in all the runs (otherwise the check proved nothing).
Exit 0 when all three hold, 1 otherwise, 2 on bad usage.
"""

from __future__ import annotations

import io
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time
from contextlib import redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import trace_report as tr  # noqa: E402

MODES = ("extkill", "webkit")
RUNS = 20  # per mode; each run takes well under a second
LINE = re.compile(r"^(\d+) +\d+\.\d+ (.*)$")
NAME = re.compile(r'"(\d+)-(\d+)"')


def strace_alive() -> bool:
    """Whether a strace still runs (a zombie, left for a container's first process to reap, has
    finished its log)."""
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                stat = (p / "stat").read_text()
            except OSError:
                continue
            name, state = stat[stat.find("(") + 1:stat.rfind(")")], stat[stat.rfind(")") + 2:][:1]
            if name == "strace" and state != "Z":
                return True
    return False


def stubs(log: str, d: str) -> tuple[int, list[str]]:
    """How many stub calls the log has, and one line for each that ran."""
    by_tid: dict[str, list[str]] = {}
    for raw in Path(log).read_text(errors="replace").splitlines():
        m = LINE.match(raw)
        if m:
            by_tid.setdefault(m.group(1), []).append(m.group(2))
    count, ran = 0, []
    for tid, recs in by_tid.items():
        for k, rest in enumerate(recs):
            if not re.match(r"\?{3,4}\(", rest):
                continue
            count += 1
            last = max((int(m.group(2)) for r in recs[:k] for m in NAME.finditer(r) if m.group(1) == tid), default=-1)
            if os.path.exists(os.path.join(d, f"{tid}-{last + 1}")):
                ran.append(f"{log}: thread {tid}'s call {last + 1} ran ({rest!r})")
    return count, ran


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    killrace, out = os.path.abspath(argv[0]), Path(argv[1])
    out.mkdir(parents=True, exist_ok=True)
    version = subprocess.run(["strace", "-V"], capture_output=True, text=True).stdout.splitlines()[0]
    print(f"kernel {platform.release()}; {version}")
    args = tr.strace_args()
    total, ran, bad, shapes = 0, [], [], {}
    for mode in MODES:
        for i in range(1, RUNS + 1):
            d = tempfile.mkdtemp(prefix="kr.")
            log = str(out / f"{mode}-{i}.log")
            subprocess.run(["strace", "-DD", "-o", log, *args, killrace, d, "16", "30", mode],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            for _ in range(100):  # strace -DD runs detached: wait until it has written the log
                if not strace_alive():
                    break
                time.sleep(0.1)
            n, r = stubs(log, d)
            total += n
            ran += r
            buf = io.StringIO()
            with redirect_stdout(buf):
                tr.main([log, "--subtree", d])
            report = buf.getvalue()
            got = re.search(r"(?m)^trace: calls strace could not read at a dying thread's entry stop: (.*)$", report)
            passed = -1
            if got:
                passed = 0
                for part in got.group(1).split("; "):
                    if part != "none":
                        c, _, s = part.partition(" x ")
                        passed += int(c)
                        shapes[s] = shapes.get(s, 0) + int(c)
            lines = report.splitlines()
            unparsed = [f"{ln} {lines[k + 1].strip() if k + 1 < len(lines) else ''}"
                        for k, ln in enumerate(lines) if ln.startswith("UNPARSED ")]
            if passed != n or unparsed:
                bad.append(f"{log}: {n} stubs, the observer passed {passed}; " + " | ".join(unparsed[:6]))
            if n == 0:
                os.unlink(log)  # keep only the logs that show a stub
            else:
                with open(f"{log}.txt", "w") as f:
                    f.write(report)
            shutil.rmtree(d, ignore_errors=True)
    print(f"runs: {len(MODES) * RUNS} ({', '.join(MODES)}); stub calls: {total}; of them ran: {len(ran)}")
    print("shapes the observer passed: " + ("; ".join(f"{c} x {s}" for s, c in sorted(shapes.items())) or "none"))
    for line in ran + bad:
        print(f"::error::{line}")
    if total == 0:
        print("::error::no stub appeared in any run: this check proved nothing")
    return 1 if ran or bad or total == 0 else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
