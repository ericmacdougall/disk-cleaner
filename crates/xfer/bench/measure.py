#!/usr/bin/env python3
"""Run a command and report wall time, CPU seconds and peak RSS of its whole process tree.

usage: measure.py OUT.json -- CMD [ARGS...]

- wall: monotonic seconds from spawn to exit
- cpu: user+sys of every waited-for descendant (RUSAGE_CHILDREN delta)
- peak_rss: the largest sum of VmRSS over the tree, sampled every 0.1 s from /proc (Linux)
  or `ps` (macOS). A tree-wide sum, so the engine and its rclone rcd count together.
- disk: optional peak of `du -sk` over --watch DIR (repeatable), sampled every 0.5 s
"""
import json
import os
import resource
import subprocess
import sys
import threading
import time


def tree_rss_linux(root):
    kids = {}
    rss = {}
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/stat", "rb") as f:
                s = f.read().decode("latin-1")
            ppid = int(s[s.rfind(")") + 2 :].split()[1])
            with open(f"/proc/{d}/statm") as f:
                pages = int(f.read().split()[1])
        except (OSError, ValueError, IndexError):
            continue
        kids.setdefault(ppid, []).append(int(d))
        rss[int(d)] = pages * os.sysconf("SC_PAGE_SIZE")
    total, stack = 0, [root]
    while stack:
        p = stack.pop()
        total += rss.get(p, 0)
        stack.extend(kids.get(p, []))
    return total


def tree_rss_ps(root):
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,rss="], capture_output=True, text=True).stdout
    kids, rss = {}, {}
    for line in out.splitlines():
        try:
            pid, ppid, kb = (int(x) for x in line.split())
        except ValueError:
            continue
        kids.setdefault(ppid, []).append(pid)
        rss[pid] = kb * 1024
    total, stack = 0, [root]
    while stack:
        p = stack.pop()
        total += rss.get(p, 0)
        stack.extend(kids.get(p, []))
    return total


def du_kib(path):
    r = subprocess.run(["du", "-sk", path], capture_output=True, text=True)
    try:
        return int(r.stdout.split()[0])
    except (ValueError, IndexError):
        return 0


def main():
    args = sys.argv[1:]
    out = args.pop(0)
    watch = []
    while args and args[0] == "--watch":
        args.pop(0)
        watch.append(args.pop(0))
    if not args or args.pop(0) != "--":
        sys.exit("usage: measure.py OUT.json [--watch DIR]... -- CMD...")
    sampler = tree_rss_linux if os.path.isdir("/proc/self") else tree_rss_ps
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.monotonic()
    p = subprocess.Popen(args)
    peak = [0]
    dpeak = {w: 0 for w in watch}
    done = threading.Event()

    def sample():
        last_du = 0.0
        while not done.is_set():
            try:
                peak[0] = max(peak[0], sampler(p.pid))
            except Exception:
                pass
            now = time.monotonic()
            if watch and now - last_du >= 0.5:
                for w in watch:
                    dpeak[w] = max(dpeak[w], du_kib(w))
                last_du = now
            done.wait(0.1)

    th = threading.Thread(target=sample, daemon=True)
    th.start()
    rc = p.wait()
    wall = time.monotonic() - t0
    done.set()
    th.join()
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
    rec = {"rc": rc, "wall_s": round(wall, 3), "cpu_s": round(cpu, 3), "peak_rss": peak[0]}
    if watch:
        rec["disk_peak_kib"] = dpeak
    with open(out, "w") as f:
        json.dump(rec, f)
    sys.exit(rc)


if __name__ == "__main__":
    main()
