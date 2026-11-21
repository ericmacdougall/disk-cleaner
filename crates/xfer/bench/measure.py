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
    return split(root, kids, rss)


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
    return split(root, kids, rss)


def split(root, kids, rss):
    """(whole tree, the root process alone) in bytes."""
    total, stack = 0, [root]
    while stack:
        p = stack.pop()
        total += rss.get(p, 0)
        stack.extend(kids.get(p, []))
    return total, rss.get(root, 0)


def swap_used():
    """Bytes of swap in use (macOS `sysctl vm.swapusage`; /proc/meminfo on Linux)."""
    try:
        if os.path.isfile("/proc/meminfo"):
            m = dict(l.split(":", 1) for l in open("/proc/meminfo"))
            return (int(m["SwapTotal"].split()[0]) - int(m["SwapFree"].split()[0])) * 1024
        out = subprocess.run(["sysctl", "-n", "vm.swapusage"], capture_output=True, text=True).stdout
        used = out.split("used = ")[1].split()[0]
        return int(float(used.rstrip("M")) * 1024 * 1024)
    except (OSError, ValueError, IndexError, KeyError):
        return -1


def du_kib(path):
    r = subprocess.run(["du", "-sk", path], capture_output=True, text=True)
    try:
        return int(r.stdout.split()[0])
    except (ValueError, IndexError):
        return 0


def main():
    args = sys.argv[1:]
    out = args.pop(0)
    watch, dfs = [], []
    while args and args[0] in ("--watch", "--df"):
        (watch if args.pop(0) == "--watch" else dfs).append(args.pop(0))
    if not args or args.pop(0) != "--":
        sys.exit("usage: measure.py OUT.json [--watch DIR]... [--df PATH]... -- CMD...")
    sampler = tree_rss_linux if os.path.isdir("/proc/self") else tree_rss_ps
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    free = lambda d: os.statvfs(d).f_bavail * os.statvfs(d).f_frsize
    df0 = {d: free(d) for d in dfs}
    swap0 = swap_used()
    t0 = time.monotonic()
    p = subprocess.Popen(args)
    # peak of the tree, of the root process (the engine) and of the rest (rclone rcd)
    peak = [0, 0, 0]
    dpeak = {w: 0 for w in watch}
    dfmin = dict(df0)
    swapmax = [swap0]
    done = threading.Event()

    def sample():
        last_du = 0.0
        while not done.is_set():
            try:
                total, root = sampler(p.pid)
                peak[0] = max(peak[0], total)
                peak[1] = max(peak[1], root)
                peak[2] = max(peak[2], total - root)
            except Exception:
                pass
            now = time.monotonic()
            if (watch or dfs) and now - last_du >= 0.5:
                for w in watch:
                    dpeak[w] = max(dpeak[w], du_kib(w))
                for d in dfs:
                    dfmin[d] = min(dfmin[d], free(d))
                swapmax[0] = max(swapmax[0], swap_used())
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
    rec = {
        "rc": rc,
        "wall_s": round(wall, 3),
        "cpu_s": round(cpu, 3),
        "peak_rss": peak[0],
        "peak_rss_engine": peak[1],
        "peak_rss_children": peak[2],
        # ru_maxrss of the waited-for tree: KiB on Linux, bytes on macOS
        "maxrss_children": after.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
    }
    if watch:
        rec["disk_peak_kib"] = dpeak
    if dfs:
        rec["df_drop_bytes"] = {d: df0[d] - dfmin[d] for d in dfs}
    rec["swap_growth_bytes"] = max(0, swapmax[0] - swap0) if swap0 >= 0 else None
    with open(out, "w") as f:
        json.dump(rec, f)
    sys.exit(rc)


if __name__ == "__main__":
    main()
