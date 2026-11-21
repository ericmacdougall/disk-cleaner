#!/usr/bin/env python3
"""The Linux write observer of the end-to-end checks (PR gate F10): every file the app and its
child processes changed, from an strace log, judged against the declared paths. The default
for a write nobody declared is failure.

usage: trace_report.py LOG [LOG...] [--declared PATH]... [--declared-temp TEMPLATE]...
                       [--ignore PATH]...

A TEMPLATE is a path whose last name ends in XXXXXX, as for mkstemp and g_mkdtemp: it allows a
file or folder whose name is the template with those six characters replaced by exactly six
letters or digits, and anything inside such a folder. For the randomly named files that GLib
writes next to a declared file and renames over it (recently-used.xbel.XXXXXX), and the folder
the accessibility bridge makes (at-spi2-XXXXXX). Nothing else about the name may differ.

The log is strace output made with -f -y (descriptors decoded to their paths), as e2e/check.sh
and e2e/idle-launch.sh write it; a log per process (-ff) works too.

A write is a successful (or unfinished: the process ended inside it) call of:
  open, openat, openat2, open_by_handle_at  with O_WRONLY, O_RDWR, O_CREAT, O_TRUNC, O_APPEND or
                                            O_TMPFILE (the returned descriptor's path is judged
                                            as well as the path asked for)
  creat, mkdir, mkdirat, mknod, mknodat, rename, renameat, renameat2 (both paths), link,
  linkat (both paths), symlink, symlinkat (the new name), unlink, unlinkat, rmdir, truncate,
  ftruncate, fallocate, chmod, fchmod, fchmodat, fchmodat2, chown, lchown, fchown, fchownat,
  utime, utimes, utimensat, futimesat, setxattr, lsetxattr, fsetxattr, removexattr,
  lremovexattr, fremovexattr
A path relative to a directory descriptor is joined to the descriptor's decoded path; a path
relative to the current folder is joined to the folder strace decoded for AT_FDCWD in that
process (chdir, fchdir and clone are followed). Each path is also judged with its folders'
symbolic links resolved, as they are when the report runs.

Each path is printed once with its kind:
  declared    a declared path or inside one (a declared path is a file or a folder), a name a
              template allows or inside it, or a folder created on the way to either (mkdir only)
  ignored     an --ignore path or inside one (exact paths and folders, never a name prefix)
  anonymous   a descriptor that names no file (memfd, socket, pipe, anon_inode)
  OTHER       anything else
  UNRESOLVED  a relative path or a descriptor the trace cannot place
  UNOBSERVABLE  io_uring was set up: its file operations bypass the trace
  UNPARSED    a line of a traced call that could not be read
Exit 0 when every write is declared, ignored or anonymous; 1 when any is OTHER, UNRESOLVED,
UNOBSERVABLE or UNPARSED, or when the log holds no traced call at all (an empty or foreign log
proves nothing); 2 on bad usage or an unreadable log.
"""

from __future__ import annotations

import os
import re
import sys
from collections import defaultdict
from pathlib import Path

OPEN_WRITE_FLAGS = {"O_WRONLY", "O_RDWR", "O_CREAT", "O_TRUNC", "O_APPEND", "O_TMPFILE"}

