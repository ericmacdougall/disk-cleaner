#!/usr/bin/env python3
"""The Linux write observer of the end-to-end checks (PR gate F10; PR review F11, F12, F15).

It proves that every change the app's processes made to any file, anywhere on the machine, is
one the app discloses (src/writes.ts, through the typed declarations of e2e/declarations.py).
Its default is failure. Each of these fails the check:
  - a system call it does not know (UNKNOWN);
  - a call whose effects it cannot judge (UNSUPPORTED): io_uring, Linux AIO, ptrace,
    process_vm_writev, a seccomp filter, a new mount namespace or root, mounts, mqueues,
    keyrings and the like;
  - a line it cannot read (UNPARSED);
  - a path or descriptor whose object it cannot establish (UNRESOLVED);
  - a process the trace lost before it ended (LOST), or a log that does not start with the
    program's execve (LATE);
  - a change no declaration allows (OTHER).

usage:
  trace_report.py LOG... DECLARATIONS [--links-from SNAPSHOT]
      judge strace logs made with the options --strace-args prints
  trace_report.py --snapshot BEFORE AFTER DECLARATIONS
      judge the difference between two e2e/manifest.py snapshots (the second, independent check:
      it sees writes by processes the trace does not follow, such as dconf-service)
  trace_report.py --strace-args
      the strace options of every traced launch, one per line

DECLARATIONS, each an exact path; a path may hold {XXXXXX} (exactly six letters or digits),
{PID} (the writing process's id), {N} (digits), {STAMP} (YYYYMMDD-HHMMSS) and {SEQ} (nothing,
or a dash and digits):
  --file PATH     a regular file: created, written, truncated, its metadata changed, removed,
                  renamed onto or away, hard-linked to; and a shared writable map of it
  --dir PATH      a folder that may be created (mkdir) and nothing else: no entry inside it
  --socket PATH   a Unix socket: bound, its mode changed, removed
  --subtree PATH  a folder that is wholly a library's data (src/writes.ts says so): anything in it
  --stdio PATH    the harness's capture of standard output: writes through a descriptor only
  --device PATH   a device such as /dev/null: opened for writing, written, mapped, ioctl
  --declared-temp PATH  the same as --file with a trailing XXXXXX read as {XXXXXX}
  --profile NAME --root VAR=PATH...   a profile of e2e/declarations.py
The folders on the way to a declared path may be created (mkdir only).
--links-from SNAPSHOT: an e2e/manifest.py snapshot taken before the run; its links seed what
the observer knows about symbolic links that were there before the trace started.

Exit 0 when every change is declared, 1 when anything above fails, 2 on bad usage or an
unreadable file.
"""

from __future__ import annotations

import json
import os
import re
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path

# ----------------------------------------------------------------------------------------------
# Every Linux x86_64 system call, by what it can do to files. syscalls-x86_64-linux-6.8.txt is
# the kernel's own table (linux-libc-dev 6.8); test_trace_report.py fails if a name in it, or in
# the build host's unistd_64.h, is missing here. Names newer than 6.8 are listed too.

# No effect on any file and no descriptor made: reads, waits, clocks, signals, credentials,
# memory (mprotect cannot make a shared map writable unless the descriptor was opened for
# writing, which the map itself is judged by), SysV IPC memory, socket options, syncs, locks.
NONE = set("""
read pread64 readv preadv preadv2 recvfrom stat fstat lstat newfstatat statx statfs fstatfs
ustat access faccessat faccessat2 readlink readlinkat getdents getdents64 getcwd lseek poll
ppoll select pselect6 epoll_wait epoll_pwait epoll_pwait2 epoll_ctl futex futex_waitv
futex_wake futex_wait futex_requeue get_robust_list set_robust_list nanosleep clock_nanosleep
clock_gettime clock_getres gettimeofday time times getitimer setitimer alarm pause sched_yield
getpid getppid gettid getuid geteuid getgid getegid getresuid getresgid getgroups getpgrp
getpgid getsid setuid setgid setreuid setregid setresuid setresgid setfsuid setfsgid setgroups
setpgid setsid capget capset getrlimit setrlimit prlimit64 getrusage sysinfo uname getcpu
getpriority setpriority sched_setparam sched_getparam sched_setscheduler sched_getscheduler
sched_get_priority_max sched_get_priority_min sched_rr_get_interval sched_setaffinity
sched_getaffinity sched_setattr sched_getattr ioprio_set ioprio_get rt_sigaction
rt_sigprocmask rt_sigreturn rt_sigpending rt_sigtimedwait rt_sigqueueinfo rt_tgsigqueueinfo
rt_sigsuspend sigaltstack kill tkill tgkill pidfd_send_signal restart_syscall wait4 waitid
exit exit_group brk mprotect pkey_mprotect pkey_alloc pkey_free munmap mremap msync mincore
madvise process_madvise mlock mlock2 munlock mlockall munlockall membarrier mbind
set_mempolicy get_mempolicy set_mempolicy_home_node migrate_pages move_pages remap_file_pages
map_shadow_stack mseal rseq set_tid_address arch_prctl set_thread_area get_thread_area
modify_ldt personality getrandom shmget shmat shmdt shmctl semget semop semtimedop semctl
msgget msgsnd msgrcv msgctl connect listen shutdown getsockname getpeername getsockopt
setsockopt sendto sendmsg sendmmsg mq_timedsend mq_timedreceive mq_notify mq_getsetattr
fsync fdatasync sync syncfs sync_file_range fadvise64 readahead flock getxattr lgetxattr
fgetxattr listxattr llistxattr flistxattr getxattrat listxattrat file_getattr
inotify_add_watch inotify_rm_watch fanotify_mark name_to_handle_at timer_create
timer_settime timer_gettime timer_getoverrun timer_delete timerfd_settime timerfd_gettime
io_getevents io_pgetevents io_cancel io_destroy kcmp process_vm_readv cachestat statmount
listmount lsm_get_self_attr lsm_list_modules landlock_add_rule landlock_restrict_self
process_mrelease tee vmsplice umask sysfs uretprobe uprobe
""".split())
# sendto, sendmsg and sendmmsg work only on sockets; tee and vmsplice only on pipes.

# Make a descriptor that names no file (or whose file the kernel names in the result).
ANON_FD = set("""
socket accept accept4 eventfd eventfd2 signalfd signalfd4 timerfd_create epoll_create
epoll_create1 inotify_init inotify_init1 fanotify_init memfd_create memfd_secret userfaultfd
perf_event_open pidfd_open landlock_create_ruleset
""".split())
PAIRS = {"pipe": 0, "pipe2": 0, "socketpair": 3}  # the argument holding the two new descriptors
OPENS = {"open": (None, 0, 1), "openat": (0, 1, 2), "openat2": (0, 1, 2), "creat": (None, 0, None),
         "open_by_handle_at": (None, None, 2)}  # (dirfd arg, path arg, flags arg)

# Change the object a descriptor refers to: (descriptor argument, what is done).
FD_WRITE = {
    "write": (0, "write"), "writev": (0, "write"), "pwrite64": (0, "write"), "pwritev": (0, "write"),
    "pwritev2": (0, "write"), "sendfile": (0, "write"), "copy_file_range": (2, "write"),
    "splice": (2, "write"), "ftruncate": (0, "write"), "fallocate": (0, "write"),
    "fchmod": (0, "meta"), "fchown": (0, "meta"), "fsetxattr": (0, "meta"), "fremovexattr": (0, "meta"),
}

# Change what a path names, each with its own way of following a final symbolic link.
PATH_CALLS = set("""
mkdir mkdirat mknod mknodat rename renameat renameat2 link linkat symlink symlinkat unlink
unlinkat rmdir truncate chmod fchmodat fchmodat2 chown lchown fchownat utime utimes utimensat
futimesat setxattr lsetxattr removexattr lremovexattr setxattrat removexattrat file_setattr bind
""".split())

# Descriptor table, current folder and process tree, or decided by their arguments.
TABLE = set("""
close close_range dup dup2 dup3 fcntl ioctl chdir fchdir clone clone3 fork vfork execve
execveat unshare recvmsg recvmmsg pidfd_getfd mmap prctl seccomp
""".split())

