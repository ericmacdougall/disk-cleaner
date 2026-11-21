#!/usr/bin/env python3
"""Planted actions for the write observer's negative controls (e2e/observer-controls.sh).

usage:
  python3 -B -I observer_controls.py ACTION CTL     run one action in the control folder CTL
  python3 -B -I observer_controls.py --list         each action and its expected result (EXPECT)
  python3 -B -I observer_controls.py declare CTL    the declarations of every control, one per line
  python3 -B -I observer_controls.py check-coverage CTL LOG
  python3 -B -I observer_controls.py record DEST ROOT LABEL=OUT...
      pack the results of observer-controls.sh runs (OUT, made where OUT was ROOT, as in a
      container with OUT=/out) into DEST, the recorded fixture test_trace_report.py replays

Each action makes exactly one change the observer must catch (or, for a "pass" control, only
declared changes, which it must pass). CTL is the control folder: CTL/sb is the declared
area (declarations()), everything else in CTL is outside it. The harness prepares the files
and descriptors an action expects (fd 3 is an inherited descriptor opened for appending to
CTL/outside-inherited.txt, fd 4 one opened read-write on CTL/outside-mapped.bin;
CTL/sb/declared-link is a symbolic link to CTL/outside-victim.txt; CTL/outside-emptydir is an
empty folder).
Linux only (but record); standard library only.
"""

from __future__ import annotations

import array
import ctypes
import json
import mmap
import os
import signal
import socket
import sys
import threading
import time

ACTIONS = {}


def action(f):
    ACTIONS[f.__name__.replace("_", "-")] = f
    return f


def libc():
    return ctypes.CDLL(None, use_errno=True)


@action
def positive(ctl: str) -> None:
    """Only declared changes, and reads: the observer must pass this."""
    sb = f"{ctl}/sb"
    with open(f"{sb}/declared.txt", "w") as f:  # an exact file
        f.write("ok")
    os.mkdir(f"{sb}/emptydir")  # an exact folder, mkdir only
    t = f"{sb}/list.xbel.Ab3dE9"  # a six-character temporary name, renamed over its file
    with open(t, "w") as f:
        f.write("x")
    os.rename(t, f"{sb}/list.xbel")
    os.makedirs(f"{sb}/subtree/a/b", exist_ok=True)  # a declared subtree
    with open(f"{sb}/subtree/a/b/c", "w") as f:
        f.write("y")
    os.symlink("c", f"{sb}/subtree/a/b/l")
    os.unlink(f"{sb}/subtree/a/b/l")
    os.write(1, b"stdout line\n")  # the harness's capture of standard output
    with open("/etc/hostname", "rb") as f:
        f.read()
    fd = os.open("/dev/null", os.O_WRONLY)
    os.write(fd, b"z")
    os.close(fd)
    r, w = os.pipe()  # anonymous descriptors
    os.write(w, b"p")
    os.read(r, 1)
    m = mmap.mmap(-1, 4096, flags=mmap.MAP_SHARED)  # anonymous shared memory
    m[0:1] = b"a"
    fd = os.open(f"{sb}/declared.txt", os.O_RDONLY)  # a read-only shared map
    m = mmap.mmap(fd, 0, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ)
    m.close()
    os.close(fd)


@action
def bind(ctl: str) -> None:
    """F11: a pathname Unix socket creates a filesystem entry without any open."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(f"{ctl}/outside.sock")


@action
def bind_abstract(ctl: str) -> None:
    """An abstract socket name creates no filesystem entry: this must pass."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(b"\0prunedisk-observer-control")


@action
def create_only(ctl: str) -> None:
    """A new empty file outside: an open that creates and nothing written through it."""
    os.close(os.open(f"{ctl}/outside-created.txt", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600))


@action
def truncate_open(ctl: str) -> None:
    """An outside file emptied by its open alone (O_TRUNC), with no write call."""
    os.close(os.open(f"{ctl}/outside-victim.txt", os.O_WRONLY | os.O_TRUNC))


@action
def unlink_outside(ctl: str) -> None:
    os.unlink(f"{ctl}/outside-victim.txt")


@action
def mkdir_outside(ctl: str) -> None:
    os.mkdir(f"{ctl}/outside-dir")