# call -> how its arguments name what it changes:
#   ("path", i)       a path at argument i, relative to the current folder
#   ("at", d, i)      a path at argument i, relative to the directory descriptor at argument d
#                     (an empty or NULL path means the descriptor itself)
#   ("fd", i)         the descriptor at argument i
# Opens also have the index of their flags (OPENS) and are judged only when those flags write.
CALLS: dict[str, list[tuple]] = {
    "open": [("path", 0)],
    "openat": [("at", 0, 1)],
    "openat2": [("at", 0, 1)],
    "open_by_handle_at": [],
    "creat": [("path", 0)],
    "mkdir": [("path", 0)],
    "mkdirat": [("at", 0, 1)],
    "mknod": [("path", 0)],
    "mknodat": [("at", 0, 1)],
    "rename": [("path", 0), ("path", 1)],
    "renameat": [("at", 0, 1), ("at", 2, 3)],
    "renameat2": [("at", 0, 1), ("at", 2, 3)],
    "link": [("path", 0), ("path", 1)],
    "linkat": [("at", 0, 1), ("at", 2, 3)],
    "symlink": [("path", 1)],
    "symlinkat": [("at", 1, 2)],
    "unlink": [("path", 0)],
    "unlinkat": [("at", 0, 1)],
    "rmdir": [("path", 0)],
    "truncate": [("path", 0)],
    "ftruncate": [("fd", 0)],
    "fallocate": [("fd", 0)],
    "chmod": [("path", 0)],
    "fchmod": [("fd", 0)],
    "fchmodat": [("at", 0, 1)],
    "fchmodat2": [("at", 0, 1)],
    "chown": [("path", 0)],
    "lchown": [("path", 0)],
    "fchown": [("fd", 0)],
    "fchownat": [("at", 0, 1)],
    "utime": [("path", 0)],
    "utimes": [("path", 0)],
    "utimensat": [("at", 0, 1)],
    "futimesat": [("at", 0, 1)],
    "setxattr": [("path", 0)],
    "lsetxattr": [("path", 0)],
    "fsetxattr": [("fd", 0)],
    "removexattr": [("path", 0)],
    "lremovexattr": [("path", 0)],
    "fremovexattr": [("fd", 0)],
}
OPENS = {"open": 1, "openat": 2, "openat2": 2, "open_by_handle_at": 2}
MKDIRS = {"mkdir", "mkdirat"}
CWD_CALLS = {"chdir", "fchdir"}
CLONES = {"clone", "clone3", "fork", "vfork"}
UNOBSERVABLE_CALLS = {"io_uring_setup"}
TRACED = set(CALLS) | CWD_CALLS | CLONES | UNOBSERVABLE_CALLS
# The strace -e trace= list for these calls (e2e/check.sh and e2e/idle-launch.sh use it).
# "?" lets an strace that does not know a call (fchmodat2 is Linux 6.6) start anyway; a
# program built against a C library that old does not make that call either.
STRACE_CALLS = ",".join(("?" + c) if c == "fchmodat2" else c for c in sorted(TRACED))

PID = re.compile(r"^(?:\[pid\s+(\d+)\]\s+|(\d+)\s+)")
CALL = re.compile(r"^(\w+)\((.*)\)\s+=\s+(.*)$")
START = re.compile(r"^(\w+)\(")
UNFINISHED = " <unfinished ...>"
RESUMED = re.compile(r"^<\.\.\. (\w+) resumed>(.*)$")
# A decoded descriptor: "7</path>"; strace 5.16 marks a removed file "</path (deleted)>", strace
# 6.8 "</path>(deleted)" (seen with memfd on Ubuntu 24.04).
RESULT = re.compile(r"^(\?|-?\d+|0x[0-9a-fA-F]+)(?:<(.*?)>(\(deleted\))?)?(?:\s|$)")
DESCRIPTOR = re.compile(r"^(AT_FDCWD|-?\d+)(?:<(.*)>(\(deleted\))?)?$")
ANONYMOUS = ("/memfd:", "socket:", "pipe:", "anon_inode:", "net:", "/dmabuf:")
BAD = ("OTHER", "UNRESOLVED", "UNOBSERVABLE", "UNPARSED")


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
            out += n.encode("utf-8", "surrogateescape")
            i += 2
    return out.decode("utf-8", "surrogateescape")