# Effects the trace cannot follow or the paths cannot be compared after: these fail whenever
# they succeed.
UNSUPPORTED = {
    **{c: "io_uring performs file operations without system calls the trace sees"
       for c in ("io_uring_setup", "io_uring_enter", "io_uring_register")},
    **{c: "Linux AIO writes files without a system call the trace sees" for c in ("io_setup", "io_submit")},
    "ptrace": "a traced process controlling another",
    "process_vm_writev": "writes into another process's memory",
    "bpf": "a BPF program",
    **{c: "the file system tree changes (mount, root or namespace)" for c in (
        "setns", "pivot_root", "chroot", "mount", "umount2", "move_mount", "fsopen", "fsconfig",
        "fsmount", "fspick", "open_tree", "open_tree_attr", "mount_setattr")},
    **{c: "a POSIX message queue file" for c in ("mq_open", "mq_unlink")},
    **{c: "the kernel's key store" for c in ("add_key", "request_key", "keyctl")},
    **{c: "system state outside files" for c in (
        "swapon", "swapoff", "reboot", "kexec_load", "kexec_file_load", "init_module", "finit_module",
        "delete_module", "create_module", "query_module", "get_kernel_syms", "acct", "quotactl",
        "quotactl_fd", "settimeofday", "clock_settime", "clock_adjtime", "adjtimex", "sethostname",
        "setdomainname", "iopl", "ioperm", "vhangup", "_sysctl", "uselib", "nfsservctl", "getpmsg",
        "putpmsg", "afs_syscall", "tuxcall", "security", "vserver", "lookup_dcookie",
        "epoll_ctl_old", "epoll_wait_old", "lsm_set_self_attr", "syslog")},
}

KNOWN = NONE | ANON_FD | set(PAIRS) | set(OPENS) | set(FD_WRITE) | PATH_CALLS | TABLE | set(UNSUPPORTED)


def kernel_table() -> dict[int, str]:
    """number -> name, from the kernel's own x86_64 table beside this file (linux-libc-dev)."""
    out = {}
    try:
        with open(Path(__file__).resolve().parent / "syscalls-x86_64-linux-6.8.txt") as f:
            for line in f:
                name, nr = line.split()
                out[int(nr)] = name
    except OSError:
        pass
    return out


KERNEL_TABLE = kernel_table()

# Calls strace leaves out of the log, because they are frequent and in NONE: they create no
# descriptor and change no file, folder or process. test_trace_report.py checks this.
HOT = sorted("""
read pread64 readv preadv preadv2 recvfrom poll ppoll select pselect6 epoll_wait epoll_pwait
epoll_pwait2 futex futex_waitv futex_wake futex_wait nanosleep clock_nanosleep clock_gettime
gettimeofday time sched_yield getpid gettid getppid getuid geteuid getgid getegid
rt_sigprocmask rt_sigaction rt_sigreturn sigaltstack mprotect madvise munmap brk mremap
membarrier getrandom lseek fstat newfstatat stat lstat statx access faccessat faccessat2
readlink readlinkat getdents64 getcwd sendmsg sendto sendmmsg flock fadvise64 set_robust_list
rseq sched_getaffinity sched_setaffinity getsockname getpeername getsockopt setsockopt
prlimit64 uname sysinfo statfs fstatfs shmget shmat shmdt shmctl
""".split())


def strace_args() -> list[str]:
    """-f follow every child; -q but not -qq, so each process's exit is in the log (LOST needs
    it); -y decode descriptors to the objects they refer to; -ttt time stamps (logs of several
    launches are judged in time order); every call but HOT in the log ("?": a name this strace
    does not know is no error). No --seccomp-bpf: with it, strace stops only for the system
    calls its own tables cover, and strace 5.16 (Ubuntu 22.04) and 6.1 (Debian 12) on Linux 6.8
    let set_mempolicy_home_node or cachestat up to lsm_list_modules (450 or 451 to 461) run
    unseen, fchmodat2 among them (observer-controls.sh, syscall-coverage). Without it every
    call stops, and one strace has no table for is printed by number."""
    return ["-f", "-q", "-y", "-ttt", "-s", "64", "-e", "trace=!" + ",".join("?" + c for c in HOT)]


# ----------------------------------------------------------------------------------------------
# Declarations

KINDS = ("file", "dir", "socket", "subtree", "stdio", "device")
OPS_ALLOWED = {
    "file": {"create", "write", "meta", "unlink", "rename-from", "rename-to", "link-to", "map"},
    "dir": {"mkdir"},
    "socket": {"bind", "meta", "unlink"},
    "stdio": {"write"},
    # A device exists: an open with O_CREAT of it creates nothing (only mknod makes one).
    "device": {"create", "write", "map", "ioctl"},
}
ALL_OPS = {"create", "write", "meta", "unlink", "rename-from", "rename-to", "link-to", "map", "mkdir",
           "rmdir", "symlink", "bind", "mknod", "ioctl"}
OPS_ALLOWED["subtree"] = ALL_OPS
PLACEHOLDERS = {"{XXXXXX}": r"[A-Za-z0-9]{6}", "{PID}": r"(?P<pid>[0-9]+)", "{N}": r"[0-9]+",
                "{STAMP}": r"[0-9]{8}-[0-9]{6}", "{SEQ}": r"(?:-[0-9]+)?"}
PLACEHOLDER = re.compile("|".join(re.escape(p) for p in PLACEHOLDERS))


def canonical_prefix(path: str) -> str:
    """The path with the folder holding its first component that has a placeholder (or its last
    component) resolved, as the harness made it; the rest is taken as written."""
    path = os.path.normpath(path)
    parts = path.split("/")
    for i, part in enumerate(parts):
        if PLACEHOLDER.search(part):
            break
    else:
        i = len(parts) - 1
    head = "/".join(parts[:i]) or "/"
    return os.path.join(os.path.realpath(head), *parts[i:]) if i < len(parts) else os.path.realpath(head)


@dataclass
class Decl:
    kind: str
    path: str
    regex: re.Pattern[str] = field(init=False)

    def __post_init__(self) -> None:
        if self.kind not in KINDS:
            raise ValueError(f"unknown declaration kind {self.kind}")
        if not self.path.startswith("/"):
            raise ValueError(f"a declaration must be an absolute path: {self.path}")
        self.path = canonical_prefix(self.path)
        if self.kind == "subtree" and (PLACEHOLDER.search(self.path) or self.path == "/"):
            raise ValueError(f"a subtree must be a fixed folder: {self.path}")
        out, last = [], 0
        for m in PLACEHOLDER.finditer(self.path):
            out.append(re.escape(self.path[last:m.start()]))
            out.append(PLACEHOLDERS[m.group(0)])
            last = m.end()
        out.append(re.escape(self.path[last:]))
        self.regex = re.compile("".join(out))

    def matches(self, path: str, tgid: str | None) -> bool:
        if self.kind == "subtree":
            return path == self.path or path.startswith(self.path + "/")
        m = self.regex.fullmatch(path)
        if not m:
            return False
        pid = m.groupdict().get("pid")
        return pid is None or tgid is None or pid == tgid

    def ancestors(self) -> list[str]:
        """The fixed folders on the way to this declaration."""
        fixed = self.path.split("{", 1)[0]
        head = os.path.dirname(fixed if "{" not in self.path else fixed.rstrip("/") + "x")
        out = []
        while head and head != "/":
            out.append(head)
            head = os.path.dirname(head)
        return out


class Declarations:
    def __init__(self, decls: list[Decl]):
        self.decls = decls
        self.on_the_way = set()
        for d in decls:
            self.on_the_way.update(d.ancestors())

    def judge(self, op: str, path: str, via: str, tgid: str | None) -> bool:
        """Whether `op` on the object at `path` (canonical) is declared. `via` is "fd" when the
        change went through a descriptor, "path" when a call named it by path, "snapshot" when
        two snapshots show it."""
        for d in self.decls:
            if op in OPS_ALLOWED[d.kind] and d.matches(path, tgid):
                if d.kind == "stdio" and via == "path":
                    continue
                return True
        return op == "mkdir" and path in self.on_the_way


# ----------------------------------------------------------------------------------------------
# strace's text

PREFIX = re.compile(r"^(?:\[pid\s+(\d+)\]\s+|(\d+)\s+)?(?:(\d+\.\d+)\s+)?")
EXIT = re.compile(r"^\+\+\+ (exited with -?\d+|killed by SIG\w+(?: \(core dumped\))?) \+\+\+$")
SUPERSEDED = re.compile(r"^\+\+\+ superseded by execve in pid (\d+) \+\+\+$")
SIGNAL = re.compile(r"^--- (?:SIG\w+ \{.*\}|stopped by SIG\w+) ---$")
PERSONALITY = re.compile(r"^\[ Process PID=\d+ runs in (?:32 bit|x32) mode\. \]$")
DETACHED = re.compile(r"^strace: Process (\d+) detached$")
START = re.compile(r"^([a-z_][a-z0-9_]*)\(")
UNFINISHED = " <unfinished ...>"
RESUMED = re.compile(r"^<\.\.\. ([a-z_][a-z0-9_]*) resumed>(.*)$")
RESULT = re.compile(r"^(\?|-?\d+|0x[0-9a-fA-F]+)(?:<((?:[^>\\]|\\.)*)>(\(deleted\))?)?(?:\s+(.*))?$")
DESCRIPTOR = re.compile(r"^(AT_FDCWD|-?\d+)(?:<((?:[^>\\]|\\.)*)>(\(deleted\))?)?$")
DELETED = " (deleted)"