@action
def rmdir_outside(ctl: str) -> None:
    os.rmdir(f"{ctl}/outside-emptydir")


@action
def symlink_outside(ctl: str) -> None:
    os.symlink("/etc/hostname", f"{ctl}/outside-link")


@action
def inherited_write(ctl: str) -> None:
    """F11: a write through a descriptor inherited at exec, with no open in the trace."""
    os.write(3, b"inherited\n")


@action
def dup_write(ctl: str) -> None:
    """A write through a copy of an inherited descriptor (dup2 onto a new number)."""
    os.dup2(3, 9)
    os.write(9, b"dup2\n")


@action
def fcntl_dup_write(ctl: str) -> None:
    import fcntl

    fd = fcntl.fcntl(3, fcntl.F_DUPFD_CLOEXEC, 20)
    os.write(fd, b"F_DUPFD\n")


@action
def pwritev(ctl: str) -> None:
    os.pwritev(3, [b"a", b"b"], 0)


@action
def ftruncate_dup(ctl: str) -> None:
    fd = os.dup(3)
    os.ftruncate(fd, 0)


@action
def fallocate(ctl: str) -> None:
    os.posix_fallocate(3, 0, 4096)


@action
def mmap_shared_write(ctl: str) -> None:
    """A shared, writable map of an inherited read-write descriptor: writes reach the file
    with no further call."""
    m = mmap.mmap(4, 4096, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ | mmap.PROT_WRITE)
    m[0:5] = b"mmap!"
    m.flush()


@action
def mmap_shared_readonly_prot(ctl: str) -> None:
    """A shared map without PROT_WRITE of a descriptor that may write: mprotect can add
    PROT_WRITE later without a traced call that names the file."""
    m = mmap.mmap(4, 4096, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ)
    m.close()


# sendfile, copy_file_range and splice refuse a descriptor opened for appending: they write
# through fd 4 (read-write, no O_APPEND).
@action
def sendfile(ctl: str) -> None:
    src = os.open(f"{ctl}/outside-src.txt", os.O_RDONLY)
    os.sendfile(4, src, 0, 4)


@action
def copy_file_range(ctl: str) -> None:
    src = os.open(f"{ctl}/outside-src.txt", os.O_RDONLY)
    os.copy_file_range(src, 4, 4)


@action
def splice(ctl: str) -> None:
    r, w = os.pipe()
    os.write(w, b"splice")
    os.splice(r, 4, 6)


@action
def symlink_truncate(ctl: str) -> None:
    """F12: truncate through a declared name that is a link to a file outside."""
    os.truncate(f"{ctl}/sb/declared-link", 0)


@action
def symlink_chmod(ctl: str) -> None:
    os.chmod(f"{ctl}/sb/declared-link", 0o600)


@action
def symlink_lchown_passes(ctl: str) -> None:
    """lchown changes the link itself, a declared name: this must pass."""
    os.lchown(f"{ctl}/sb/declared-link", os.getuid(), os.getgid())


@action
def transient_symlink(ctl: str) -> None:
    """A link made inside a declared subtree, followed and removed during the run: gone
    before the report runs, so only the trace knows where it pointed."""
    link = f"{ctl}/sb/subtree/l"
    os.makedirs(f"{ctl}/sb/subtree", exist_ok=True)
    os.symlink(f"{ctl}/outside-victim.txt", link)
    os.truncate(link, 0)
    os.unlink(link)


@action
def transient_dir_symlink(ctl: str) -> None:
    """A link to a folder outside, as a middle component of the path written."""
    link = f"{ctl}/sb/subtree/d"
    os.makedirs(f"{ctl}/sb/subtree", exist_ok=True)
    os.symlink(ctl, link)
    with open(f"{link}/outside-through-dir.txt", "w") as f:
        f.write("x")
    os.unlink(link)


@action
def mkdir_only_write(ctl: str) -> None:
    """F15: a file under a folder declared as an empty folder (mkdir only)."""
    os.mkdir(f"{ctl}/sb/gtk-3.0")
    with open(f"{ctl}/sb/gtk-3.0/undisclosed.txt", "w") as f:
        f.write("x")