def split_args(s: str) -> list[str]:
    """Top-level arguments of an strace call, keeping strings, structs, arrays and decoded
    descriptors (<...>) whole."""
    args, stack, cur, i, quote = [], [], [], 0, False
    pairs = {"(": ")", "{": "}", "[": "]", "<": ">"}
    while i < len(s):
        c = s[i]
        if quote:
            cur.append(c)
            if c == "\\" and i + 1 < len(s):
                cur.append(s[i + 1])
                i += 1
            elif c == '"':
                quote = False
        elif c == '"':
            quote = True
            cur.append(c)
        elif c in pairs:
            stack.append(pairs[c])
            cur.append(c)
        elif c in pairs.values():
            if stack and stack[-1] == c:
                stack.pop()
            elif c != ">":  # a ">" that closes nothing is text (strace's "=>")
                raise ValueError("unbalanced arguments")
            cur.append(c)
        elif c == "," and not stack:
            args.append("".join(cur).strip())
            cur = []
        else:
            cur.append(c)
        i += 1
    if quote or stack:
        raise ValueError("unbalanced arguments")
    tail = "".join(cur).strip()
    if tail or args:
        args.append(tail)
    return args


def string_arg(a: str) -> str | None:
    """A quoted path (None for NULL)."""
    if a == "NULL":
        return None
    m = re.match(r'^"((?:[^"\\]|\\.)*)"(?:\.\.\.)?$', a)
    if not m:
        raise ValueError(f"not a path: {a[:60]}")
    return unescape(m.group(1))


def descriptor(a: str) -> tuple[bool, str | None]:
    """(is AT_FDCWD, decoded path or None)."""
    m = DESCRIPTOR.match(a)
    if not m:
        raise ValueError(f"not a descriptor: {a[:60]}")
    path = m.group(2)
    return m.group(1) == "AT_FDCWD", (unescape(path) if path is not None else None)


def anonymous(p: str) -> bool:
    return p.startswith(ANONYMOUS)


def strip_deleted(p: str) -> str:
    return p[: -len(" (deleted)")] if p.endswith(" (deleted)") else p


TEMPLATE_TAIL = "XXXXXX"


def template_pattern(prefix: str) -> re.Pattern[str]:
    """The names a template allows: the prefix, exactly six letters or digits, then nothing or a
    path inside that folder."""
    return re.compile(re.escape(prefix) + r"[A-Za-z0-9]{6}(?:/.*)?")