class Unparsed(ValueError):
    pass


def unescape(s: str) -> str:
    """strace's C escapes (\\" \\\\ \\n \\t \\ooo \\xhh ...) back to the path's bytes."""
    if "\\" not in s:
        return s
    out = bytearray()
    i = 0
    simple = {"n": 10, "t": 9, "r": 13, "v": 11, "f": 12, "a": 7, "b": 8, "\\": 92, '"': 34, "'": 39}
    while i < len(s):
        c = s[i]
        if c != "\\" or i + 1 == len(s):
            out += c.encode("utf-8", "surrogateescape")
            i += 1
            continue
        n = s[i + 1]
        if n == "x" and re.match(r"[0-9a-fA-F]{2}", s[i + 2:i + 4]):
            out.append(int(s[i + 2:i + 4], 16))
            i += 4
        elif n in "01234567":
            m = re.match(r"[0-7]{1,3}", s[i + 1:])
            out.append(int(m.group(0), 8) & 0xFF)
            i += 1 + len(m.group(0))
        elif n in simple:
            out.append(simple[n])
            i += 2
        else:
            raise Unparsed(f"unknown escape \\{n}")
    return out.decode("utf-8", "surrogateescape")


def split_call(text: str) -> tuple[str, str, str]:
    """name, the arguments' text and the result's text of a complete call line. Strings,
    structures, arrays and decoded descriptors (<...>, in which strace escapes < and >) are kept
    whole; the argument list ends at the parenthesis that closes it."""
    m = START.match(text)
    if not m:
        raise Unparsed("not a call")
    i, depth, close = m.end(), [], {"(": ")", "[": "]", "{": "}"}
    while i < len(text):
        c = text[i]
        if c == '"':
            j = i + 1
            while j < len(text) and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            if j >= len(text):
                raise Unparsed("unterminated string")
            i = j + 1
            continue
        if c == "<" and i > 0 and (text[i - 1].isdigit() or text[i - 4:i] == "CWD"):
            j = text.find(">", i)
            if j < 0:
                raise Unparsed("unterminated descriptor")
            i = j + 1
            continue
        if c in close:
            depth.append(close[c])
        elif c in ")]}":
            if not depth:
                if c != ")":
                    raise Unparsed("unbalanced arguments")
                rest = text[i + 1:]
                r = re.match(r"^\s*=\s*(.*)$", rest)
                if not r:
                    raise Unparsed("no result")
                return m.group(1), text[m.end():i], r.group(1)
            if depth.pop() != c:
                raise Unparsed("unbalanced arguments")
        i += 1
    raise Unparsed("unbalanced arguments")


def split_args(s: str) -> list[str]:
    """Top-level arguments of an argument text."""
    args, cur, depth, i = [], [], [], 0
    close = {"(": ")", "[": "]", "{": "}"}
    while i < len(s):
        c = s[i]
        if c == '"':
            j = i + 1
            while j < len(s) and s[j] != '"':
                j += 2 if s[j] == "\\" else 1
            cur.append(s[i:j + 1])
            i = j + 1
            continue
        if c == "<" and i > 0 and (s[i - 1].isdigit() or s[i - 4:i] == "CWD"):
            j = s.find(">", i)
            if j < 0:
                raise Unparsed("unterminated descriptor")
            cur.append(s[i:j + 1])
            i = j + 1
            continue
        if c in close:
            depth.append(close[c])
        elif c in ")]}":
            if not depth or depth.pop() != c:
                raise Unparsed("unbalanced arguments")
        if c == "," and not depth:
            args.append("".join(cur).strip())
            cur = []
        else:
            cur.append(c)
        i += 1
    if depth:
        raise Unparsed("unbalanced arguments")
    tail = "".join(cur).strip()
    if tail or args:
        args.append(tail)
    return args


def string_arg(a: str) -> str | None:
    """A quoted path (None for NULL). A truncated string ("..."...) cannot be a path."""
    if a == "NULL":
        return None
    m = re.fullmatch(r'"((?:[^"\\]|\\.)*)"', a)
    if not m:
        raise Unparsed(f"not a path: {a[:60]}")
    return unescape(m.group(1))


def fd_arg(a: str) -> tuple[str, str | None]:
    """(the descriptor number or AT_FDCWD, the decoded object or None)."""
    m = DESCRIPTOR.match(a)
    if not m:
        raise Unparsed(f"not a descriptor: {a[:60]}")
    obj = unescape(m.group(2)) if m.group(2) is not None else None
    if obj is not None and m.group(3):
        obj += DELETED
    return m.group(1), obj


def flags_of(a: str) -> set[str]:
    return set(t for t in re.split(r"[|{}=,\s\[\]]+", a) if t)


def anonymous(obj: str) -> bool:
    """An object that is no file: socket:[..], pipe:[..], anon_inode:..., or a memfd (strace
    prints a memfd as /memfd:NAME and always as deleted)."""
    if not obj.startswith("/"):
        return True
    return obj.startswith(("/memfd:", "/dmabuf:")) and obj.endswith(DELETED)


def object_path(obj: str) -> str:
    return obj[: -len(DELETED)] if obj.endswith(DELETED) else obj


# ----------------------------------------------------------------------------------------------
# The processes


@dataclass
class Fd:
    obj: str | None  # the decoded object, None if unknown
    mode: str  # "r", "w", "rw", "path", or "?" (unknown: inherited, received, or not seen made)
    cloexec: bool | None


class Table:
    def __init__(self, fds: dict[int, Fd] | None = None):
        self.fds: dict[int, Fd] = dict(fds or {})

    def get(self, n: int) -> Fd:
        return self.fds.get(n) or Fd(None, "?", None)

    def copy(self) -> "Table":
        return Table({k: Fd(v.obj, v.mode, v.cloexec) for k, v in self.fds.items()})


class Folder:
    def __init__(self, cwd: str | None):
        self.cwd = cwd


@dataclass
class Task:
    tid: str
    tgid: str
    table: Table
    folder: Folder
    ended: bool = False
    exe: str = "?"


@dataclass
class PathOp:
    """A change named by a path, judged once every log has been read (in time order), against
    what the trace knows about the file system at that moment."""
    ts: float
    seq: int
    tid: str
    tgid: str
    call: str
    op: str
    path: str  # absolute, as the process named it (links not resolved)
    follow: bool  # whether the call follows a final symbolic link
    target: str | None = None  # a symbolic link's text (symlink)
    peer: int | None = None  # seq of the other half of a rename, exchange or link; -1 when it could not be placed
    kernel: bool = False  # the kernel named this object (an open's result): already canonical
    exchange: bool = False