@action
def mkdir_only_chmod(ctl: str) -> None:
    os.mkdir(f"{ctl}/sb/gtk-3.0")
    os.chmod(f"{ctl}/sb/gtk-3.0", 0o777)


@action
def rename_in(ctl: str) -> None:
    """A rename into a declared name from outside: the outside folder loses an entry."""
    os.rename(f"{ctl}/outside-src.txt", f"{ctl}/sb/declared.txt")


@action
def rename_out(ctl: str) -> None:
    """A declared file renamed onto an undeclared name."""
    with open(f"{ctl}/sb/declared.txt", "w") as f:
        f.write("x")
    os.rename(f"{ctl}/sb/declared.txt", f"{ctl}/outside-dst.txt")


@action
def link_in(ctl: str) -> None:
    """A hard link of an outside file to a declared name, then a write through that name."""
    os.link(f"{ctl}/outside-victim.txt", f"{ctl}/sb/declared.txt")
    with open(f"{ctl}/sb/declared.txt", "a") as f:
        f.write("through a hard link")


@action
def tmpfile_link(ctl: str) -> None:
    """An unnamed file (O_TMPFILE) in an outside folder, then given a name there through
    /proc/self/fd (linkat with AT_EMPTY_PATH needs a capability). The folder is CTL_TMPFILE_DIR
    (a tmpfs: overlay file systems refuse O_TMPFILE), or CTL."""
    d = os.environ.get("CTL_TMPFILE_DIR", ctl)
    fd = os.open(d, os.O_TMPFILE | os.O_WRONLY, 0o600)
    os.write(fd, b"tmpfile")
    # linkat(AT_FDCWD, "/proc/self/fd/N", AT_FDCWD, name, AT_SYMLINK_FOLLOW), as open(2) says
    if libc().linkat(-100, f"/proc/self/fd/{fd}".encode(), -100, f"{d}/outside-tmpfile.txt".encode(), 0x400) != 0:
        raise OSError(ctypes.get_errno(), "linkat")


@action
def proc_fd_reopen(ctl: str) -> None:
    """Reopening an inherited descriptor for writing through /proc/self/fd."""
    fd = os.open("/proc/self/fd/3", os.O_WRONLY | os.O_APPEND)
    os.write(fd, b"proc\n")


@action
def proc_fd_truncate(ctl: str) -> None:
    os.truncate("/proc/self/fd/3", 0)


@action
def scm_rights(ctl: str) -> None:
    """A descriptor passed over a socket, then written through on the receiving side."""
    a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    a.sendmsg([b"x"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [3]))])
    msg, anc, flags, addr = b.recvmsg(1, socket.CMSG_LEN(4))
    fd = array.array("i", anc[0][2])[0]
    os.close(3)
    os.write(fd, b"scm\n")


@action
def utimensat(ctl: str) -> None:
    os.utime(f"{ctl}/outside-victim.txt", (0, 0))


@action
def setxattr(ctl: str) -> None:
    os.setxattr(f"{ctl}/outside-victim.txt", "user.prunedisk", b"1")


@action
def mknod_fifo(ctl: str) -> None:
    os.mkfifo(f"{ctl}/outside.fifo")


@action
def fork_write(ctl: str) -> None:
    """A child process writes; its parent only waits."""
    pid = os.fork()
    if pid == 0:
        os.write(3, b"child\n")
        os._exit(0)
    os.waitpid(pid, 0)


@action
def thread_write(ctl: str) -> None:
    t = threading.Thread(target=lambda: os.write(3, b"thread\n"))
    t.start()
    t.join()


@action
def exec_write(ctl: str) -> None:
    """A program the app runs writes through the descriptor it inherits."""
    os.execv("/bin/sh", ["sh", "-c", "echo exec >&3"])


@action
def relative_write(ctl: str) -> None:
    os.chdir(ctl)
    with open("outside-relative.txt", "w") as f:
        f.write("x")


FIRST_PROBED = 334  # x86_64: every number below is in the tables of every strace these checks use