class Report:
    def __init__(self, declared: list[str], ignore: list[str], temps: list[str] | None = None):
        self.declared = self._both(declared)
        self.ignore = self._both(ignore)
        # Each template's folder, as written and with its links resolved, joined to the name's
        # fixed part; the folders count as declared folders for mkdir on the way.
        self.temp_prefixes: list[str] = []
        for t in temps or []:
            if not os.path.basename(t).endswith(TEMPLATE_TAIL):
                raise ValueError(f"a template's last name must end in {TEMPLATE_TAIL}: {t}")
            stem = os.path.basename(t)[: -len(TEMPLATE_TAIL)]
            for folder in self._both([os.path.dirname(os.path.normpath(t))]):
                self.temp_prefixes.append(os.path.join(folder, stem))
        self.temps = [template_pattern(p) for p in sorted(set(self.temp_prefixes))]
        self.seen: dict[tuple[str, str], set[str]] = defaultdict(set)
        self.cwd: dict[str, str | None] = {}
        self.calls = 0

    @staticmethod
    def _both(paths: list[str]) -> list[str]:
        out = []
        for p in paths:
            n = os.path.normpath(p)
            out += [n, os.path.realpath(n)]
        return sorted(set(out))

    @staticmethod
    def _under(p: str, roots: list[str]) -> bool:
        return any(p == r or p.startswith(r.rstrip("/") + "/") for r in roots)

    def _on_the_way(self, p: str) -> bool:
        return any(r.startswith(p.rstrip("/") + "/") for r in self.declared + self.temp_prefixes)

    def _a_temp(self, p: str) -> bool:
        return any(t.fullmatch(p) for t in self.temps)

    def kind(self, p: str, call: str) -> str:
        """The kind of one absolute path, judged as written and with its folders' links
        resolved; both must be allowed."""
        forms = {p, os.path.join(os.path.realpath(os.path.dirname(p)), os.path.basename(p))}
        kinds = set()
        for f in forms:
            if self._under(f, self.declared) or self._a_temp(f) or (call in MKDIRS and self._on_the_way(f)):
                kinds.add("declared")
            elif self._under(f, self.ignore):
                kinds.add("ignored")
            else:
                kinds.add("OTHER")
        return "OTHER" if "OTHER" in kinds else "ignored" if "ignored" in kinds else "declared"

    def note(self, kind: str, call: str, path: str) -> None:
        self.seen[(kind, path)].add(call)

    def judge(self, call: str, path: str | None, unresolved_why: str = "") -> None:
        if path is None:
            self.note("UNRESOLVED", call, unresolved_why or "(no path)")
            return
        path = strip_deleted(path)
        if anonymous(path):
            self.note("anonymous", call, path)
        elif not path.startswith("/"):
            self.note("UNRESOLVED", call, unresolved_why or path)
        else:
            path = os.path.normpath(path)
            self.note(self.kind(path, call), call, path)

    def resolve(self, pid: str, base: str | None, rel: str | None, cwd_is_base: bool) -> tuple[str | None, str]:
        """An absolute path for `rel` against `base` (a descriptor's path, or the current
        folder), or None and the reason."""
        if rel is not None and rel.startswith("/"):
            return rel, ""
        if base is None and cwd_is_base:
            base = self.cwd.get(pid)
        if base is None:
            return None, f"{rel!r} relative to a folder the trace cannot place"
        base = strip_deleted(base)
        if anonymous(base) or not base.startswith("/"):
            return None, f"{rel!r} relative to {base}"
        return (base if not rel else os.path.join(base, rel)), ""

    def line(self, pid: str, call: str, argstr: str, result: str | None) -> None:
        """One complete call; `result` is None when the process ended inside it."""
        if call not in TRACED:
            return
        self.calls += 1
        rc, retpath = None, None
        if result is not None:
            m = RESULT.match(result)
            if not m:
                raise ValueError(f"unreadable result: {result[:60]}")
            if m.group(1) != "?":
                rc = int(m.group(1), 0)
                if rc < 0:
                    return
            retpath = unescape(m.group(2)) if m.group(2) is not None else None
        if call in CLONES:
            # Only the new process's id matters; it starts in its parent's folder.
            if rc is not None and rc > 0:
                self.cwd[str(rc)] = self.cwd.get(pid)
            return
        args = split_args(argstr)
        # Every decoded AT_FDCWD tells where this process's current folder is.
        for a in args:
            if a.startswith("AT_FDCWD<"):
                self.cwd[pid] = descriptor(a)[1]
        if call in UNOBSERVABLE_CALLS:
            self.note("UNOBSERVABLE", call, f"{call} in process {pid}: io_uring file operations bypass the trace")
            return
        if call in CWD_CALLS:
            # Threads share their folder: forget every process's, then keep the one now known.
            new = None
            if call == "chdir":
                rel = string_arg(args[0])
                new, _ = self.resolve(pid, None, rel, True)
            else:
                new = descriptor(args[0])[1]
                if new is not None and not new.startswith("/"):
                    new = None
            self.cwd = {k: None for k in self.cwd}
            self.cwd[pid] = new
            return
        if call in OPENS:
            flags = set(re.split(r"[|{}=,\s]+", args[OPENS[call]])) if len(args) > OPENS[call] else set()
            numeric = any(re.fullmatch(r"0x[0-9a-fA-F]+|\d+", f) for f in flags if f)
            if not (flags & OPEN_WRITE_FLAGS) and not numeric:
                return
            if retpath is not None:
                self.judge(call, retpath)
            elif call == "open_by_handle_at":
                self.judge(call, None, "open_by_handle_at without a decoded descriptor")
        for spec in CALLS[call]:
            if spec[0] == "path":
                rel = string_arg(args[spec[1]])
                p, why = self.resolve(pid, None, rel, True)
                self.judge(call, p, why)
            elif spec[0] == "fd":
                fdcwd, p = descriptor(args[spec[1]])
                self.judge(call, p, f"descriptor {args[spec[1]]} not decoded")
            else:
                fdcwd, base = descriptor(args[spec[1]])
                rel = string_arg(args[spec[2]])
                if not rel:  # AT_EMPTY_PATH or NULL: the descriptor itself
                    self.judge(call, base, f"descriptor {args[spec[1]]} not decoded")
                    continue
                p, why = self.resolve(pid, base, rel, fdcwd)
                self.judge(call, p, why)

    def read(self, path: str) -> None:
        default_pid = path.rsplit(".", 1)[-1] if re.search(r"\.\d+$", path) else "main"
        pending: dict[tuple[str, str], str] = {}
        with open(path, errors="surrogateescape") as f:
            for raw in f:
                text = raw.rstrip("\n")
                m = PID.match(text)
                pid = (m.group(1) or m.group(2)) if m else default_pid
                text = text[m.end():] if m else text
                text = text.strip()
                if not text or text.startswith(("---", "+++", "strace:")):
                    continue
                try:
                    if text.endswith(UNFINISHED):
                        s = START.match(text)
                        if s and s.group(1) in TRACED:
                            pending[(pid, s.group(1))] = text[: -len(UNFINISHED)]
                        continue
                    r = RESUMED.match(text)
                    if r:
                        if r.group(1) not in TRACED:
                            continue
                        head = pending.pop((pid, r.group(1)), None)
                        if head is None:
                            raise ValueError("resumed without its start")
                        text = head + r.group(2)
                    c = CALL.match(text)
                    if not c:
                        s = START.match(text)
                        if s and s.group(1) in TRACED:
                            raise ValueError("not a complete call")
                        continue
                    self.line(pid, c.group(1), c.group(2), c.group(3))
                except (ValueError, IndexError) as e:
                    self.note("UNPARSED", "line", f"{text[:160]} ({e})")
        # A call the process never returned from may have taken effect: judge what it named.
        for (pid, call), head in pending.items():
            try:
                self.line(pid, call, head[len(call) + 1:].rstrip().rstrip(","), None)
            except (ValueError, IndexError) as e:
                self.note("UNPARSED", "line", f"{head[:160]} ({e})")

    def print(self) -> int:
        order = list(BAD) + ["declared", "ignored", "anonymous"]
        counts = defaultdict(int)
        for kind in order:
            for (k, p), calls in sorted(self.seen.items()):
                if k == kind:
                    counts[kind] += 1
                    print(f"{kind} {','.join(sorted(calls))}: {p}")
        bad = sum(counts[k] for k in BAD)
        print(f"trace: {self.calls} traced calls; " + ", ".join(f"{counts[k]} {k}" for k in order))
        if self.calls == 0:
            print("trace: FAIL: no traced call in the log (an empty or foreign log proves nothing)")
            return 1
        print("trace: FAIL: writes nobody declared (above)" if bad else "trace: every write is declared or ignored")
        return 1 if bad else 0


def main(argv: list[str]) -> int:
    logs: list[str] = []
    declared: list[str] = []
    temps: list[str] = []
    ignore: list[str] = []
    rest = list(argv)
    while rest:
        a = rest.pop(0)
        if a in ("--declared", "--declared-temp", "--ignore"):
            if not rest:
                print(__doc__, file=sys.stderr)
                return 2
            {"--declared": declared, "--declared-temp": temps, "--ignore": ignore}[a].append(rest.pop(0))
        elif a.startswith("--"):
            print(__doc__, file=sys.stderr)
            return 2
        else:
            logs.append(a)
    if not logs:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        report = Report(declared, ignore, temps)
    except ValueError as e:
        print(f"trace: {e}", file=sys.stderr)
        return 2
    for log in logs:
        try:
            report.read(log)
        except OSError as e:
            print(f"trace: cannot read {log}: {e.strerror}", file=sys.stderr)
            return 2
    return report.print()


if __name__ == "__main__":
    if sys.argv[1:] == ["--strace-calls"]:
        print(STRACE_CALLS)
        sys.exit(0)
    sys.exit(main(sys.argv[1:]))