class Report:
    def __init__(self, decls: Declarations):
        self.decls = decls
        self.seen: dict[tuple[str, str], set[str]] = defaultdict(set)
        self.calls = 0
        self.logs = 0
        self.ops: list[PathOp] = []
        self.seq = 0
        self.links_seed: dict[str, str] = {}
        self.plain_seed: set[str] = set()

    def note(self, kind: str, call: str, what: str) -> None:
        self.seen[(kind, what)].add(call)

    # -- one log ---------------------------------------------------------------------------

    def read(self, path: str) -> None:
        self.logs += 1
        default = path.rsplit(".", 1)[-1] if re.search(r"\.\d+$", path) else "main"
        records = list(self.records(path, default))
        log = LogState(self, path, records)
        log.run()

    def records(self, path: str, default: str):
        """(tid, timestamp, kind, payload) for every line: kind is "call" (name, args, result
        or None for a call the process never returned from), "exit", "superseded", "signal",
        "detached" or "bad" (the line, the reason)."""
        pending: dict[tuple[str, str], tuple[float, str]] = {}
        with open(path, errors="surrogateescape") as f:
            for raw in f:
                line = raw.rstrip("\n")
                m = PREFIX.match(line)
                tid = m.group(1) or m.group(2) or default
                ts = float(m.group(3)) if m.group(3) else 0.0
                text = line[m.end():].strip()
                if not text:
                    continue
                try:
                    if EXIT.match(text):
                        yield tid, ts, "exit", text
                    elif s := SUPERSEDED.match(text):
                        yield tid, ts, "superseded", s.group(1)
                    elif SIGNAL.match(text):
                        yield tid, ts, "signal", text
                    elif d := DETACHED.match(text):
                        yield d.group(1), ts, "detached", text
                    elif PERSONALITY.match(text):
                        yield tid, ts, "bad", (text, "a 32-bit or x32 process: its calls are not x86_64's")
                    elif text.endswith(UNFINISHED):
                        s = START.match(text)
                        if not s:
                            raise Unparsed("not a call")
                        pending[(tid, s.group(1))] = (ts, text[: -len(UNFINISHED)])
                    elif r := RESUMED.match(text):
                        head = pending.pop((tid, r.group(1)), None)
                        if head is None:
                            raise Unparsed("resumed without its start")
                        name, args, result = split_call(head[1] + r.group(2))
                        yield tid, head[0], "call", (name, args, result)
                    else:
                        name, args, result = split_call(text)
                        yield tid, ts, "call", (name, args, result)
                except Unparsed as e:
                    yield tid, ts, "bad", (text[:200], str(e))
                if len(pending) > 100000:
                    yield tid, ts, "bad", (text[:200], "too many unfinished calls")
                    pending.clear()
        for (tid, name), (ts, head) in pending.items():
            # The process ended inside the call: it may have taken effect.
            try:
                body = head[len(name) + 1:].rstrip().rstrip(",")
                split_args(body)
                yield tid, ts, "call", (name, body, None)
            except Unparsed as e:
                yield tid, ts, "bad", (head[:200], str(e))

    # -- after every log: the paths -------------------------------------------------------

    def judge_paths(self) -> None:
        ops = sorted(self.ops, key=lambda o: (o.ts, o.seq))
        # First pass: which entries the trace changes at any time. An entry the trace changes
        # is never taken from the file system as it is now (it may have been a link then).
        ns = Namespace(self.links_seed, self.plain_seed, (set(), set()))
        for o in ops:
            ns.apply(o, ns.resolve(o, self)[0] if not o.kernel else o.path)
        touched = (ns.changed, ns.removed)
        ns = Namespace(self.links_seed, self.plain_seed, touched)
        for o in ops:
            if o.kernel:
                p, why = o.path, ""
            else:
                p, why = ns.resolve(o, self)
            if p is None:
                self.note("UNRESOLVED", o.call, f"{o.path}: {why}")
            elif anonymous(p):
                self.note("anonymous", o.call, p)
            else:
                obj = object_path(p)
                self.note("declared" if self.decls.judge(o.op, obj, "path", o.tgid) else "OTHER", o.call, obj)
            ns.apply(o, p)

    # -- the verdict -----------------------------------------------------------------------

    def print(self, label: str = "trace") -> int:
        bad = ["LATE", "LOST", "UNKNOWN", "UNSUPPORTED", "UNPARSED", "UNRESOLVED", "OTHER"]
        order = bad + ["declared", "anonymous"]
        counts = defaultdict(int)
        # One line per verdict, call and object: "OTHER openat: /tmp/x" (e2e/expect_variant.py
        # looks for that exact line).
        for kind in order:
            for (k, p), calls in sorted(self.seen.items()):
                if k == kind:
                    counts[kind] += 1
                    for call in sorted(calls):
                        print(f"{kind} {call}: {p}")
        nbad = sum(counts[k] for k in bad)
        print(f"{label}: {self.calls} calls in {self.logs} logs; " + ", ".join(f"{counts[k]} {k}" for k in order)
              + " (objects)")
        if self.calls == 0:
            print(f"{label}: FAIL: no call in the logs (an empty or foreign log proves nothing)")
            return 1
        print(f"{label}: FAIL: see the lines above" if nbad else f"{label}: every change is declared")
        return 1 if nbad else 0