@action
def syscall_coverage(ctl: str) -> None:
    """Every system call number the kernel has, from FIRST_PROBED to 1023, is seen by the trace
    (with --seccomp-bpf, strace lets through numbers its tables do not cover). Each is called once, in a
    child of its own, between two marker writes to /dev/null, with arguments that fail harmlessly
    (-7, then zeros); CTL/coverage.json records each number's child, errno or signal for
    `check-coverage`."""
    out = {}
    null = os.open("/dev/null", os.O_WRONLY)
    lib = libc()
    for nr in range(FIRST_PROBED, 1024):
        r, w = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(r)
            signal.alarm(2)
            os.write(null, f"prunedisk-coverage-before {nr}".encode())
            res = lib.syscall(nr, -7, 0, 0, 0, 0, 0)
            err = ctypes.get_errno() if res == -1 else 0
            os.write(null, f"prunedisk-coverage-after {nr}".encode())
            os.write(w, str(err).encode())
            os._exit(0)
        os.close(w)
        got = os.read(r, 16)
        os.close(r)
        _, status = os.waitpid(pid, 0)
        out[nr] = {"pid": pid, "errno": int(got) if got else None,
                   "signal": os.WTERMSIG(status) if os.WIFSIGNALED(status) else None}
    with open(f"{ctl}/coverage.json", "w") as f:
        json.dump(out, f)


def check_coverage(ctl: str, log: str) -> int:
    """Each probed number the kernel has (its call did not fail with ENOSYS), but those of HOT,
    has a traced call between its child's two markers. A container's own seccomp profile refuses
    some numbers (EPERM) before any tracer: those count as the kernel's, so they must be seen too
    (ptrace stops before the profile refuses them)."""
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import trace_report  # noqa: E402  (HOT: the calls strace is told not to stop for)

    hot = set(trace_report.HOT)
    probes = json.load(open(f"{ctl}/coverage.json"))
    lines: dict[str, list[str]] = {}
    for line in open(log, errors="replace"):
        pid, _, rest = line.partition(" ")
        lines.setdefault(pid, []).append(rest.strip())
    implemented, missing = 0, []
    for nr, p in sorted(probes.items(), key=lambda x: int(x[0])):
        if p["errno"] == 38 and p["signal"] is None:  # ENOSYS: the kernel has no such call
            continue
        if trace_report.KERNEL_TABLE.get(int(nr)) in hot:  # left untraced on purpose (changes nothing)
            continue
        implemented += 1
        seen, inside = False, False
        for text in lines.get(str(p["pid"]), []):
            if f"prunedisk-coverage-before {nr}" in text:
                inside = True
            elif f"prunedisk-coverage-after {nr}" in text:
                break
            elif inside and not text.startswith(("+++", "---", "<... write resumed>")):
                seen = True
        if not seen:
            missing.append(f"{nr} (errno {p['errno']}, signal {p['signal']})")
    print(f"coverage: {len(probes)} numbers probed from {FIRST_PROBED}; {implemented} the kernel has; "
          f"{len(missing)} not seen by the trace")
    for m in missing:
        print(f"NOT TRACED: system call {m}")
    return 1 if missing or not implemented else 0


@action
def unknown_syscall(ctl: str) -> None:
    """A call no Linux has: strace prints it by number; the observer cannot vouch for it."""
    libc().syscall(1000)


@action
def slow_write(ctl: str) -> None:
    """Writes after the harness has killed strace (its processes then run on untraced): the
    observer must say it lost this process."""
    time.sleep(4)
    with open(f"{ctl}/outside-after-detach.txt", "w") as f:
        f.write("unseen")


@action
def leave_child(ctl: str) -> None:
    """A child that outlives the program, as a daemon would: its own session, deaf to SIGTERM
    and SIGHUP. The survivor check (e2e/traced.py) must catch it and kill it."""
    if os.fork() == 0:
        os.setsid()
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        time.sleep(60)
        with open(f"{ctl}/outside-after-close.txt", "w") as f:
            f.write("written after the session")
        os._exit(0)


@action
def ask_service(ctl: str) -> None:
    """Asks a process that is not a child (e2e/observer-controls.sh starts it beside the
    trace, as D-Bus starts dconf-service) to write a file in the sandbox. The trace sees only
    a connection; the snapshot must see the file."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    for _ in range(50):
        try:
            s.connect(b"\0prunedisk-observer-service-" + os.path.basename(ctl).encode())
            break
        except OSError:
            time.sleep(0.1)
    s.sendall(b"write\n")
    s.recv(16)


def service(ctl: str) -> None:
    """The non-child writer for ask-service: one request, then exit."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(b"\0prunedisk-observer-service-" + os.path.basename(ctl).encode())
    s.listen(1)
    c, _ = s.accept()
    c.recv(16)
    with open(f"{ctl}/sb/home/written-by-a-service.txt", "w") as f:
        f.write("not the app's process")
    c.sendall(b"done\n")


def declarations(ctl: str) -> list[str]:
    """What every control declares (trace_report.py options): an exact file, a folder that may
    only be created, a six-character temporary, a subtree, a declared name that is a link to a
    file outside, the harness's capture of standard output and the null device."""
    sb = f"{ctl}/sb"
    return ["--file", f"{sb}/declared.txt", "--dir", f"{sb}/emptydir", "--file", f"{sb}/list.xbel",
            "--file", f"{sb}/list.xbel.{{XXXXXX}}", "--subtree", f"{sb}/subtree", "--file", f"{sb}/declared-link",
            "--dir", f"{sb}/gtk-3.0", "--stdio", f"{ctl}/stdout.log", "--device", "/dev/null"]


# What each control must produce: "pass" (every check passes), "TRACE <KIND> <path>" (a line of
# the trace report with that kind and path; CTL is the control folder, TMP the O_TMPFILE
# folder), "TRACE-PREFIX <text>", "SNAPSHOT <line of the snapshot report>", "SURVIVOR <name>"
# (traced.py's), "COVERAGE" (check-coverage). test_trace_report.py replays the recorded runs
# against the same table.
EXPECT = {
    "positive": "pass",
    "bind-abstract": "pass",
    "symlink-lchown-passes": "pass",
    "bind": "TRACE OTHER CTL/outside.sock",
    "create-only": "TRACE OTHER CTL/outside-created.txt",
    "truncate-open": "TRACE OTHER CTL/outside-victim.txt",
    "unlink-outside": "TRACE OTHER CTL/outside-victim.txt",
    "mkdir-outside": "TRACE OTHER CTL/outside-dir",
    "rmdir-outside": "TRACE OTHER CTL/outside-emptydir",
    "symlink-outside": "TRACE OTHER CTL/outside-link",
    "inherited-write": "TRACE OTHER CTL/outside-inherited.txt",
    "dup-write": "TRACE OTHER CTL/outside-inherited.txt",
    "fcntl-dup-write": "TRACE OTHER CTL/outside-inherited.txt",
    "pwritev": "TRACE OTHER CTL/outside-inherited.txt",
    "ftruncate-dup": "TRACE OTHER CTL/outside-inherited.txt",
    "fallocate": "TRACE OTHER CTL/outside-inherited.txt",
    "mmap-shared-write": "TRACE OTHER CTL/outside-mapped.bin",
    "mmap-shared-readonly-prot": "TRACE OTHER CTL/outside-mapped.bin",
    "sendfile": "TRACE OTHER CTL/outside-mapped.bin",
    "copy-file-range": "TRACE OTHER CTL/outside-mapped.bin",
    "splice": "TRACE OTHER CTL/outside-mapped.bin",
    "symlink-truncate": "TRACE OTHER CTL/outside-victim.txt",
    "symlink-chmod": "TRACE OTHER CTL/outside-victim.txt",
    "transient-symlink": "TRACE OTHER CTL/outside-victim.txt",
    "transient-dir-symlink": "TRACE OTHER CTL/outside-through-dir.txt",
    "mkdir-only-write": "TRACE OTHER CTL/sb/gtk-3.0/undisclosed.txt",
    "mkdir-only-chmod": "TRACE OTHER CTL/sb/gtk-3.0",
    "rename-in": "TRACE OTHER CTL/outside-src.txt",
    "rename-out": "TRACE OTHER CTL/outside-dst.txt",
    "link-in": "TRACE OTHER CTL/outside-victim.txt",
    "tmpfile-link": "TRACE OTHER TMP/outside-tmpfile.txt",
    "proc-fd-reopen": "TRACE OTHER CTL/outside-inherited.txt",
    "proc-fd-truncate": "TRACE-PREFIX UNRESOLVED truncate: /proc/self/fd/3 ",
    "scm-rights": "TRACE OTHER CTL/outside-inherited.txt",
    "utimensat": "TRACE OTHER CTL/outside-victim.txt",
    "setxattr": "TRACE OTHER CTL/outside-victim.txt",
    "mknod-fifo": "TRACE OTHER CTL/outside.fifo",
    "fork-write": "TRACE OTHER CTL/outside-inherited.txt",
    "thread-write": "TRACE OTHER CTL/outside-inherited.txt",
    "exec-write": "TRACE OTHER CTL/outside-inherited.txt",
    "relative-write": "TRACE OTHER CTL/outside-relative.txt",
    "syscall-coverage": "COVERAGE",
    "unknown-syscall": "TRACE-PREFIX UNKNOWN syscall_0x3e8: ",
    "slow-write": "TRACE-PREFIX LOST trace: process ",
    "leave-child": "SURVIVOR python3",
    "ask-service": "SNAPSHOT UNDECLARED added: CTL/sb/home/written-by-a-service.txt",
}