class LogState:
    """The processes of one log."""

    def __init__(self, report: Report, path: str, records: list):
        self.r = report
        self.path = path
        self.records = records
        self.tasks: dict[str, Task] = {}
        self.children: dict[str, tuple[str, set[str]]] = {}  # child tid -> (parent tid, flags)
        self.first = True

    # The clone records first, so a child that runs before its parent's clone returns already
    # knows whose descriptors and folder it has.
    def run(self) -> None:
        for tid, ts, kind, payload in self.records:
            if kind == "call" and payload[0] in ("clone", "clone3", "fork", "vfork") and payload[2] is not None:
                m = RESULT.match(payload[2])
                if m and m.group(1) not in ("?",) and int(m.group(1), 0) > 0:
                    self.children[str(int(m.group(1), 0))] = (tid, self.clone_flags(payload[0], payload[1]))
        for tid, ts, kind, payload in self.records:
            try:
                self.record(tid, ts, kind, payload)
            except (Unparsed, IndexError, ValueError) as e:
                text = f"{payload[0]}({payload[1][:120]}) = {payload[2]}" if kind == "call" else str(payload)
                self.r.note("UNPARSED", payload[0] if kind == "call" else "line", f"{text} ({e})")
        for t in self.tasks.values():
            if not t.ended:
                self.r.note("LOST", "trace", f"process {t.tid} ({t.exe}) has no exit in {self.path}: the trace "
                            "let it go (strace detached or was killed) or the log is cut; what it did after is unseen")

    @staticmethod
    def clone_flags(name: str, args: str) -> set[str]:
        if name in ("fork", "vfork"):
            return {"VFORK"} if name == "vfork" else set()
        m = re.search(r"flags=([A-Z0-9_|x]+)", args)
        return flags_of(m.group(1)) if m else set()

    def task(self, tid: str) -> Task:
        t = self.tasks.get(tid)
        if t is not None:
            return t
        link = self.children.get(tid)
        parent = self.tasks.get(link[0]) if link else None
        if parent is None:
            # The first process of the log, or one whose parent the log does not show.
            t = Task(tid, tid, Table(), Folder(None))
        else:
            flags = link[1]
            t = Task(tid, parent.tgid if "CLONE_THREAD" in flags else tid,
                     parent.table if "CLONE_FILES" in flags else parent.table.copy(),
                     parent.folder if "CLONE_FS" in flags else Folder(parent.folder.cwd), exe=parent.exe)
            if flags & {"CLONE_NEWNS", "CLONE_NEWUSER", "CLONE_NEWPID"}:
                self.r.note("UNSUPPORTED", "clone", f"process {tid} starts in a new namespace: its paths cannot be compared")
        self.tasks[tid] = t
        return t

    def record(self, tid: str, ts: float, kind: str, payload) -> None:
        if kind == "bad":
            self.r.note("UNPARSED", "line", f"{payload[0]} ({payload[1]})")
            return
        if kind == "detached":
            self.r.note("LOST", "trace", f"strace detached from process {tid}: what it did after is unseen")
            self.task(tid)
            return
        t = self.task(tid)
        if kind == "exit":
            t.ended = True
            return
        if kind == "superseded":
            # A thread's execve took over the thread group: it continues as `tid`.
            other = self.task(payload)
            other.ended = True
            t.exe = other.exe
            return
        if kind == "signal":
            return
        name, args, result = payload
        self.r.calls += 1
        if self.first:
            self.first = False
            if name not in ("execve", "execveat"):
                self.r.note("LATE", name, f"{self.path} does not start with the program's execve: the trace began after it ran")
        raw = re.fullmatch(r"syscall_0x([0-9a-f]+)", name)
        if raw:
            # A call strace's tables are older than: the kernel's own table names it, but strace
            # printed its arguments as numbers.
            nr = int(raw.group(1), 16)
            known = KERNEL_TABLE.get(nr)
            if known is None or known not in KNOWN:
                self.r.note("UNKNOWN", name, f"{name} in process {tid}: a call the observer does not know")
                return
            if known not in NONE and known not in UNSUPPORTED:
                if result is not None and RESULT.match(result) and RESULT.match(result).group(1).startswith("-") \
                        and not RESULT.match(result).group(1).startswith("0x"):
                    return  # failed: no effect
                self.r.note("UNRESOLVED", known, f"{known} ({name}) in process {tid}: strace is older than the "
                            "kernel and printed its arguments as numbers, so what it changed is unknown")
                return
            name = known
        if name not in KNOWN:
            self.r.note("UNKNOWN", name, f"{name} in process {tid}: a call the observer does not know")
            return
        rc, robj, rtext = None, None, ""
        if result is not None:
            m = RESULT.match(result)
            if not m:
                raise Unparsed(f"unreadable result: {result[:60]}")
            rtext = m.group(4) or ""
            if m.group(1) == "?":
                if rtext.startswith("ERESTART"):
                    return  # interrupted before it did anything; the kernel runs it again
            else:
                rc = int(m.group(1), 0)
                if rc < 0 and not m.group(1).startswith("0x"):
                    return  # failed: no effect
            if m.group(2) is not None:
                robj = unescape(m.group(2)) + (DELETED if m.group(3) else "")
        a = split_args(args)
        for x in a:
            if x.startswith("AT_FDCWD<"):
                t.folder.cwd = fd_arg(x)[1]
        if name in NONE:
            return
        if name in UNSUPPORTED:
            self.r.note("UNSUPPORTED", name, f"{name} in process {tid}: {UNSUPPORTED[name]}")
            return
        getattr(self, "on_" + name, None) and getattr(self, "on_" + name)(t, ts, name, a, rc, robj)
        if name in ANON_FD:
            self.add_fd(t, rc, robj, "rw", any(f.endswith("CLOEXEC") for x in a for f in flags_of(x)))
        elif name in PAIRS:
            cloexec = any(f.endswith("CLOEXEC") for x in a for f in flags_of(x))
            for fd in re.findall(r"(\d+)(?:<((?:[^>\\]|\\.)*)>)?", a[PAIRS[name]]):
                self.add_fd(t, int(fd[0]), unescape(fd[1]) if fd[1] else None, "rw", cloexec)
        elif name in OPENS:
            self.open(t, ts, name, a, rc, robj)
        elif name in FD_WRITE:
            n, op = FD_WRITE[name]
            self.fd_change(t, name, op, a[n])
        elif name in PATH_CALLS:
            self.path_call(t, ts, name, a, rc)

    # -- descriptors -----------------------------------------------------------------------

    def add_fd(self, t: Task, n: int | None, obj: str | None, mode: str, cloexec: bool | None) -> None:
        if n is not None and n >= 0:
            t.table.fds[n] = Fd(obj, mode, cloexec)

    def target(self, t: Task, a: str) -> tuple[int | None, str | None]:
        """A descriptor argument's number and object: strace's decoding first, the table second."""
        num, obj = fd_arg(a)
        if num == "AT_FDCWD":
            return None, obj if obj is not None else t.folder.cwd
        n = int(num)
        if obj is None:
            obj = t.table.get(n).obj
        elif n in t.table.fds:
            t.table.fds[n].obj = obj
        return n, obj

    def fd_change(self, t: Task, call: str, op: str, a: str) -> None:
        n, obj = self.target(t, a)
        if obj is None:
            self.r.note("UNRESOLVED", call, f"descriptor {a} in process {t.tid}: the trace cannot tell what it refers to")
        elif anonymous(obj):
            self.r.note("anonymous", call, obj)
        else:
            p = object_path(obj)
            self.r.note("declared" if self.r.decls.judge(op, p, "fd", t.tgid) else "OTHER", call, p)

    def on_close(self, t, ts, name, a, rc, robj):
        t.table.fds.pop(int(fd_arg(a[0])[0]), None)

    def on_close_range(self, t, ts, name, a, rc, robj):
        lo, hi = int(a[0], 0), int(a[1].replace("~0", str(2**32 - 1)), 0)
        fl = flags_of(a[2])
        if "CLOSE_RANGE_UNSHARE" in fl:
            t.table = t.table.copy()
        for n in [n for n in t.table.fds if lo <= n <= hi]:
            if "CLOSE_RANGE_CLOEXEC" in fl:
                t.table.fds[n].cloexec = True
            else:
                del t.table.fds[n]

    def dup(self, t: Task, old: str, new: int, robj: str | None, cloexec: bool) -> None:
        n, obj = self.target(t, old)
        src = t.table.get(n) if n is not None else Fd(obj, "?", None)
        t.table.fds[new] = Fd(robj or obj or src.obj, src.mode, cloexec)

    def on_dup(self, t, ts, name, a, rc, robj):
        self.dup(t, a[0], rc, robj, False)

    def on_dup2(self, t, ts, name, a, rc, robj):
        self.dup(t, a[0], rc, robj, False)

    def on_dup3(self, t, ts, name, a, rc, robj):
        self.dup(t, a[0], rc, robj, "O_CLOEXEC" in flags_of(a[2]))

    def on_pidfd_getfd(self, t, ts, name, a, rc, robj):
        self.add_fd(t, rc, robj, "?", "O_CLOEXEC" in flags_of(a[2]) or None)

    FCNTL_NONE = set("""F_GETFD F_GETFL F_SETFL F_GETLK F_SETLK F_SETLKW F_GETLK64 F_SETLK64 F_SETLKW64
        F_OFD_GETLK F_OFD_SETLK F_OFD_SETLKW F_GETOWN F_SETOWN F_GETOWN_EX F_SETOWN_EX F_GETSIG
        F_SETSIG F_GETLEASE F_SETLEASE F_NOTIFY F_GETPIPE_SZ F_SETPIPE_SZ F_ADD_SEALS F_GET_SEALS
        F_GET_RW_HINT F_SET_RW_HINT F_GET_FILE_RW_HINT F_SET_FILE_RW_HINT F_DUPFD_QUERY
        F_CREATED_QUERY F_GETOWNER_UIDS""".split())

    def on_fcntl(self, t, ts, name, a, rc, robj):
        cmd = a[1]
        if cmd in ("F_DUPFD", "F_DUPFD_CLOEXEC"):
            self.dup(t, a[0], rc, robj, cmd == "F_DUPFD_CLOEXEC")
        elif cmd == "F_SETFD":
            n, _ = self.target(t, a[0])
            if n in t.table.fds:
                t.table.fds[n].cloexec = "FD_CLOEXEC" in flags_of(a[2])
            elif n is not None:
                t.table.fds[n] = Fd(fd_arg(a[0])[1], "?", "FD_CLOEXEC" in flags_of(a[2]))
        elif cmd not in self.FCNTL_NONE:
            self.r.note("UNSUPPORTED", "fcntl", f"fcntl {cmd} in process {t.tid}: a command the observer does not know")

    # ioctls that only read, or set the descriptor's own flags.
    IOCTL_READ = set("""TCGETS TCGETS2 TIOCGWINSZ TIOCGPGRP TIOCGSID TIOCGPTN TIOCGPTPEER FIONREAD
        FIOQSIZE FIGETBSZ FIONBIO FIOASYNC FIOCLEX FIONCLEX FS_IOC_GETFLAGS FS_IOC_GETVERSION
        FS_IOC_FSGETXATTR FS_IOC_FIEMAP FS_IOC_GET_ENCRYPTION_POLICY FS_IOC_GET_ENCRYPTION_POLICY_EX
        FS_IOC_GETFSLABEL FS_IOC_GETFSUUID FS_IOC_GETFSSYSFSPATH BLKGETSIZE64 BLKSSZGET
        BLKGETSIZE""".split())
    TERMINAL = re.compile(r"^/dev/(tty\w*|pts/\d+|ptmx|console)$")

    def on_ioctl(self, t, ts, name, a, rc, robj):
        n, obj = self.target(t, a[0])
        req = a[1].split(" ", 1)[0]
        if req in ("FIOCLEX", "FIONCLEX") and n in t.table.fds:
            t.table.fds[n].cloexec = req == "FIOCLEX"
        if obj is None:
            self.r.note("UNRESOLVED", "ioctl", f"ioctl {req} on descriptor {a[0]} in process {t.tid}: the trace cannot tell what it refers to")
        elif anonymous(obj) or req in self.IOCTL_READ or self.TERMINAL.match(object_path(obj)):
            return
        else:
            p = object_path(obj)
            self.r.note("declared" if self.r.decls.judge("ioctl", p, "fd", t.tgid) else "OTHER", f"ioctl {req}", p)

    def on_mmap(self, t, ts, name, a, rc, robj):
        prot, fl = flags_of(a[2]), flags_of(a[3])
        if "MAP_ANONYMOUS" in fl or not fl & {"MAP_SHARED", "MAP_SHARED_VALIDATE"}:
            return
        n, obj = self.target(t, a[4])
        if n is None or n < 0:
            return
        mode = t.table.get(n).mode
        if "PROT_WRITE" in prot or mode not in ("r", "path"):
            # Writable now, or mprotect can make it so (the descriptor may be writable).
            if obj is None:
                self.r.note("UNRESOLVED", "mmap", f"a shared map of descriptor {a[4]} in process {t.tid}")
            elif not anonymous(obj):
                p = object_path(obj)
                self.r.note("declared" if self.r.decls.judge("map", p, "fd", t.tgid) else "OTHER", "mmap", p)

    def on_prctl(self, t, ts, name, a, rc, robj):
        if a and a[0].startswith("PR_SET_SECCOMP"):
            self.r.note("UNSUPPORTED", "prctl", f"PR_SET_SECCOMP in process {t.tid}: a seccomp filter can hide calls from the trace")

    def on_seccomp(self, t, ts, name, a, rc, robj):
        if a and a[0] in ("SECCOMP_SET_MODE_FILTER", "SECCOMP_SET_MODE_STRICT"):
            self.r.note("UNSUPPORTED", "seccomp", f"{a[0]} in process {t.tid}: a seccomp filter can hide calls from the trace")

    def on_recvmsg(self, t, ts, name, a, rc, robj):
        self.received(t, name, a)

    def on_recvmmsg(self, t, ts, name, a, rc, robj):
        self.received(t, name, a)

    def received(self, t: Task, name: str, a: list[str]) -> None:
        """Descriptors passed over a socket (SCM_RIGHTS): known by strace's decoding, their
        access mode unknown."""
        text = ",".join(a)
        if "SCM_RIGHTS" not in text:
            return
        cloexec = "MSG_CMSG_CLOEXEC" in text or None
        found = 0
        for m in re.finditer(r"cmsg_type=SCM_RIGHTS, cmsg_data=\[", text):
            j, depth = m.end(), 1
            k = j
            while k < len(text) and depth:
                if text[k] == "<":
                    k = text.index(">", k)
                elif text[k] == "[":
                    depth += 1
                elif text[k] == "]":
                    depth -= 1
                k += 1
            for item in split_args(text[j:k - 1]):
                if item == "...":
                    raise Unparsed("SCM_RIGHTS descriptors cut short")
                num, obj = fd_arg(item)
                self.add_fd(t, int(num), obj, "?", cloexec)
                found += 1
        if not found:
            self.r.note("UNRESOLVED", name, f"descriptors received in process {t.tid} that strace did not decode")

    def on_chdir(self, t, ts, name, a, rc, robj):
        p = string_arg(a[0])
        if p is not None and p.startswith("/"):
            t.folder.cwd = p
        elif t.folder.cwd is not None and p is not None:
            t.folder.cwd = t.folder.cwd.rstrip("/") + "/" + p
        else:
            t.folder.cwd = None

    def on_fchdir(self, t, ts, name, a, rc, robj):
        _, obj = self.target(t, a[0])
        t.folder.cwd = obj if obj and obj.startswith("/") and not anonymous(obj) else None

    def on_unshare(self, t, ts, name, a, rc, robj):
        fl = flags_of(a[0])
        if "CLONE_FILES" in fl:
            t.table = t.table.copy()
        if "CLONE_FS" in fl:
            t.folder = Folder(t.folder.cwd)
        if fl & {"CLONE_NEWNS", "CLONE_NEWUSER", "CLONE_NEWPID"}:
            self.r.note("UNSUPPORTED", "unshare", f"process {t.tid} enters a new namespace: its paths cannot be compared")

    def on_clone(self, t, ts, name, a, rc, robj):
        if rc is not None and rc > 0:
            self.task(str(rc))

    on_clone3 = on_fork = on_vfork = on_clone

    def on_execve(self, t, ts, name, a, rc, robj):
        exe = a[0] if name == "execve" else a[1]
        t.exe = exe.strip('"')[:80]
        # Every other thread of the group ends; descriptors marked close-on-exec close.
        for o in self.tasks.values():
            if o is not t and o.tgid == t.tgid:
                o.ended = True
        t.table = Table({n: f for n, f in t.table.fds.items() if f.cloexec is not True})

    on_execveat = on_execve

    # -- paths -----------------------------------------------------------------------------

    def base(self, t: Task, dirfd: str | None) -> tuple[str | None, str]:
        """The folder a relative path starts from, or None and why."""
        if dirfd is None:
            return t.folder.cwd, "the current folder is not known"
        num, obj = fd_arg(dirfd)
        if num == "AT_FDCWD":
            obj = obj if obj is not None else t.folder.cwd
            return obj, "the current folder is not known"
        n, obj = self.target(t, dirfd)
        if obj is None or anonymous(obj):
            return None, f"descriptor {dirfd} is not a known folder"
        return object_path(obj), ""

    PROC_LINK = re.compile(r"^/proc/(self|thread-self|\d+)(?:/task/(\d+))?/(fd/(\d+)|cwd|root)(/.*)?$")

    def proc_link(self, t: Task, call: str, path: str, follow: bool) -> tuple[bool, str | None, bool]:
        """A path through /proc's magic links (a descriptor, the current folder, the root) of a
        traced process: (whether it is one, the path it stands for or None when unknown, whether
        that path is the object itself). The kernel follows these links to the object, not to a
        name, so the object comes from the descriptor table."""
        m = self.PROC_LINK.match(path)
        if not m:
            return False, None, False
        who, rest = m.group(1), m.group(5) or ""
        if not rest and not follow:
            return True, None, False  # the magic link itself: nothing on disk changes, but say so
        if who in ("self", "thread-self"):
            owner = t
        else:
            owner = next((x for x in self.tasks.values() if x.tgid == who and not x.ended), None)
            if owner is None:
                return True, None, False
        if m.group(3) == "root":
            return True, "/" + rest.lstrip("/") if rest else "/", False
        if m.group(3) == "cwd":
            base = owner.folder.cwd
        else:
            base = owner.table.get(int(m.group(4))).obj
        if base is None or anonymous(base) and not rest:
            return True, base, not rest
        if anonymous(base):
            return True, None, False
        return True, object_path(base) + rest, not rest

    def pathop(self, t: Task, ts: float, call: str, op: str, dirfd: str | None, rel: str | None,
               follow: bool, **kw) -> int | None:
        """Queue a change named by (dirfd, rel); returns its index, or None when it cannot be placed."""
        if rel and rel.startswith("/proc/"):
            magic, p, whole = self.proc_link(t, call, rel, follow)
            if magic:
                if p is None:
                    self.r.note("UNRESOLVED", call, f"{rel} in process {t.tid}: a /proc link to an object the trace does not know")
                    return None
                if anonymous(p):
                    self.r.note("anonymous", call, p)
                    return None
                return self.queue(PathOp(ts, 0, t.tid, t.tgid, call, op, p, follow and not whole, kernel=whole, **kw))
        if rel is None or rel == "":
            # AT_EMPTY_PATH or NULL: the descriptor itself.
            if dirfd is None:
                self.r.note("UNRESOLVED", call, f"no path in process {t.tid}")
                return None
            n, obj = self.target(t, dirfd)
            if obj is None:
                self.r.note("UNRESOLVED", call, f"descriptor {dirfd} in process {t.tid}: the trace cannot tell what it refers to")
                return None
            if anonymous(obj):
                self.r.note("anonymous", call, obj)
                return None
            return self.queue(PathOp(ts, 0, t.tid, t.tgid, call, op, object_path(obj), False, kernel=True, **kw))
        if not rel.startswith("/"):
            b, why = self.base(t, dirfd)
            if b is None:
                self.r.note("UNRESOLVED", call, f"{rel!r} in process {t.tid}: {why}")
                return None
            rel = b.rstrip("/") + "/" + rel
        return self.queue(PathOp(ts, 0, t.tid, t.tgid, call, op, rel, follow, **kw))

    def queue(self, o: PathOp) -> int:
        self.r.seq += 1
        o.seq = self.r.seq
        self.r.ops.append(o)
        return len(self.r.ops) - 1

    def open(self, t: Task, ts: float, name: str, a: list[str], rc, robj) -> None:
        dirarg, patharg, flagarg = OPENS[name]
        fl = flags_of(a[flagarg]) if flagarg is not None and len(a) > flagarg else set()
        if name == "creat":
            fl = {"O_WRONLY", "O_CREAT", "O_TRUNC"}
        numeric = any(re.fullmatch(r"0x[0-9a-fA-F]+|\d+", f) for f in fl if f not in ("0",))
        mode = "path" if "O_PATH" in fl else "rw" if "O_RDWR" in fl else "w" if "O_WRONLY" in fl else "r"
        if numeric:
            mode = "?"
        self.add_fd(t, rc, robj, mode, "O_CLOEXEC" in fl)
        writes = mode in ("w", "rw", "?") or fl & {"O_CREAT", "O_TRUNC", "O_TMPFILE", "O_APPEND"}
        if not writes or "O_PATH" in fl:
            return
        op = "create" if fl & {"O_CREAT", "O_TMPFILE"} else "write"
        if robj is not None:
            # The kernel's answer: the object opened, every link resolved.
            if anonymous(robj):
                self.r.note("anonymous", name, robj)
            else:
                self.queue(PathOp(ts, 0, t.tid, t.tgid, name, op, object_path(robj), False, kernel=True))
            return
        if patharg is None:
            self.r.note("UNRESOLVED", name, f"{name} in process {t.tid} without a decoded descriptor")
            return
        follow = "O_NOFOLLOW" not in fl and not {"O_CREAT", "O_EXCL"} <= fl
        if name == "openat2" and re.search(r"RESOLVE_IN_ROOT|RESOLVE_BENEATH", a[flagarg]):
            self.r.note("UNRESOLVED", name, f"openat2 with RESOLVE_IN_ROOT or RESOLVE_BENEATH in process {t.tid}")
            return
        self.pathop(t, ts, name, op, a[dirarg] if dirarg is not None else None, string_arg(a[patharg]), follow)

    AT = {"mkdirat": (0, 1), "mknodat": (0, 1), "unlinkat": (0, 1), "fchmodat": (0, 1), "fchmodat2": (0, 1),
          "fchownat": (0, 1), "utimensat": (0, 1), "futimesat": (0, 1), "setxattrat": (0, 1),
          "removexattrat": (0, 1), "file_setattr": (0, 1)}
    FLAGS_ARG = {"fchmodat2": 3, "fchownat": 4, "utimensat": 3, "setxattrat": 2, "removexattrat": 2,
                 "file_setattr": 4, "unlinkat": 2}

    def path_call(self, t: Task, ts: float, name: str, a: list[str], rc) -> None:
        fl = flags_of(a[self.FLAGS_ARG[name]]) if name in self.FLAGS_ARG and len(a) > self.FLAGS_ARG[name] else set()
        nofollow = "AT_SYMLINK_NOFOLLOW" in fl
        if name in ("mkdir",):
            self.pathop(t, ts, name, "mkdir", None, string_arg(a[0]), False)
        elif name == "mkdirat":
            self.pathop(t, ts, name, "mkdir", a[0], string_arg(a[1]), False)
        elif name in ("mknod", "mknodat"):
            d, p, m = (None, a[0], a[1]) if name == "mknod" else (a[0], a[1], a[2])
            kinds = flags_of(m)
            op = "bind" if "S_IFSOCK" in kinds else "mknod" if kinds & {"S_IFIFO", "S_IFCHR", "S_IFBLK"} else "create"
            self.pathop(t, ts, name, op, d, string_arg(p), False)
        elif name in ("rename", "renameat", "renameat2"):
            if name == "rename":
                src, dst = (None, a[0]), (None, a[1])
            else:
                src, dst = (a[0], a[1]), (a[2], a[3])
            rfl = flags_of(a[4]) if name == "renameat2" and len(a) > 4 else set()
            if "RENAME_WHITEOUT" in rfl:
                self.r.note("UNSUPPORTED", name, f"RENAME_WHITEOUT in process {t.tid}")
                return
            ex = "RENAME_EXCHANGE" in rfl
            i = self.pathop(t, ts, name, "rename-from", src[0], string_arg(src[1]), False, exchange=ex)
            j = self.pathop(t, ts, name, "rename-to", dst[0], string_arg(dst[1]), False, exchange=ex)
            if i is not None and j is not None:
                self.r.ops[i].peer, self.r.ops[j].peer = self.r.ops[j].seq, self.r.ops[i].seq
            elif i is not None:
                self.r.ops[i].peer = -1  # the destination could not be placed
        elif name in ("link", "linkat"):
            if name == "link":
                src, dst, follow, empty = (None, a[0]), (None, a[1]), False, False
            else:
                lfl = flags_of(a[4])
                src, dst, follow, empty = (a[0], a[1]), (a[2], a[3]), "AT_SYMLINK_FOLLOW" in lfl, "AT_EMPTY_PATH" in lfl
            srel = string_arg(src[1])
            # The object gains a name: a change of its link count (metadata) where it is.
            i = self.pathop(t, ts, name, "meta", src[0], "" if empty and not srel else srel, follow)
            j = self.pathop(t, ts, name, "link-to", dst[0], string_arg(dst[1]), False)
            if i is not None and j is not None:
                self.r.ops[i].peer, self.r.ops[j].peer = self.r.ops[j].seq, self.r.ops[i].seq
        elif name in ("symlink", "symlinkat"):
            target = string_arg(a[0])
            d, p = (None, a[1]) if name == "symlink" else (a[1], a[2])
            self.pathop(t, ts, name, "symlink", d, string_arg(p), False, target=target)
        elif name in ("unlink", "rmdir"):
            self.pathop(t, ts, name, "rmdir" if name == "rmdir" else "unlink", None, string_arg(a[0]), False)
        elif name == "unlinkat":
            self.pathop(t, ts, name, "rmdir" if "AT_REMOVEDIR" in fl else "unlink", a[0], string_arg(a[1]), False)
        elif name == "truncate":
            self.pathop(t, ts, name, "write", None, string_arg(a[0]), True)
        elif name in ("chmod", "chown", "utime", "utimes", "setxattr", "removexattr"):
            self.pathop(t, ts, name, "meta", None, string_arg(a[0]), True)
        elif name in ("lchown", "lsetxattr", "lremovexattr"):
            self.pathop(t, ts, name, "meta", None, string_arg(a[0]), False)
        elif name == "bind":
            m = re.search(r'sa_family=AF_UNIX, sun_path=(@?)("(?:[^"\\]|\\.)*")', a[1])
            if "AF_UNIX" not in a[1] or (m and m.group(1)) or re.search(r"sa_family=AF_UNIX\}", a[1]):
                return  # another family, an abstract name or an automatic one: no file
            if not m:
                raise Unparsed("an AF_UNIX address strace did not decode")
            self.pathop(t, ts, name, "bind", None, string_arg(m.group(2)), False)
        elif name in self.AT:
            d, p = self.AT[name]
            rel = string_arg(a[p])
            self.pathop(t, ts, name, "meta", a[d], rel, not nofollow)