# Not in the recorded fixture: the coverage log is large and needs the probes' results, and a
# survivor is traced.py's verdict, not the trace's. CI runs both live on every leg.
NOT_RECORDED = ("syscall-coverage", "leave-child")


def record(dest: str, root: str, runs: list[str]) -> int:
    """{label: {"strace": version line, "actions": {action: {trace.log, before.json,
    after.json}}}} with the control folder as @CTL@, the O_TMPFILE folder as @TMP@ and the
    run folder as @RUN@, compressed with xz."""
    import lzma
    import re

    out_all = {}
    for spec in runs:
        label, _, out = spec.partition("=")
        first = open(f"{out}/controls.txt").readline().strip()
        fix = {"strace": first, "actions": {}}
        for a in EXPECT:
            if a in NOT_RECORDED:
                continue
            entry = {}
            for name in ("trace.log", "before.json", "after.json"):
                t = open(f"{out}/run-{a}/{name}", errors="surrogateescape").read()
                t = re.sub(r"/dev/shm/prunedisk-ctl-\d+-" + re.escape(a) + r"(?![\w-])", "@TMP@", t)
                t = re.sub(re.escape(f"{root}/ctl-{a}") + r"(?![\w-])", "@CTL@", t)
                t = re.sub(re.escape(f"{root}/run-{a}") + r"(?![\w-])", "@RUN@", t)
                left = re.search(re.escape(root) + r"(?=[/\"<>,\s])", t)
                if left:
                    print(f"{label} {a}/{name}: a path under {root} is left: {t[max(0, left.start() - 60):left.end() + 60]!r}",
                          file=sys.stderr)
                    return 1
                entry[name] = t
            fix["actions"][a] = entry
        out_all[label] = fix
    raw = json.dumps(out_all, sort_keys=True, indent=0).encode("utf-8", "surrogateescape")
    with open(dest, "wb") as f:
        f.write(lzma.compress(raw, preset=9 | lzma.PRESET_EXTREME))
    labels = ", ".join(f"{k} ({v['strace']})" for k, v in out_all.items())
    print(f"{dest}: {labels}")
    return 0


def main(argv: list[str]) -> int:
    if set(EXPECT) != set(ACTIONS):
        print(f"observer_controls.py: EXPECT and the actions differ: {sorted(set(EXPECT) ^ set(ACTIONS))}", file=sys.stderr)
        return 2
    if len(argv) == 2 and argv[0] == "service":
        service(argv[1])
        return 0
    if len(argv) == 3 and argv[0] == "check-coverage":
        return check_coverage(argv[1], argv[2])
    if len(argv) == 2 and argv[0] == "declare":
        print("\n".join(declarations(argv[1])))
        return 0
    if len(argv) >= 4 and argv[0] == "record":
        return record(argv[1], argv[2].rstrip("/"), argv[3:])
    if len(argv) == 1 and argv[0] == "--list":
        print("\n".join(f"{a}\t{EXPECT[a]}" for a in ACTIONS))
        return 0
    if len(argv) != 2 or argv[0] not in ACTIONS:
        print(__doc__, file=sys.stderr)
        return 2
    ACTIONS[argv[0]](argv[1])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