# ----------------------------------------------------------------------------------------------
# What the trace knows of the file system: links, and entries the trace made or removed.


class Namespace:
    MAX_LINKS = 40

    # Changes to what a name is: after one of these the file system as it is now says nothing of
    # what the name was before.
    NAMESPACE_OPS = {"create", "mkdir", "bind", "mknod", "symlink", "unlink", "rmdir", "rename-from", "rename-to", "link-to"}

    MOVE_OPS = {"rename-from", "rename-to", "unlink", "rmdir"}

    def __init__(self, links: dict[str, str], plain: set[str], touched: tuple[set[str], set[str]]):
        self.links = dict(links)
        self.plain = set(plain)
        self.gone: set[str] = set()
        # Names the trace changes at some time, and names it moves or removes (whatever was
        # under those changes too); second pass only.
        self.touched, self.moved = touched
        self.changed: set[str] = set()  # names this pass saw changed (first pass)
        self.removed: set[str] = set()  # names this pass saw moved or removed (first pass)
        self.pending_from: dict[int, str] = {}  # a rename's or link's source, by its seq

    def is_touched(self, entry: str) -> bool:
        """The name is changed by the trace at some time, or a folder holding it is moved or removed."""
        if entry in self.touched:
            return True
        p = entry.rsplit("/", 1)[0]
        while p and p != "/":
            if p in self.moved:
                return True
            p = p.rsplit("/", 1)[0]
        return False

    def kind(self, entry: str, o: PathOp, report: Report) -> tuple[str, str | None]:
        """("link", text) | ("plain", None) | ("unknown", why) for one entry, as it was then."""
        if entry == "/proc":
            return "plain", None
        proc = re.fullmatch(r"/proc/(self|thread-self|\d+)(/.*)?", entry)
        if proc:
            return self.proc(entry, proc, o, report)
        if entry in self.links:
            return "link", self.links[entry]
        if entry in self.plain or entry in self.gone:
            return "plain", None
        if self.is_touched(entry):
            return "unknown", f"{entry} is changed by the trace and was not seen made before this call"
        try:
            st = os.lstat(entry)
        except OSError:
            return "unknown", f"{entry} is gone and the trace did not see it made: whether it was a link cannot be established"
        if os.path.islink(entry):
            return "link", os.readlink(entry)
        del st
        return "plain", None

    @staticmethod
    def proc(entry: str, m: re.Match, o: PathOp, report: Report) -> tuple[str, str | None]:
        who, rest = m.group(1), m.group(2) or ""
        if who == "self":
            return "link", f"/proc/{o.tgid}"
        if who == "thread-self":
            return "link", f"/proc/{o.tgid}/task/{o.tid}"
        parts = rest.strip("/").split("/") if rest else []
        if len(parts) == 3 and parts[0] == "task":
            parts = parts[2:]
        if len(parts) == 2 and parts[0] == "fd":
            return "unknown", f"{entry}: a descriptor's magic link (the trace judges a reopen by its result)"
        if parts and parts[0] in ("cwd", "root", "exe", "map_files", "ns"):
            return "unknown", f"{entry}: a magic link the observer does not follow"
        return "plain", None

    def resolve(self, o: PathOp, report: Report) -> tuple[str | None, str]:
        """The object `o.path` names, as the call itself resolves it."""
        parts = [p for p in o.path.split("/") if p not in ("", ".")]
        cur, hops = "", 0
        while parts:
            comp = parts.pop(0)
            if comp == "..":
                cur = cur.rsplit("/", 1)[0]
                continue
            cand = cur + "/" + comp
            if not parts and not o.follow:
                cur = cand
                break
            kind, text = self.kind(cand, o, report)
            if kind == "unknown":
                return None, text or ""
            if kind == "link":
                hops += 1
                if hops > self.MAX_LINKS:
                    return None, "too many links"
                if text.startswith("/"):
                    cur = ""
                parts = [p for p in text.split("/") if p not in ("", ".")] + parts
                continue
            cur = cand
        return (cur or "/"), ""

    def move(self, src: str, dst: str) -> None:
        """Model a rename of `src` (and everything under it) to `dst`."""
        def under(p, root):
            return p == root or p.startswith(root + "/")
        for coll in (self.links,):
            for k in [k for k in coll if under(k, dst)]:
                del coll[k]
            for k in [k for k in coll if under(k, src)]:
                coll[dst + k[len(src):]] = coll.pop(k)
        for k in [k for k in self.plain if under(k, dst)]:
            self.plain.discard(k)
        for k in [k for k in self.plain if under(k, src)]:
            self.plain.discard(k)
            self.plain.add(dst + k[len(src):])
        if src not in self.links:
            self.plain.add(dst)
        self.gone.add(src)
        self.gone.discard(dst)

    def apply(self, o: PathOp, p: str | None) -> None:
        if p is None:
            return
        p = object_path(p)
        if o.peer is not None and o.peer >= 0 and o.op not in ("rename-to", "link-to"):
            self.pending_from[o.seq] = p
        if o.op in self.NAMESPACE_OPS:
            self.changed.add(p)
        if o.op in self.MOVE_OPS:
            self.removed.add(p)
        if o.op in ("create", "mkdir", "bind", "mknod"):
            self.links.pop(p, None)
            self.plain.add(p)
            self.gone.discard(p)
        elif o.op == "symlink":
            self.links[p] = o.target or ""
            self.plain.discard(p)
            self.gone.discard(p)
        elif o.op in ("unlink", "rmdir"):
            self.links.pop(p, None)
            self.plain.discard(p)
            self.gone.add(p)
            if o.op == "rmdir":
                for k in [k for k in self.links if k.startswith(p + "/")]:
                    del self.links[k]
        elif o.op == "rename-from":
            if o.peer == -1:
                # The destination could not be placed: nothing is known of what moved where.
                self.links.pop(p, None)
                self.plain.discard(p)
                self.gone.add(p)
        elif o.op == "rename-to":
            src = self.pending_from.pop(o.peer, None) if o.peer is not None else None
            if src is None:
                # The source could not be placed: what this name is now is not known.
                self.links.pop(p, None)
                self.plain.discard(p)
                return
            if o.exchange:
                tmp = "\0exchange"
                self.move(p, tmp)
                self.move(src, p)
                self.move(tmp, src)
                self.gone.discard(src)
                self.gone.discard(tmp)
            else:
                self.move(src, p)
        elif o.op == "link-to":
            src = self.pending_from.pop(o.peer, None) if o.peer is not None else None
            self.gone.discard(p)
            if src is not None and src in self.links:
                # A hard link to a symbolic link is that link again.
                self.links[p] = self.links[src]
                self.plain.discard(p)
            elif src is not None:
                self.links.pop(p, None)
                self.plain.add(p)
            else:
                self.links.pop(p, None)
                self.plain.discard(p)


# ----------------------------------------------------------------------------------------------
# The snapshot check


ADDED = {"dir": "mkdir", "file": "create", "link": "symlink"}  # anything else (a socket, a FIFO) as "bind"


def judge_snapshot(before_file: str, after_file: str, decls: Declarations) -> int:
    """Every entry added, removed or changed between two e2e/manifest.py snapshots, judged by
    the same declarations as the trace. A folder whose listing changed is declared when every
    entry it gained or lost is; a folder whose time alone changed (an entry made and removed
    again inside it) when it is in a declared subtree or holds a declared path."""
    before = json.loads(Path(before_file).read_text())["entries"]
    after = json.loads(Path(after_file).read_text())["entries"]

    def ok(op: str, p: str) -> bool:
        return decls.judge(op, p, "snapshot", None)

    def holds_declared(p: str) -> bool:
        return p in decls.on_the_way or any(d.kind == "subtree" and d.matches(p, None) for d in decls.decls)

    verdict: dict[str, tuple[str, bool]] = {}
    # Deepest first, so a folder is judged after the entries it gained or lost.
    for p in sorted(set(before) | set(after), key=lambda p: (-p.count("/"), p)):
        a, b = before.get(p), after.get(p)
        if a == b:
            continue
        if a is None:
            verdict[p] = ("added", ok(ADDED.get(b.get("kind"), "bind"), p))
            continue
        if b is None:
            verdict[p] = ("removed", ok("rmdir" if a.get("kind") == "dir" else "unlink", p))
            continue
        if a.get("kind") != b.get("kind"):
            gone = ok("rmdir" if a.get("kind") == "dir" else "unlink", p)
            verdict[p] = ("replaced", gone and ok(ADDED.get(b.get("kind"), "bind"), p))
            continue
        diff = {k for k in set(a) | set(b) if a.get(k) != b.get(k)}
        what = "changed " + ",".join(sorted(diff))
        meta = diff - {"names", "mtime_ns", "sha256", "size", "target"}
        if a.get("kind") == "dir":
            na, nb = a.get("names"), b.get("names")
            if not isinstance(na, list) or not isinstance(nb, list):
                good = False  # a folder that could not be listed
            else:
                kids = set(na) ^ set(nb)
                good = all(verdict.get(os.path.join(p, k), ("", False))[1] for k in kids)
                if not kids:
                    good = holds_declared(p)
        elif a.get("kind") == "link":
            good = ok("symlink", p)
        else:
            good = ok("write", p) if diff - meta else True
        if meta:
            good = good and ok("meta", p)
        verdict[p] = (what, good)
    lines = sorted(verdict.items())
    good = [f"{w}: {p}" for p, (w, g) in lines if g]
    bad = [f"{w}: {p}" for p, (w, g) in lines if not g]
    for line in good:
        print(f"declared {line}")
    for line in bad:
        print(f"UNDECLARED {line}")
    print(f"manifest: {len(before)} entries before, {len(after)} after; {len(good)} declared changes, {len(bad)} undeclared")
    return 1 if bad else 0


# ----------------------------------------------------------------------------------------------


def load_seed(report: Report, snapshot: str) -> None:
    entries = json.loads(Path(snapshot).read_text())["entries"]
    for p, e in entries.items():
        if e.get("kind") == "link":
            report.links_seed[p] = e.get("target", "")
        else:
            report.plain_seed.add(p)


def parse_declarations(argv: list[str]) -> tuple[list[str], list[Decl], list[str]]:
    """(the remaining arguments, the declarations, the --links-from snapshots)."""
    rest, decls, seeds = [], [], []
    profile, roots = None, {}
    flags = {"--file": "file", "--dir": "dir", "--socket": "socket", "--subtree": "subtree",
             "--stdio": "stdio", "--device": "device"}
    argv = list(argv)
    while argv:
        a = argv.pop(0)
        if a in flags or a in ("--declared-temp", "--profile", "--root", "--links-from"):
            if not argv:
                raise ValueError(f"{a} needs a value")
            v = argv.pop(0)
            if a in flags:
                decls.append(Decl(flags[a], v))
            elif a == "--declared-temp":
                if not v.endswith("XXXXXX"):
                    raise ValueError(f"a template must end in XXXXXX: {v}")
                decls.append(Decl("file", v[:-6] + "{XXXXXX}"))
            elif a == "--profile":
                profile = v
            elif a == "--root":
                k, _, p = v.partition("=")
                if not k or not p:
                    raise ValueError(f"--root wants VAR=PATH: {v}")
                roots[k] = p
            else:
                seeds.append(v)
        elif a == "--snapshot":
            rest.insert(0, a)
        elif a.startswith("--"):
            raise ValueError(f"unknown option {a}")
        else:
            rest.append(a)
    if profile is not None:
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        import declarations  # noqa: E402

        decls += [Decl(k, p) for k, p in declarations.expand(profile, roots)]
    elif roots:
        raise ValueError("--root without --profile")
    return rest, decls, seeds


def main(argv: list[str]) -> int:
    if argv == ["--strace-args"]:
        print("\n".join(strace_args()))
        return 0
    try:
        rest, decls, seeds = parse_declarations(argv)
    except (ValueError, KeyError) as e:
        print(f"trace: {e}", file=sys.stderr)
        print(__doc__, file=sys.stderr)
        return 2
    d = Declarations(decls)
    if rest[:1] == ["--snapshot"]:
        files = rest[1:]
        if len(files) != 2:
            print(__doc__, file=sys.stderr)
            return 2
        try:
            return judge_snapshot(files[0], files[1], d)
        except (OSError, ValueError, KeyError) as e:
            print(f"manifest: cannot read a snapshot: {e}", file=sys.stderr)
            return 2
    if not rest:
        print(__doc__, file=sys.stderr)
        return 2
    report = Report(d)
    try:
        for s in seeds:
            load_seed(report, s)
        for log in rest:
            report.read(log)
    except OSError as e:
        print(f"trace: cannot read {e.filename}: {e.strerror}", file=sys.stderr)
        return 2
    report.judge_paths()
    return report.print()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
