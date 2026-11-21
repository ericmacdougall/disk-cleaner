#!/usr/bin/env python3
"""Tests for the Linux write observer of the end-to-end checks: trace_report.py, declarations.py
and traced.py (PR gate F10; PR review F11, F12, F15).

  - Recorded: every negative and positive control of e2e/observer-controls.sh, as strace 5.16
    (Ubuntu 22.04), 6.1 (Debian 12) and 6.17 (Fedora 41) recorded it on Linux 6.8
    (e2e/recorded/observer-controls.json.xz, made by `observer_controls.py record`), replayed
    through the observer and the snapshot check: each must give the result
    observer_controls.EXPECT names.
  - Mutants: each fault class of the review, planted into the observer here (a call classified
    as harmless, writes judged only through descriptors the trace saw opened, an unknown
    descriptor taken as read-only, the final link of a path never followed, a folder
    declaration read as a whole subtree ...), must make a recorded control or a test give the
    wrong answer: none survives.
  - Lines: the observer's rules one by one, on strace lines written here.
  - Classifier: every system call of the kernel's table (and of this host's unistd_64.h) is
    classified, default deny.
  - Declarations: each typed declaration is tied to the src/writes.ts entry that discloses it.
  - Survivors: traced.py's end-of-session check (Linux only).

usage: python3 -m unittest -v e2e/test_trace_report.py      (from apps/desktop)
PRUNEDISK_REQUIRE_UNISTD=1 (set in CI) fails the classifier test on a host without
unistd_64.h instead of skipping it.
"""

from __future__ import annotations

import io
import json
import lzma
import os
import re
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import contextmanager, redirect_stdout
from dataclasses import replace
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import declarations  # noqa: E402
import observer_controls  # noqa: E402
import trace_report as tr  # noqa: E402
import traced  # noqa: E402

RECORDED = HERE / "recorded" / "observer-controls.json.xz"


def run_main(argv: list[str]) -> tuple[int, str]:
    out = io.StringIO()
    with redirect_stdout(out):
        code = tr.main(argv)
    return code, out.getvalue()


# ----------------------------------------------------------------------------------------------
# The recorded controls


_recorded_cache: dict | None = None


def recorded() -> dict:
    global _recorded_cache
    if _recorded_cache is None:
        _recorded_cache = json.loads(lzma.decompress(RECORDED.read_bytes()).decode("utf-8", "surrogateescape"))
    return _recorded_cache


@contextmanager
def world():
    """A folder for one replay: ctl (the control folder), tmp (the O_TMPFILE folder), run."""
    with tempfile.TemporaryDirectory() as d:
        root = os.path.realpath(d)
        for sub in ("ctl", "tmp", "run"):
            os.mkdir(f"{root}/{sub}")
        yield root


def replay(entry: dict, root: str) -> dict:
    """The trace and snapshot verdicts of one recorded control, with its folders under root."""
    ctl, tmp, run = f"{root}/ctl", f"{root}/tmp", f"{root}/run"
    files = {}
    for name, text in entry.items():
        p = f"{run}/{name}"
        with open(p, "w", errors="surrogateescape") as f:
            f.write(text.replace("@CTL@", ctl).replace("@TMP@", tmp).replace("@RUN@", run))
        files[name] = p
    decl = observer_controls.declarations(ctl)
    tcode, tout = run_main([files["trace.log"], *decl, "--links-from", files["before.json"]])
    scode, sout = run_main(["--snapshot", files["before.json"], files["after.json"], *decl])
    return {"trace": tcode, "trace.txt": tout, "snapshot": scode, "manifest.txt": sout, "ctl": ctl, "tmp": tmp}


def gives_its_result(action: str, r: dict) -> tuple[bool, str]:
    """Whether a replay gives observer_controls.EXPECT's result, as observer-controls.sh judges it."""
    want = observer_controls.EXPECT[action]
    kind, _, rest = want.partition(" ")
    rest = rest.replace("CTL", r["ctl"]).replace("TMP", r["tmp"])
    lines = r["trace.txt"].splitlines()
    if kind == "pass":
        ok = r["trace"] == 0 and r["snapshot"] == 0
    elif kind == "TRACE":
        k, _, p = rest.partition(" ")
        ok = r["trace"] == 1 and any(ln.startswith(k + " ") and ln.endswith(": " + p) for ln in lines)
    elif kind == "TRACE-PREFIX":
        ok = r["trace"] == 1 and rest in r["trace.txt"]
    elif kind == "SNAPSHOT":
        ok = r["snapshot"] == 1 and rest in r["manifest.txt"].splitlines()
    else:
        raise AssertionError(f"{action}: {want} is not replayable")
    bad = [ln for ln in lines if re.match(r"(LATE|LOST|UNKNOWN|UNSUPPORTED|UNPARSED|UNRESOLVED|OTHER) ", ln)]
    snap = [ln for ln in r["manifest.txt"].splitlines() if ln.startswith("UNDECLARED")]
    return ok, f"wanted {want}; trace exit {r['trace']}, snapshot exit {r['snapshot']}; trace: {bad[:6]}; snapshot: {snap[:4]}"


def wrong_answers(actions=None, labels=None) -> list[str]:
    """The recorded controls (label/action) that do not give their result."""
    out = []
    for label, fix in sorted(recorded().items()):
        if labels and label not in labels:
            continue
        for action, entry in sorted(fix["actions"].items()):
            if actions is not None and action not in actions:
                continue
            with world() as root:
                ok, _ = gives_its_result(action, replay(entry, root))
            if not ok:
                out.append(f"{label}/{action}")
    return out


class Recorded(unittest.TestCase):
    """The controls as three versions of strace recorded them (observer-controls.sh on t2-dev)."""

    def test_the_fixture_has_every_control_from_three_strace_versions(self):
        fix = recorded()
        self.assertEqual(len(fix), 3, list(fix))
        versions = {re.search(r"version (\S+);", v["strace"]).group(1) for v in fix.values()}
        self.assertEqual(versions, {"5.16", "6.1", "6.17"})
        want = set(observer_controls.EXPECT) - set(observer_controls.NOT_RECORDED)
        for label, v in fix.items():
            self.assertEqual(set(v["actions"]), want, label)

    def test_every_control_gives_its_result(self):
        for label, fix in sorted(recorded().items()):
            for action, entry in sorted(fix["actions"].items()):
                with self.subTest(strace=label, control=action), world() as root:
                    ok, why = gives_its_result(action, replay(entry, root))
                    self.assertTrue(ok, why)

    def test_a_planted_control_is_caught_by_the_exact_line_not_by_noise(self):
        # A negative control passes only through its own finding: its log, cut to the lines
        # before the planted call, must pass every check (no other finding hides a miss).
        cut = {"bind": "bind(", "inherited-write": 'write(3<', "symlink-truncate": "truncate(",
               "mkdir-only-write": "undisclosed.txt"}
        for label, fix in sorted(recorded().items()):
            for action, marker in cut.items():
                entry = dict(fix["actions"][action])
                lines = entry["trace.log"].splitlines(keepends=True)
                i = next(n for n, ln in enumerate(lines) if marker in ln)
                pid = lines[0].split()[0]
                entry["trace.log"] = "".join(lines[:i]) + f"{pid} 1700000000.000000 +++ exited with 0 +++\n"
                entry["after.json"] = entry["before.json"]
                with self.subTest(strace=label, control=action), world() as root:
                    r = replay(entry, root)
                    self.assertEqual(r["trace"], 0, r["trace.txt"][-2000:])


# ----------------------------------------------------------------------------------------------
# Mutants: each fault class of the review, planted here, must be caught.


CALL = re.compile(r"^\S+\s+[\d.]+\s+([a-z_][a-z0-9_]*)\((.*)\) += (?!-1 )")


def calls_in(text: str) -> set[str]:
    """Names of successful calls in a log."""
    return {m.group(1) for m in map(CALL.match, text.splitlines()) if m}


def write_calls_in_controls() -> dict[str, set[str]]:
    """Every call that can change a file and that a negative control makes on its own folder
    (not the interpreter's start), with those controls."""
    writers = set(tr.FD_WRITE) | tr.PATH_CALLS | set(tr.OPENS) | {"mmap"}
    out: dict[str, set[str]] = {}
    for fix in recorded().values():
        for action, entry in fix["actions"].items():
            if not observer_controls.EXPECT[action].startswith(("TRACE OTHER", "TRACE-PREFIX UNRESOLVED")):
                continue
            for line in entry["trace.log"].splitlines():
                m = CALL.match(line)
                if m and m.group(1) in writers and re.search(r"@CTL@|@TMP@|/proc/self/fd/", line):
                    out.setdefault(m.group(1), set()).add(action)
    return out


@contextmanager
def harmless(call: str):
    """The mutant: `call` classified as changing nothing (moved into NONE)."""
    with mock.patch.object(tr, "NONE", tr.NONE | {call}):
        yield


class Mutants(unittest.TestCase):
    def assertKilled(self, mutant, actions, labels=None):
        before = wrong_answers(actions, labels)
        self.assertEqual(before, [], "the controls must give their results without the mutant")
        with mutant:
            after = wrong_answers(actions, labels)
        self.assertNotEqual(after, [], "the mutant survived: every control still gives its result")
        return after

    # -- a removed syscall class (F11) --------------------------------------------------------

    def test_every_write_call_the_controls_make_is_load_bearing(self):
        # Each call that can change a file and that a negative control makes, classified as
        # harmless, makes one of those controls miss its write.
        seen = write_calls_in_controls()
        required = {"bind", "write", "pwritev2", "ftruncate", "fallocate", "sendfile", "copy_file_range", "splice",
                    "truncate", "chmod", "utimensat", "setxattr", "mknodat", "rename", "link", "linkat", "openat",
                    "mmap", "unlink", "mkdir", "rmdir", "symlink"}
        self.assertLessEqual(required, set(seen), sorted(required - set(seen)))
        survivors = {}
        for call, actions in sorted(seen.items()):
            with harmless(call):
                if not wrong_answers(actions):
                    survivors[call] = sorted(actions)
        self.assertEqual(survivors, {}, "mutants that survived (call: the controls that make it)")

    def test_a_call_removed_from_every_class_fails_as_unknown(self):
        # Default deny: the positive control's own calls, each made unknown, fail it.
        log = recorded()["fedora-41"]["actions"]["positive"]["trace.log"]
        for call in sorted(calls_in(log) - set(tr.HOT)):
            with self.subTest(call=call), mock.patch.object(tr, "KNOWN", tr.KNOWN - {call}):
                with world() as root:
                    r = replay(recorded()["fedora-41"]["actions"]["positive"], root)
                self.assertEqual(r["trace"], 1)
                self.assertIn(f"UNKNOWN {call}: ", r["trace.txt"])

    # -- a removed descriptor rule (F11) --------------------------------------------------------

    def test_writes_judged_only_through_descriptors_the_trace_saw_opened(self):
        # The 87221db observer's omission: inherited, duplicated and received descriptors.
        orig = tr.LogState.fd_change

        def only_seen(self, t, call, op, a):
            n, _ = tr.fd_arg(a)
            if n != "AT_FDCWD" and int(n) not in t.table.fds:
                return
            orig(self, t, call, op, a)

        killed = self.assertKilled(mock.patch.object(tr.LogState, "fd_change", only_seen),
                                   ["inherited-write", "pwritev", "fallocate", "fork-write", "thread-write", "exec-write"])
        self.assertTrue(any(k.endswith("/inherited-write") for k in killed), killed)

    def test_a_descriptor_the_trace_did_not_see_made_taken_as_read_only(self):
        self.assertKilled(mock.patch.object(tr.Table, "get", lambda self, n: self.fds.get(n) or tr.Fd(None, "r", None)),
                          ["mmap-shared-readonly-prot"])

    def test_strace_decoding_of_descriptors_ignored(self):
        orig = tr.LogState.target

        def table_only(self, t, a):
            num, obj = tr.fd_arg(a)
            if num == "AT_FDCWD":
                return orig(self, t, a)
            return int(num), t.table.get(int(num)).obj

        self.assertKilled(mock.patch.object(tr.LogState, "target", table_only), ["inherited-write", "scm-rights", "dup-write"])

    # -- parent-only resolution (F12) --------------------------------------------------------

    def test_the_final_link_of_a_path_never_followed(self):
        orig = tr.Namespace.resolve
        killed = self.assertKilled(
            mock.patch.object(tr.Namespace, "resolve", lambda self, o, rep: orig(self, replace(o, follow=False), rep)),
            ["symlink-truncate", "symlink-chmod", "transient-symlink"])
        for a in ("symlink-truncate", "symlink-chmod", "transient-symlink"):
            self.assertTrue(any(k.endswith("/" + a) for k in killed), (a, killed))

    def test_a_call_that_does_not_follow_links_followed(self):
        orig = tr.Namespace.resolve
        self.assertKilled(
            mock.patch.object(tr.Namespace, "resolve", lambda self, o, rep: orig(self, replace(o, follow=True), rep)),
            ["symlink-lchown-passes"])

    def test_links_the_trace_made_forgotten(self):
        orig = tr.Namespace.apply

        def no_links(self, o, p):
            if o.op != "symlink":
                orig(self, o, p)

        self.assertKilled(mock.patch.object(tr.Namespace, "apply", no_links), ["transient-symlink", "transient-dir-symlink"])

    def test_a_rename_that_does_not_move_what_the_trace_knows(self):
        # No recorded control renames a link: Lines.test_a_rename_moves_a_link_with_it is the killer.
        with mock.patch.object(tr.Namespace, "move", lambda self, src, dst: None):
            self.assertFalse(Lines.rename_moves_link_caught())

    # -- declarations broader than the disclosure (F15) ----------------------------------------

    def test_a_folder_declaration_read_as_a_subtree(self):
        orig = tr.Decl.matches

        def dir_as_subtree(self, path, tgid):
            if self.kind == "dir":
                return path == self.path or path.startswith(self.path + "/")
            return orig(self, path, tgid)

        @contextmanager
        def mutant():
            with mock.patch.object(tr, "OPS_ALLOWED", dict(tr.OPS_ALLOWED, dir=tr.ALL_OPS)), \
                    mock.patch.object(tr.Decl, "matches", dir_as_subtree):
                yield

        self.assertKilled(mutant(), ["mkdir-only-write", "mkdir-only-chmod"])

    def test_a_template_that_matches_any_length(self):
        with mock.patch.dict(tr.PLACEHOLDERS, {"{XXXXXX}": r"[A-Za-z0-9]+"}):
            self.assertFalse(Lines.template_neighbours_caught())

    def test_the_output_capture_writable_by_path(self):
        orig = tr.Declarations.judge

        def by_path(self, op, path, via, tgid, excl=False):
            return orig(self, op, path, "fd" if via == "path" else via, tgid, excl)

        with mock.patch.object(tr.Declarations, "judge", by_path):
            self.assertFalse(Lines.stdio_by_path_caught())

    def test_a_writer_other_than_the_pid_in_a_temporary_name(self):
        with mock.patch.dict(tr.PLACEHOLDERS, {"{PID}": r"[0-9]+"}):
            self.assertFalse(Lines.pid_template_caught())


# ----------------------------------------------------------------------------------------------
# The rules, on lines written here


class Lines(unittest.TestCase):
    """Each world is a temporary folder: SB the declared area, U a user's files outside it,
    RUN a runtime folder. A path call is resolved link by link there."""

    @staticmethod
    @contextmanager
    def world():
        with tempfile.TemporaryDirectory() as d:
            w = os.path.realpath(d)
            for p in ("sb/data/app", "sb/home/exports", "sb/subtree", "u/elsewhere", "u/project", "run", "conf"):
                os.makedirs(f"{w}/{p}")
            for p in ("u/victim.txt", "u/secret.txt", "sb/declared.txt"):
                Path(f"{w}/{p}").write_text("x")
            os.symlink(f"{w}/u/victim.txt", f"{w}/sb/declared-link")
            yield w

    @staticmethod
    def decl(w: str) -> list[str]:
        return ["--subtree", f"{w}/sb/data/app", "--file", f"{w}/sb/home/exports/plan.json", "--device", "/dev/null",
                "--file", f"{w}/sb/declared.txt", "--file", f"{w}/sb/declared-link", "--dir", f"{w}/sb/gtk-3.0",
                "--subtree", f"{w}/sb/subtree", "--stdio", f"{w}/stdout.log",
                "--file", f"{w}/sb/data/recently-used.xbel", "--file", f"{w}/sb/data/recently-used.xbel.{{XXXXXX}}",
                "--dir", f"{w}/run/at-spi2-{{XXXXXX}}", "--socket", f"{w}/run/at-spi2-{{XXXXXX}}/socket",
                "--file", f"{w}/conf/config.toml.{{PID}}.{{N}}.tmp"]

    @classmethod
    def report(cls, w: str, lines: list[str], *extra: str, start: bool = True, end: bool = True) -> tuple[int, str]:
        """The verdict on a log of 1234: its execve first, then `lines`, then its exit."""
        body = []
        if start:
            body.append('1234 1.000000 execve("/app", ["app"], 0x7ffd /* 3 vars */) = 0')
        body += [ln if re.match(r"^\d+ [\d.]+ ", ln) else re.sub(r"^(\d+)\s+", r"\1 2.000000 ", ln) for ln in lines]
        pids = sorted({ln.split()[0] for ln in body if re.match(r"^\d+ ", ln)})
        if end:
            body += [f"{p} 9.000000 +++ exited with 0 +++" for p in pids]
        with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as f:
            f.write("\n".join(body) + "\n")
            name = f.name
        try:
            return run_main([name, *cls.decl(w), *extra])
        finally:
            os.unlink(name)

    def check(self, lines, code, *patterns, absent=(), **kw):
        with self.world() as w:
            lines = [ln.replace("$W", w) for ln in lines]
            got, out = self.report(w, lines, **kw)
            self.assertEqual(got, code, out)
            for p in patterns:
                self.assertRegex(out, "(?m)^" + p.replace("$W", re.escape(w)) + "$", out)
            for p in absent:
                self.assertNotRegex(out, "(?m)^" + p.replace("$W", re.escape(w)), out)
            return out

    # Killers for the mutants above that no recorded control reaches.
    @classmethod
    def caught(cls, lines, want: str) -> bool:
        """Whether the log fails with exactly the line `want` ($W the world)."""
        with cls.world() as w:
            code, out = cls.report(w, [ln.replace("$W", w) for ln in lines])
            return code == 1 and want.replace("$W", w) in out.splitlines()

    @classmethod
    def rename_moves_link_caught(cls) -> bool:
        return cls.caught(['1234  symlink("$W/u/victim.txt", "$W/sb/subtree/a") = 0',
                           '1234  rename("$W/sb/subtree/a", "$W/sb/subtree/b") = 0',
                           '1234  truncate("$W/sb/subtree/b", 0) = 0'], "OTHER truncate: $W/u/victim.txt")

    @classmethod
    def template_neighbours_caught(cls) -> bool:
        return all(cls.caught([f'1234  openat(AT_FDCWD</>, "{p}", O_WRONLY|O_CREAT, 0644) = 9<{p}>'], f"OTHER openat: {p}")
                   for p in ("$W/sb/data/recently-used.xbel.Q7ZK2", "$W/sb/data/recently-used.xbel.Q7ZK2BX"))

    @classmethod
    def stdio_by_path_caught(cls) -> bool:
        return cls.caught(['1234  openat(AT_FDCWD</>, "$W/stdout.log", O_WRONLY|O_TRUNC) = 5<$W/stdout.log>'],
                          "OTHER openat: $W/stdout.log")

    @classmethod
    def pid_template_caught(cls) -> bool:
        return cls.caught(['1234  openat(AT_FDCWD</>, "$W/conf/config.toml.999.0.tmp", O_WRONLY|O_CREAT|O_EXCL, 0600)'
                           ' = 7<$W/conf/config.toml.999.0.tmp>'], "OTHER openat: $W/conf/config.toml.999.0.tmp")

    # -- default deny ------------------------------------------------------------------------

    def test_declared_writes_and_reads_pass(self):
        self.check(['1234  openat(AT_FDCWD</>, "$W/sb/data/app/x.db", O_RDWR|O_CREAT|O_CLOEXEC, 0644) = 7<$W/sb/data/app/x.db>',
                    '1234  openat(AT_FDCWD</>, "/etc/hosts", O_RDONLY|O_CLOEXEC) = 8</etc/hosts>',
                    '1234  openat(AT_FDCWD</>, "/dev/null", O_WRONLY|O_CREAT) = 9</dev/null>',
                    '1234  write(9</dev/null>, "x", 1) = 1',
                    '1234  openat(AT_FDCWD</>, "$W/u/x", O_WRONLY|O_CREAT, 0644) = -1 EACCES (Permission denied)'],
                   0, absent=["OTHER"])

    def test_an_undeclared_write_fails(self):
        self.check(['1234  openat(AT_FDCWD</>, "/tmp/prunedisk-planted-outside", O_WRONLY|O_CREAT|O_TRUNC, 0644) = 7</tmp/prunedisk-planted-outside>'],
                   1, r"OTHER openat: /tmp/prunedisk-planted-outside")

    def test_a_call_the_observer_does_not_know_fails(self):
        self.check(['1234  frobnicate(1, 2) = 0'], 1, r"UNKNOWN frobnicate: .*")

    def test_a_call_strace_printed_by_number(self):
        # 451 cachestat changes nothing; 452 fchmodat2 can, and its arguments are numbers.
        self.check(['1234  syscall_0x1c3(0x3, 0, 0, 0, 0, 0) = 0'], 0)
        self.check(['1234  syscall_0x1c4(0xffffff9c, 0x7ffd, 0x1ff, 0, 0, 0) = 0'], 1, r"UNRESOLVED fchmodat2: .*")
        self.check(['1234  syscall_0x1c4(0xffffff9c, 0x7ffd, 0x1ff, 0, 0, 0) = -1 ENOENT (No such file or directory)'], 0)
        self.check(['1234  syscall_0x3e8(0, 0, 0, 0, 0, 0) = -1 ENOSYS (Function not implemented)'], 1, r"UNKNOWN syscall_0x3e8: .*")

    def test_effects_the_trace_cannot_follow_fail(self):
        for line, call in (('1234  io_uring_setup(8, {flags=0}) = 5<anon_inode:[io_uring]>', "io_uring_setup"),
                           ('1234  io_submit(0x7f00, 1, [{aio_lio_opcode=IOCB_CMD_PWRITE}]) = 1', "io_submit"),
                           ('1234  ptrace(PTRACE_POKEDATA, 99, 0x1000, 0x1) = 0', "ptrace"),
                           ('1234  mount("none", "/mnt", "tmpfs", 0, NULL) = 0', "mount"),
                           ('1234  seccomp(SECCOMP_SET_MODE_FILTER, 0, {len=4, filter=0x5600}) = 0', "seccomp"),
                           ('1234  prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, {len=4, filter=0x5600}) = 0', "prctl"),
                           ('1234  unshare(CLONE_NEWNS) = 0', "unshare"),
                           ('1234  fcntl(3</x>, F_NEWTHING, 0) = 0', "fcntl")):
            with self.subTest(call=call):
                self.check([line], 1, rf"UNSUPPORTED {call}: .*")
        self.check(['1234  io_uring_setup(8, {flags=0}) = -1 EPERM (Operation not permitted)'], 0)

    def test_a_line_that_cannot_be_read_fails(self):
        self.check(['1234  openat(AT_FDCWD</>, "/home/u/x", O_WRONLY = 3'], 1, r"UNPARSED .*")
        self.check(['1234  this line is not strace\'s'], 1, r"UNPARSED line: .*")

    def test_a_log_that_misses_the_start_or_an_end_fails(self):
        self.check(['1234  close(3) = 0'], 1, r"LATE close: .*", start=False)
        self.check(['1234  close(3) = 0'], 1, r"LOST trace: process 1234 .*", end=False)
        self.check(['1234  close(3) = 0', '1234  strace: Process 1234 detached'], 1, r"LOST trace: strace detached from process 1234.*")

    def test_an_empty_log_fails(self):
        with self.world() as w:
            code, out = self.report(w, [], start=False, end=False)
        self.assertEqual(code, 1, out)
        self.assertIn("no call in the logs", out)

    # -- descriptors (F11) --------------------------------------------------------------------

    def test_a_write_through_an_inherited_or_copied_descriptor_fails(self):
        for lines in (['1234  write(3<$W/u/secret.txt>, "x", 1) = 1'],
                      ['1234  dup2(3<$W/u/secret.txt>, 9) = 9<$W/u/secret.txt>', '1234  pwrite64(9<$W/u/secret.txt>, "x", 1, 0) = 1'],
                      ['1234  fcntl(3<$W/u/secret.txt>, F_DUPFD_CLOEXEC, 20) = 20<$W/u/secret.txt>', '1234  ftruncate(20<$W/u/secret.txt>, 0) = 0'],
                      ['1234  sendfile(4<$W/u/secret.txt>, 5</etc/hosts>, NULL, 4) = 4'],
                      ['1234  copy_file_range(5</etc/hosts>, NULL, 4<$W/u/secret.txt>, NULL, 4, 0) = 4'],
                      ['1234  splice(6<pipe:[1]>, NULL, 4<$W/u/secret.txt>, NULL, 6, 0) = 6'],
                      ['1234  fchmod(7<$W/u/secret.txt>, 0600) = 0'],
                      ['1234  fsetxattr(7<$W/u/secret.txt>, "user.x", "1", 1, 0) = 0']):
            with self.subTest(lines=lines):
                self.check(lines, 1, r"OTHER \w+: $W/u/secret.txt")

    def test_a_child_or_thread_writes_through_the_parents_descriptor(self):
        self.check(['1234  clone(child_stack=NULL, flags=CLONE_CHILD_CLEARTID|CLONE_CHILD_SETTID|SIGCHLD) = 1235',
                    '1235  write(3<$W/u/secret.txt>, "x", 1) = 1'], 1, r"OTHER write: $W/u/secret.txt")

    def test_a_shared_map_of_a_descriptor_that_may_write_fails(self):
        # Inherited (mode unknown), received over a socket, or opened read-write: mprotect can
        # make the map writable later with no call that names the file.
        self.check(['1234  mmap(NULL, 4096, PROT_READ, MAP_SHARED, 4<$W/u/secret.txt>, 0) = 0x7f00'], 1, r"OTHER mmap: $W/u/secret.txt")
        self.check(['1234  recvmsg(5<socket:[9]>, {msg_name=NULL, msg_namelen=0, msg_iov=[{iov_base="x", iov_len=1}], msg_iovlen=1, '
                    'msg_control=[{cmsg_len=20, cmsg_level=SOL_SOCKET, cmsg_type=SCM_RIGHTS, cmsg_data=[7<$W/u/secret.txt>]}], '
                    'msg_controllen=24, msg_flags=0}, 0) = 1',
                    '1234  mmap(NULL, 4096, PROT_READ, MAP_SHARED, 7<$W/u/secret.txt>, 0) = 0x7f00'], 1, r"OTHER mmap: $W/u/secret.txt")
        # Opened read-only here, or a private map: no write can reach the file.
        self.check(['1234  openat(AT_FDCWD</>, "$W/u/secret.txt", O_RDONLY) = 4<$W/u/secret.txt>',
                    '1234  mmap(NULL, 4096, PROT_READ, MAP_SHARED, 4<$W/u/secret.txt>, 0) = 0x7f00',
                    '1234  mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE, 3<$W/u/victim.txt>, 0) = 0x7f00'], 0)

    def test_exec_closes_close_on_exec_descriptors(self):
        # A read-only descriptor kept across exec stays read-only; one marked close-on-exec is
        # gone, and a later descriptor of that number is one the trace did not see made.
        keep = ['1234  openat(AT_FDCWD</>, "$W/u/secret.txt", O_RDONLY) = 5<$W/u/secret.txt>',
                '1234  execve("/bin/sh", ["sh"], 0x7ffd /* 3 vars */) = 0',
                '1234  mmap(NULL, 4096, PROT_READ, MAP_SHARED, 5<$W/u/secret.txt>, 0) = 0x7f00']
        self.check(keep, 0)
        self.check([keep[0].replace("O_RDONLY", "O_RDONLY|O_CLOEXEC"), *keep[1:]], 1, r"OTHER mmap: $W/u/secret.txt")

    def test_a_proc_fd_path_is_the_descriptors_object(self):
        self.check(['1234  truncate("/proc/self/fd/3", 0) = 0'], 1, r"UNRESOLVED truncate: /proc/self/fd/3 .*")
        self.check(['1234  openat(AT_FDCWD</>, "$W/sb/declared.txt", O_RDONLY) = 7<$W/sb/declared.txt>',
                    '1234  truncate("/proc/self/fd/7", 0) = 0'], 0)
        self.check(['1234  openat(AT_FDCWD</>, "$W/u/secret.txt", O_RDONLY) = 7<$W/u/secret.txt>',
                    '1234  truncate("/proc/self/fd/7", 0) = 0'], 1, r"OTHER truncate: $W/u/secret.txt")

    def test_a_unix_socket_bound_to_a_path_is_a_new_file(self):
        self.check(['1234  bind(3<socket:[1]>, {sa_family=AF_UNIX, sun_path="$W/u/x.sock"}, 110) = 0'], 1, r"OTHER bind: $W/u/x.sock")
        self.check(['1234  bind(3<socket:[1]>, {sa_family=AF_UNIX, sun_path=@"prunedisk"}, 12) = 0',
                    '1234  bind(3<socket:[1]>, {sa_family=AF_UNIX}, 2) = 0',
                    '1234  bind(3<socket:[1]>, {sa_family=AF_INET, sin_port=htons(0), sin_addr=inet_addr("127.0.0.1")}, 16) = 0'], 0)

    def test_ioctl_on_a_file_fails_unless_it_only_reads(self):
        self.check(['1234  ioctl(7<$W/u/secret.txt>, FS_IOC_SETFLAGS, [FS_IMMUTABLE_FL]) = 0'], 1, r"OTHER ioctl FS_IOC_SETFLAGS: $W/u/secret.txt")
        self.check(['1234  ioctl(7<$W/u/secret.txt>, FS_IOC_GETFLAGS, [0]) = 0', '1234  ioctl(1</dev/pts/0>, TIOCSWINSZ, {ws_row=1}) = 0'], 0)

    # -- links (F12) --------------------------------------------------------------------------

    def test_a_call_that_follows_a_link_is_judged_by_its_target(self):
        for line in ('1234  truncate("$W/sb/declared-link", 0) = 0',
                     '1234  chmod("$W/sb/declared-link", 0600) = 0',
                     '1234  setxattr("$W/sb/declared-link", "user.x", "1", 1, 0) = 0',
                     '1234  utimensat(AT_FDCWD</>, "$W/sb/declared-link", NULL, 0) = 0',
                     '1234  fchmodat(AT_FDCWD</>, "$W/sb/declared-link", 0600) = 0'):
            with self.subTest(line=line):
                self.check([line], 1, r"OTHER \w+: $W/u/victim.txt")

    def test_a_call_that_does_not_follow_a_link_changes_the_link(self):
        self.check(['1234  lchown("$W/sb/declared-link", 1000, 1000) = 0',
                    '1234  utimensat(AT_FDCWD</>, "$W/sb/declared-link", NULL, AT_SYMLINK_NOFOLLOW) = 0',
                    '1234  fchownat(AT_FDCWD</>, "$W/sb/declared-link", 1000, 1000, AT_SYMLINK_NOFOLLOW) = 0',
                    '1234  lsetxattr("$W/sb/declared-link", "user.x", "1", 1, 0) = 0'], 0)

    def test_a_link_the_trace_made_and_removed_is_followed(self):
        self.check(['1234  symlink("$W/u/victim.txt", "$W/sb/subtree/l") = 0',
                    '1234  truncate("$W/sb/subtree/l", 0) = 0',
                    '1234  unlink("$W/sb/subtree/l") = 0'], 1, r"OTHER truncate: $W/u/victim.txt")

    def test_a_link_in_the_middle_of_a_path_is_followed(self):
        self.check(['1234  symlinkat("$W/u", AT_FDCWD</>, "$W/sb/subtree/d") = 0',
                    '1234  mkdir("$W/sb/subtree/d/newdir", 0777) = 0',
                    '1234  unlink("$W/sb/subtree/d") = 0'], 1, r"OTHER mkdir: $W/u/newdir")

    def test_a_rename_moves_a_link_with_it(self):
        self.assertTrue(self.rename_moves_link_caught())
        out = self.check(['1234  symlink("$W/u/victim.txt", "$W/sb/subtree/a") = 0',
                          '1234  rename("$W/sb/subtree/a", "$W/sb/subtree/b") = 0',
                          '1234  truncate("$W/sb/subtree/b", 0) = 0'], 1, r"OTHER truncate: $W/u/victim.txt")
        self.assertNotRegex(out, r"(?m)^UNRESOLVED ")

    def test_an_exchange_swaps_what_the_names_are(self):
        self.check(['1234  symlink("$W/u/victim.txt", "$W/sb/subtree/a") = 0',
                    '1234  openat(AT_FDCWD</>, "$W/sb/subtree/b", O_WRONLY|O_CREAT, 0644) = 5<$W/sb/subtree/b>',
                    '1234  renameat2(AT_FDCWD</>, "$W/sb/subtree/a", AT_FDCWD</>, "$W/sb/subtree/b", RENAME_EXCHANGE) = 0',
                    '1234  truncate("$W/sb/subtree/b", 0) = 0',
                    '1234  truncate("$W/sb/subtree/a", 0) = 0'], 1, r"OTHER truncate: $W/u/victim.txt",
                   r"declared truncate: $W/sb/subtree/a")

    def test_a_hard_link_to_a_link_is_that_link(self):
        self.check(['1234  symlink("$W/u/victim.txt", "$W/sb/subtree/a") = 0',
                    '1234  link("$W/sb/subtree/a", "$W/sb/subtree/b") = 0',
                    '1234  truncate("$W/sb/subtree/b", 0) = 0'], 1, r"OTHER truncate: $W/u/victim.txt")

    def test_a_name_the_trace_changes_is_not_taken_from_the_disk_as_it_is_now(self):
        # The name is removed later in the trace and was not seen made: whether it was a link
        # when it was truncated cannot be known.
        self.check(['1234  truncate("$W/sb/subtree/gone", 0) = 0',
                    '1234  unlink("$W/sb/subtree/gone") = 0'], 1, r"UNRESOLVED truncate: $W/sb/subtree/gone: .*")

    def test_a_folder_on_the_way_is_resolved_too(self):
        # u/elsewhere replaced by a link to u/project between two calls.
        self.check(['1234  rmdir("$W/u/elsewhere") = 0'], 1, r"OTHER rmdir: $W/u/elsewhere")
        with self.world() as w:
            os.rmdir(f"{w}/sb/subtree")
            os.symlink(f"{w}/u", f"{w}/sb/subtree")
            code, out = self.report(w, [f'1234  mkdir("{w}/sb/subtree/x", 0777) = 0'])
            self.assertEqual(code, 1, out)
            self.assertIn(f"OTHER mkdir: {w}/u/x", out)

    # -- typed declarations (F15) -------------------------------------------------------------

    def test_a_folder_declaration_allows_its_creation_only(self):
        self.check(['1234  mkdir("$W/sb/gtk-3.0", 0700) = 0'], 0)
        for line in ('1234  openat(AT_FDCWD</>, "$W/sb/gtk-3.0/x", O_WRONLY|O_CREAT, 0644) = 5<$W/sb/gtk-3.0/x>',
                     '1234  mkdir("$W/sb/gtk-3.0/sub", 0700) = 0',
                     '1234  chmod("$W/sb/gtk-3.0", 0777) = 0',
                     '1234  rmdir("$W/sb/gtk-3.0") = 0'):
            with self.subTest(line=line):
                self.check(['1234  mkdir("$W/sb/gtk-3.0", 0700) = 0', line], 1, r"OTHER \w+: $W/sb/gtk-3\.0.*")

    def test_a_file_declaration_is_that_file(self):
        self.check(['1234  openat(AT_FDCWD</>, "$W/sb/declared.txt", O_WRONLY|O_TRUNC) = 5<$W/sb/declared.txt>',
                    '1234  unlink("$W/sb/declared.txt") = 0'], 0)
        for p in ("$W/sb/declared.txt.bak", "$W/sb/declared.tx", "$W/sb/declared.txt/x"):
            self.check([f'1234  openat(AT_FDCWD</>, "{p}", O_WRONLY|O_CREAT, 0644) = 5<{p}>'], 1, rf"OTHER openat: {re.escape(p)}")
        self.check(['1234  mkdir("$W/sb/declared.txt", 0700) = 0'], 1, r"OTHER mkdir: $W/sb/declared.txt")

    def test_a_subtree_allows_anything_inside_and_nothing_beside(self):
        self.check(['1234  mkdir("$W/sb/subtree/a", 0700) = 0',
                    '1234  openat(AT_FDCWD</>, "$W/sb/subtree/a/c", O_WRONLY|O_CREAT, 0644) = 5<$W/sb/subtree/a/c>',
                    '1234  symlink("c", "$W/sb/subtree/a/l") = 0', '1234  unlink("$W/sb/subtree/a/l") = 0',
                    '1234  rename("$W/sb/subtree/a/c", "$W/sb/subtree/a/d") = 0'], 0)
        self.check(['1234  openat(AT_FDCWD</>, "$W/sb/subtree2", O_WRONLY|O_CREAT, 0644) = 5<$W/sb/subtree2>'], 1,
                   r"OTHER openat: $W/sb/subtree2")

    def test_both_names_of_a_rename_or_link_are_judged(self):
        self.check(['1234  rename("$W/u/secret.txt", "$W/sb/declared.txt") = 0'], 1, r"OTHER rename: $W/u/secret.txt")
        self.check(['1234  rename("$W/sb/declared.txt", "$W/u/new.txt") = 0'], 1, r"OTHER rename: $W/u/new.txt")
        self.check(['1234  link("$W/u/secret.txt", "$W/sb/declared.txt") = 0'], 1, r"OTHER link: $W/u/secret.txt")

    def test_temporary_names(self):
        self.check(['1234  openat(AT_FDCWD</>, "$W/sb/data/recently-used.xbel.Q7ZK2B", O_RDWR|O_CREAT|O_EXCL, 0666) = 9<$W/sb/data/recently-used.xbel.Q7ZK2B>',
                    '1234  fallocate(9<$W/sb/data/recently-used.xbel.Q7ZK2B>, 0, 0, 762) = 0',
                    '1234  rename("$W/sb/data/recently-used.xbel.Q7ZK2B", "$W/sb/data/recently-used.xbel") = 0',
                    '1234  mkdir("$W/run/at-spi2-aB3dE9", 0700) = 0',
                    '1234  bind(3<socket:[1]>, {sa_family=AF_UNIX, sun_path="$W/run/at-spi2-aB3dE9/socket"}, 110) = 0',
                    '1234  openat(AT_FDCWD</>, "$W/conf/config.toml.1234.0.tmp", O_WRONLY|O_CREAT|O_EXCL, 0600) = 7<$W/conf/config.toml.1234.0.tmp>'], 0)
        self.assertTrue(self.template_neighbours_caught())

    # A file made new (click-test N2, the plan export): the name is stamped and the folder may
    # hold other plans, so "--file" with the template would excuse replacing one of them.
    PLAN = "$W/sb/home/exports/prunedisk-plan-20261121-093141"
    EXPORT = ('1234  openat(AT_FDCWD</>, "$W/sb/home/exports/prunedisk-plan-20261121-093141-2.json", '
              'O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC, 0666) = 5<$W/sb/home/exports/prunedisk-plan-20261121-093141-2.json>',
              '1234  write(5<$W/sb/home/exports/prunedisk-plan-20261121-093141-2.json>, "{\\n", 2) = 2',
              '1234  fsync(5<$W/sb/home/exports/prunedisk-plan-20261121-093141-2.json>) = 0',
              '1234  close(5<$W/sb/home/exports/prunedisk-plan-20261121-093141-2.json>) = 0')
    NOT_NEW = (
        # an existing plan replaced: opened to write, with or without truncation, or truncated
        f'1234  openat(AT_FDCWD</>, "{PLAN}.json", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 5<{PLAN}.json>',
        f'1234  openat(AT_FDCWD</>, "{PLAN}.json", O_WRONLY|O_CLOEXEC) = 5<{PLAN}.json>',
        f'1234  truncate("{PLAN}.json", 0) = 0',
        # renamed over, removed, its mode changed
        f'1234  rename("$W/sb/subtree/plan", "{PLAN}.json") = 0',
        f'1234  unlink("{PLAN}.json") = 0',
        f'1234  chmod("{PLAN}.json", 0777) = 0',
        # another name, or the name in a subfolder
        '1234  openat(AT_FDCWD</>, "$W/sb/home/exports/prunedisk-plan-x.json", O_WRONLY|O_CREAT|O_EXCL, 0666) = 5<$W/sb/home/exports/prunedisk-plan-x.json>',
        f'1234  openat(AT_FDCWD</>, "$W/sb/home/exports/sub/prunedisk-plan-20261121-093141.json", O_WRONLY|O_CREAT|O_EXCL, 0666) = 5<$W/sb/home/exports/sub/prunedisk-plan-20261121-093141.json>',
    )

    @classmethod
    def plan_verdict(cls, lines, kind: str = "--new") -> tuple[int, str, str]:
        with cls.world() as w:
            # A plan that was there (the suite plants such names), so a call by its path resolves.
            Path(cls.PLAN.replace("$W", w) + ".json").write_text("{}\n")
            code, out = cls.report(w, [ln.replace("$W", w) for ln in lines], kind,
                                   f"{w}/sb/home/exports/prunedisk-plan-{{STAMP}}{{SEQ}}.json")
            return code, out, w

    @classmethod
    def new_rules_hold(cls) -> bool:
        """The export passes, and every way of touching a plan that was there fails."""
        code, _, _ = cls.plan_verdict(list(cls.EXPORT))
        return code == 0 and all(cls.plan_verdict([ln])[0] == 1 for ln in cls.NOT_NEW)

    def test_a_new_file_declaration_allows_a_new_file_only(self):
        code, out, w = self.plan_verdict(list(self.EXPORT))
        self.assertEqual(code, 0, out)
        self.assertIn(f"declared openat: {w}/sb/home/exports/prunedisk-plan-20261121-093141-2.json", out.splitlines())
        for line in self.NOT_NEW:
            with self.subTest(line=line):
                code, out, w = self.plan_verdict([line])
                self.assertEqual(code, 1, out)
                self.assertRegex(out, r"(?m)^OTHER \w+: " + re.escape(w) + r"/sb/(home/exports|subtree)/")

    def test_a_file_declaration_of_the_same_name_would_excuse_replacing_a_plan(self):
        # Why the kind exists: "--file" allows writes to whatever matches, including a plan that
        # was there before.
        for line in self.NOT_NEW[:3]:
            with self.subTest(line=line):
                code, out, _ = self.plan_verdict([line], "--file")
                self.assertEqual(code, 0, out)

    def test_the_new_file_rules_are_load_bearing(self):
        orig = tr.Declarations.judge
        mutants = {
            # O_EXCL not required of the create
            "excl ignored": lambda self, op, p, via, tgid, excl=False: orig(self, op, p, via, tgid, True),
            # a write by path (an open without O_CREAT, truncate) taken as one through the descriptor
            "path writes allowed": lambda self, op, p, via, tgid, excl=False: orig(
                self, op, p, "fd" if via == "path" and op == "write" else via, tgid, excl),
            # "new" judged as "file"
            "new as file": lambda self, op, p, via, tgid, excl=False: orig(
                tr.Declarations([replace(d, kind="file") if d.kind == "new" else d for d in self.decls]) if any(
                    d.kind == "new" for d in self.decls) else self, op, p, via, tgid, excl),
        }
        self.assertTrue(self.new_rules_hold())
        for name, m in mutants.items():
            with self.subTest(mutant=name), mock.patch.object(tr.Declarations, "judge", m):
                self.assertFalse(self.new_rules_hold(), f"mutant survived: {name}")
        self.assertTrue(self.pid_template_caught())
        for p in ("$W/sb/data/recently-used.xbel.Q7-K2B", "$W/sb/data/recently-used.xbel.Q7ZK2B.bak",
                  "$W/sb/data/recently-used.xbelXQ7ZK2B", "$W/sb/data/other.xbel.Q7ZK2B", "$W/sb/data/x/recently-used.xbel.Q7ZK2B",
                  "$W/run/at-spi2-aB3dE9x/socket", "$W/conf/config.toml.1234.x.tmp"):
            with self.subTest(path=p):
                self.check([f'1234  openat(AT_FDCWD</>, "{p}", O_WRONLY|O_CREAT, 0644) = 9<{p}>'], 1, rf"OTHER openat: {re.escape(p)}")

    def test_the_output_capture_is_written_through_a_descriptor_only(self):
        self.check(['1234  write(1<$W/stdout.log>, "line\\n", 5) = 5'], 0)
        self.assertTrue(self.stdio_by_path_caught())

    def test_the_null_device_and_not_its_neighbours(self):
        self.check(['1234  openat(AT_FDCWD</>, "/dev/nullified", O_WRONLY|O_CREAT, 0644) = 3</dev/nullified>'], 1, r"OTHER openat: /dev/nullified")

    def test_folders_on_the_way_to_a_declared_path_may_be_created_only(self):
        with self.world() as w:
            os.rmdir(f"{w}/sb/home/exports")
            code, out = self.report(w, [f'1234  mkdir("{w}/sb/home/exports", 0777) = 0'])
            self.assertEqual(code, 0, out)
        self.check(['1234  rmdir("$W/sb/home/exports") = 0'], 1, r"OTHER rmdir: $W/sb/home/exports")
        self.check(['1234  mkdir("$W/sb/elsewhere", 0777) = 0'], 1, r"OTHER mkdir: $W/sb/elsewhere")

    def test_a_declaration_must_be_typed_exact_and_absolute(self):
        for bad in (["--declared", "/x"], ["--file", "relative"], ["--subtree", "/x/{XXXXXX}"], ["--subtree", "/"],
                    ["--declared-temp", "/x/a.XXXXX"], ["--profile", "nope", "--root", "DATA=/x"], ["--root", "DATA=/x"]):
            with self.subTest(args=bad):
                self.assertEqual(run_main(["/nonexistent.log", *bad])[0], 2)

    # -- strace's text ------------------------------------------------------------------------

    def test_an_unfinished_call_is_joined_to_its_resumption(self):
        self.check(['1234  openat(AT_FDCWD</>, "$W/u/t0-4", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666 <unfinished ...>',
                    '1235  openat(AT_FDCWD</>, "/etc/hosts", O_RDONLY <unfinished ...>',
                    '1234  <... openat resumed>)             = 7<$W/u/t0-4>',
                    '1235  <... openat resumed>)             = 8</etc/hosts>'], 1, r"OTHER openat: $W/u/t0-4", absent=[r".*/etc/hosts"])

    def test_a_call_the_process_never_returned_from_is_judged(self):
        self.check(['1234  unlinkat(5<$W/u>, "victim.txt", 0 <unfinished ...>'], 1, r"OTHER unlinkat: $W/u/victim.txt")

    def test_a_thread_shares_the_folder_and_a_process_copies_it(self):
        self.check(['1234  openat(AT_FDCWD<$W/sb/data/app>, "x", O_WRONLY|O_CREAT, 0644) = 3<$W/sb/data/app/x>',
                    '1234  clone3({flags=CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM, exit_signal=0, '
                    'stack=0x7f0b5affe000, stack_size=0x7fff00, tls=0x7f0b5b7fe640} => {parent_tid=[1235]}, 88) = 1235',
                    '1235  mkdir("y", 0777) = 0'], 0, r"declared mkdir: $W/sb/data/app/y")
        # A chdir in one thread moves the folder of every thread made with CLONE_FS; a process
        # made without it keeps its own.
        for flags, code, want in (("CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD", 1, r"OTHER unlink: $W/u/a"),
                                  ("CLONE_CHILD_CLEARTID|CLONE_CHILD_SETTID|SIGCHLD", 0, r"declared unlink: $W/sb/data/app/a")):
            with self.subTest(flags=flags):
                self.check(['1234  openat(AT_FDCWD<$W/sb/data/app>, "/etc/hosts", O_RDONLY) = 3</etc/hosts>',
                            f'1234  clone(child_stack=NULL, flags={flags}) = 1235',
                            '1234  chdir("$W/u") = 0',
                            '1235  unlink("a") = 0'], code, want)

    def test_chdir_and_fchdir_move_the_folder(self):
        self.check(['1234  chdir("$W/sb/data/app") = 0', '1234  unlink("a") = 0',
                    '1234  fchdir(4<$W/u/elsewhere>) = 0', '1234  unlink("b") = 0'], 1,
                   r"declared unlink: $W/sb/data/app/a", r"OTHER unlink: $W/u/elsewhere/b")

    def test_a_relative_path_the_trace_cannot_place_fails(self):
        self.check(['1234  openat(AT_FDCWD, "rel.txt", O_WRONLY|O_CREAT, 0644) = 3', '1234  chmod("rel.txt", 0600) = 0'], 1,
                   r"UNRESOLVED chmod: .*")

    def test_memfd_sockets_and_pipes_are_not_files(self):
        self.check(['1234  ftruncate(6</memfd:probe-shm (deleted)>, 4096) = 0',
                    '1234  ftruncate(14</memfd:WebKitSharedMemory>(deleted), 27049) = 0',
                    '1234  fchmod(5<socket:[12345]>, 0600) = 0', '1234  write(4<pipe:[9]>, "x", 1) = 1'], 0,
                   r"anonymous ftruncate: /memfd:WebKitSharedMemory \(deleted\)")

    def test_a_removed_file_is_judged_by_its_path(self):
        self.check(['1234  ftruncate(9<$W/u/gone.tmp>(deleted), 0) = 0', '1234  ftruncate(6<$W/u/x.tmp (deleted)>, 0) = 0'], 1,
                   r"OTHER ftruncate: $W/u/gone.tmp", r"OTHER ftruncate: $W/u/x.tmp", absent=["UNPARSED"])

    def test_escaped_paths_are_decoded(self):
        self.check(['1234  mkdir("$W/u/a\\"b\\303\\251", 0777) = 0'], 1, r'OTHER mkdir: $W/u/a"bé')


# ----------------------------------------------------------------------------------------------
# The snapshot check


class Snapshot(unittest.TestCase):
    def judge(self, before: dict, after: dict, *decl: str) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as d:
            b, a = f"{d}/b.json", f"{d}/a.json"
            Path(b).write_text(json.dumps({"entries": before}))
            Path(a).write_text(json.dumps({"entries": after}))
            return run_main(["--snapshot", b, a, *decl])

    D = ["--file", "/w/sb/declared.txt", "--dir", "/w/sb/gtk-3.0", "--subtree", "/w/sb/subtree"]

    @staticmethod
    def folder(*names, mtime=1):
        return {"kind": "dir", "mode": 493, "mtime_ns": mtime, "names": sorted(names)}

    @staticmethod
    def file(size=1, sha="a", mode=420):
        return {"kind": "file", "mode": mode, "mtime_ns": 1, "size": size, "sha256": sha}

    def test_a_declared_file_added_passes_and_its_folder_with_it(self):
        code, out = self.judge({"/w/sb": self.folder()}, {"/w/sb": self.folder("declared.txt", mtime=2), "/w/sb/declared.txt": self.file()}, *self.D)
        self.assertEqual(code, 0, out)
        self.assertIn("declared added: /w/sb/declared.txt", out)

    def test_a_file_in_a_folder_declared_empty_fails(self):
        code, out = self.judge({"/w/sb": self.folder()},
                               {"/w/sb": self.folder("gtk-3.0", mtime=2), "/w/sb/gtk-3.0": self.folder("bookmarks"),
                                "/w/sb/gtk-3.0/bookmarks": self.file()}, *self.D)
        self.assertEqual(code, 1, out)
        self.assertIn("UNDECLARED added: /w/sb/gtk-3.0/bookmarks", out)
        self.assertIn("declared added: /w/sb/gtk-3.0", out)

    def test_a_folder_that_lost_or_gained_an_undeclared_entry_fails(self):
        code, out = self.judge({"/w/sb": self.folder("x"), "/w/sb/x": self.file()}, {"/w/sb": self.folder(mtime=2)}, *self.D)
        self.assertEqual(code, 1, out)
        self.assertIn("UNDECLARED removed: /w/sb/x", out)
        self.assertRegex(out, r"(?m)^UNDECLARED changed .*: /w/sb$")

    def test_a_folders_time_alone_passes_only_where_something_is_declared(self):
        # An entry made and removed again inside it.
        code, out = self.judge({"/w/sb": self.folder()}, {"/w/sb": self.folder(mtime=2)}, *self.D)
        self.assertEqual(code, 0, out)
        code, out = self.judge({"/w/u": self.folder()}, {"/w/u": self.folder(mtime=2)}, *self.D)
        self.assertEqual(code, 1, out)

    def test_a_changed_file_or_mode_outside_fails(self):
        code, out = self.judge({"/w/u/f": self.file()}, {"/w/u/f": self.file(sha="b")}, *self.D)
        self.assertEqual(code, 1, out)
        code, out = self.judge({"/w/sb/gtk-3.0": self.folder()}, {"/w/sb/gtk-3.0": self.folder() | {"mode": 511}}, *self.D)
        self.assertEqual(code, 1, out)

    def test_anything_inside_a_subtree_passes(self):
        code, out = self.judge({"/w/sb/subtree": self.folder()},
                               {"/w/sb/subtree": self.folder("a", mtime=2), "/w/sb/subtree/a": self.folder("l"),
                                "/w/sb/subtree/a/l": {"kind": "link", "mode": 511, "mtime_ns": 1, "target": "/etc"}}, *self.D)
        self.assertEqual(code, 0, out)

    def test_an_unlistable_folder_fails(self):
        code, out = self.judge({"/w/sb": self.folder()}, {"/w/sb": {"kind": "dir", "mode": 493, "mtime_ns": 2, "names": "unlistable: x"}}, *self.D)
        self.assertEqual(code, 1, out)

    def test_a_new_file_declaration_allows_added_files_only(self):
        # The plan export (N2): added plans pass; a plan that was there and changed, was removed
        # or became a link fails, and so does any other name or a subfolder.
        new = ["--new", "/w/ex/prunedisk-plan-{STAMP}{SEQ}.json"]
        old = "prunedisk-plan-20261121-093141.json"
        before = {"/w/ex": self.folder(old), f"/w/ex/{old}": self.file()}
        added = {"prunedisk-plan-20261121-093141-2.json": self.file(sha="n")}
        after = {"/w/ex": self.folder(old, *added, mtime=2), f"/w/ex/{old}": self.file()} | {f"/w/ex/{k}": v for k, v in added.items()}
        code, out = self.judge(before, after, *new)
        self.assertEqual(code, 0, out)
        bad = {
            "changed": {**after, f"/w/ex/{old}": self.file(sha="b")},
            "removed": {"/w/ex": self.folder(mtime=2)},
            "made a link": {**after, f"/w/ex/{old}": {"kind": "link", "mode": 511, "mtime_ns": 2, "target": "/etc/passwd"}},
            "other name": {"/w/ex": self.folder(old, "other.json", mtime=2), f"/w/ex/{old}": self.file(), "/w/ex/other.json": self.file()},
            "subfolder": {"/w/ex": self.folder(old, "sub", mtime=2), f"/w/ex/{old}": self.file(), "/w/ex/sub": self.folder()},
            "added as a link": {"/w/ex": self.folder(old, "prunedisk-plan-20261121-093142.json", mtime=2), f"/w/ex/{old}": self.file(),
                                "/w/ex/prunedisk-plan-20261121-093142.json": {"kind": "link", "mode": 511, "mtime_ns": 2, "target": "/etc"}},
        }
        for name, a in bad.items():
            with self.subTest(case=name):
                code, out = self.judge(before, a, *new)
                self.assertEqual(code, 1, out)
                # the same with "--file" lets the changed plan through: why "new" exists
        code, out = self.judge(before, bad["changed"], "--file", new[1])
        self.assertEqual(code, 0, out)


# ----------------------------------------------------------------------------------------------
# The classifier


class Classifier(unittest.TestCase):
    CLASSES = {"NONE": tr.NONE, "ANON_FD": tr.ANON_FD, "PAIRS": set(tr.PAIRS), "OPENS": set(tr.OPENS),
               "FD_WRITE": set(tr.FD_WRITE), "PATH_CALLS": tr.PATH_CALLS, "TABLE": tr.TABLE, "UNSUPPORTED": set(tr.UNSUPPORTED)}

    def test_every_call_of_the_kernels_table_is_classified(self):
        self.assertGreaterEqual(len(tr.KERNEL_TABLE), 360)
        self.assertEqual(tr.KERNEL_TABLE[1], "write")
        self.assertEqual(tr.KERNEL_TABLE[452], "fchmodat2")
        missing = sorted(n for n in tr.KERNEL_TABLE.values() if n not in tr.KNOWN)
        self.assertEqual(missing, [])

    def test_every_call_of_this_hosts_headers_is_classified(self):
        hdr = next((p for p in ("/usr/include/x86_64-linux-gnu/asm/unistd_64.h", "/usr/include/asm/unistd_64.h") if os.path.exists(p)), None)
        if hdr is None:
            if os.environ.get("PRUNEDISK_REQUIRE_UNISTD") == "1":
                self.fail("no unistd_64.h on this host (PRUNEDISK_REQUIRE_UNISTD=1)")
            self.skipTest("no unistd_64.h on this host")
        names = dict(re.findall(r"^#define __NR_(\w+)\s+(\d+)", Path(hdr).read_text(), re.M))
        self.assertGreater(len(names), 300)
        self.assertEqual(sorted(n for n in names if n not in tr.KNOWN), [])
        for n, nr in names.items():
            if int(nr) in tr.KERNEL_TABLE:
                self.assertEqual(tr.KERNEL_TABLE[int(nr)], n, f"{hdr}: {n} is {nr}")

    def test_each_call_has_one_class(self):
        seen = {}
        for cls, names in self.CLASSES.items():
            for n in names:
                self.assertNotIn(n, seen, f"{n} is in {seen.get(n)} and {cls}")
                seen[n] = cls
        handled = {n[3:] for n in dir(tr.LogState) if n.startswith("on_")}
        self.assertLessEqual(handled, tr.TABLE | {"execveat"}, handled - tr.TABLE)
        self.assertLessEqual(tr.TABLE - {"close", "close_range", "chdir", "fchdir"} - handled, {"dup2", "dup3"} - handled)

    def test_strace_is_left_to_skip_only_calls_that_change_nothing(self):
        self.assertLessEqual(set(tr.HOT), tr.NONE)
        args = tr.strace_args()
        self.assertNotIn("--seccomp-bpf", args)
        for a in ("-f", "-q", "-y", "-ttt"):
            self.assertIn(a, args)
        self.assertNotIn("-qq", args)
        trace = args[args.index("-e") + 1]
        self.assertEqual(trace, "trace=!" + ",".join("?" + c for c in tr.HOT))


# ----------------------------------------------------------------------------------------------
# The declarations and the app's own list


def writes_ts() -> list[tuple[str, str, str]]:
    """(group title, path, what) of every entry of src/writes.ts."""
    text = (HERE.parent / "src" / "writes.ts").read_text()
    text = text[text.index("export const WRITES"):]
    out = []
    for g in re.finditer(r'title: "([^"]+)",\s*writes: \[(.*?)\n    \],', text, re.S):
        for e in re.finditer(r'path:\s*"((?:[^"\\]|\\.)*)",\s*what:\s*"((?:[^"\\]|\\.)*)"', g.group(2), re.S):
            out.append((g.group(1), e.group(1).replace("\\\\", "\\"), e.group(2)))
    return out


def spelled(name: str) -> str:
    """A declared name as src/writes.ts spells it."""
    return name.replace("{XXXXXX}", "XXXXXX")


class Declarations(unittest.TestCase):
    def setUp(self):
        self.entries = {p: (g, w) for g, p, w in writes_ts()}
        self.assertGreaterEqual(len(self.entries), 12, self.entries)
        self.linux = {p for g, p, _ in writes_ts() if not g.startswith(("macOS", "Windows"))}

    def all_decls(self):
        for name, decls in declarations.PROFILES.items():
            for d in decls:
                yield name, d

    def test_every_declaration_names_its_entry_in_writes_ts(self):
        for profile, (kind, root, rel, src) in self.all_decls():
            with self.subTest(profile=profile, decl=(kind, root, rel)):
                if src in declarations.HARNESS or src == declarations.E2E_ONLY:
                    continue
                self.assertIn(src, self.entries, f"declarations.py ties {rel} to {src!r}, which src/writes.ts does not list")

    def test_each_declaration_is_what_its_entry_says(self):
        # A subtree's entry says the whole folder is the library's; a folder declared with
        # nothing inside it is described as empty; a file or socket is named (below the entry's
        # folder, for a folder entry), a temporary as NAME.PID.N.tmp or with its XXXXXX.
        every = list(self.all_decls())
        for profile, (kind, root, rel, src) in every:
            if src not in self.entries:
                continue
            what = self.entries[src][1]
            text = src + " " + what
            with self.subTest(profile=profile, decl=(kind, root, rel)):
                if kind == "subtree":
                    self.assertIn("whole folder", what, f"{src}: a subtree must be described as the whole folder")
                elif kind == "dir":
                    if not any(r == root and x.startswith(rel + "/") for _, (_, r, x, _) in every):
                        self.assertIn("empty", text, f"{src}: {rel} is declared as an empty folder")
                elif kind in ("file", "socket") and rel:
                    name = spelled(rel)
                    if name.endswith(".{PID}.{N}.tmp"):
                        self.assertIn("NAME.PID.N.tmp", text, src)
                        name = name[: -len(".{PID}.{N}.tmp")]
                    if src.endswith("/"):
                        last = src.rstrip("/").rsplit("/", 1)[-1] + "/"
                        name = name[name.find(last) + len(last):] if last in name else name
                    else:
                        name = name.rsplit("/", 1)[-1]
                    self.assertIn(name, text, f"{src} does not name {name}")

    def test_every_linux_entry_is_declared_or_says_why_not(self):
        tied = {d[3] for _, d in self.all_decls()}
        for p in sorted(self.linux):
            with self.subTest(entry=p):
                self.assertTrue(p in tied or p in declarations.NOT_OBSERVED, f"src/writes.ts lists {p}: no check declares it")
        for p in declarations.NOT_OBSERVED:
            self.assertIn(p, self.linux)

    def test_the_shipped_check_allows_only_the_libraries_writes(self):
        idle = declarations.PROFILES["shipped-idle"]
        self.assertEqual(idle, declarations.LIBRARIES)
        for d in idle:
            self.assertNotEqual(d[3], declarations.E2E_ONLY)
        e2e_only = [d for _, d in self.all_decls() if d[3] == declarations.E2E_ONLY]
        self.assertEqual(sorted((d[0], d[1], d[2]) for d in e2e_only),
                         [("subtree", "CACHE", declarations.ID), ("subtree", "DATA", declarations.ID)])
        self.assertTrue(all(d in declarations.PROFILES["e2e-suite"] for d in e2e_only))

    def test_a_profile_expands_to_typed_options(self):
        roots = {k: f"/r/{k.lower()}" for k in ("DATA", "CACHE", "CONFIG", "RUNTIME", "HOME", "SESSION_HOME", "STDIO", "EXPORT")}
        for name in declarations.PROFILES:
            for kind, path in declarations.expand(name, roots):
                self.assertIn(kind, tr.KINDS)
                tr.Decl(kind, path)
        self.assertNotIn("stdio", [k for k, _ in declarations.expand("e2e-suite", dict(roots, STDIO="-"))])
        with self.assertRaises(KeyError):
            declarations.expand("e2e-suite", {})


# ----------------------------------------------------------------------------------------------
# The scripts use the observer as it is meant to be used


class Scripts(unittest.TestCase):
    def test_every_traced_launch_uses_the_observers_options_and_the_survivor_check(self):
        for script in ("check.sh", "idle-launch.sh", "observer-controls.sh"):
            text = (HERE / script).read_text()
            with self.subTest(script=script):
                self.assertRegex(text, r'trace_report\.py"? --strace-args')
                self.assertRegex(text, r"strace -DD -o ")
                self.assertRegex(text, r'traced\.py"? settle')
                self.assertIn("PRUNEDISK_TRACE_SESSION=", text)
                self.assertNotRegex(text, r"--seccomp-bpf")
                self.assertIsNone(re.search(r"trace_report\.py[^\n]*\|\|\s*true", text), f"{script} ignores the observer's exit")

    def test_the_checks_run_the_controls_and_both_judges(self):
        for script in ("check.sh", "appimage-dialog.sh"):
            self.assertIn("observer-controls.sh", (HERE / script).read_text(), script)
        for script in ("check.sh", "idle-launch.sh"):
            text = (HERE / script).read_text()
            self.assertIn("--profile", text, script)
            self.assertIn("--snapshot", text, script)
            self.assertIn("--links-from", text, script)
            self.assertNotIn("--declared ", text.split("Windows", 1)[0] if script == "check.sh" else text, script)


# ----------------------------------------------------------------------------------------------
# The end of a session (traced.py)


@unittest.skipUnless(sys.platform.startswith("linux") and os.path.isdir("/proc/self"), "Linux /proc only")
class Survivors(unittest.TestCase):
    def spawn(self, code: str, session: str) -> subprocess.Popen:
        env = dict(os.environ, PRUNEDISK_TRACE_SESSION=session)
        return subprocess.Popen([sys.executable, "-c", code], env=env, start_new_session=True)

    def settle(self, log, **kw) -> tuple[int, str]:
        out = io.StringIO()
        with redirect_stdout(out):
            code = traced.settle({log}, "t", kw.get("grace", 0.5), None, kw.get("wait_pid"), kw.get("wait_max", 10))
        return code, out.getvalue()

    def test_a_process_of_the_session_that_outlives_it_is_caught_and_killed(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.realpath(f"{d}/strace.log")
            p = self.spawn("import signal, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\ntime.sleep(30)", log)
            other = self.spawn("import time\ntime.sleep(30)", log + ".other")
            try:
                time.sleep(0.3)
                code, out = self.settle(log)
                self.assertEqual(code, 1, out)
                self.assertRegex(out, rf"(?m)^survivor t: pid={p.pid} .*session={re.escape(log)} ")
                self.assertEqual(p.wait(timeout=5), -9)
                self.assertIsNone(other.poll(), "a process of another session was touched")
            finally:
                for x in (p, other):
                    x.kill()
                    x.wait()

    def test_a_session_whose_processes_end_passes(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.realpath(f"{d}/strace.log")
            p = self.spawn("import time\ntime.sleep(0.3)", log)
            code, out = self.settle(log, wait_pid=p.pid)
            p.wait()
            self.assertEqual(code, 0, out)
            self.assertIn("closed t: every process of the session ended within", out)

    def test_a_program_that_never_ends_is_reported(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.realpath(f"{d}/strace.log")
            p = self.spawn("import time\ntime.sleep(30)", log)
            try:
                code, out = self.settle(log, wait_pid=p.pid, wait_max=0.5)
                self.assertEqual(code, 1, out)
                self.assertIn(f"pid={p.pid} the program never ended", out)
            finally:
                p.kill()
                p.wait()


if __name__ == "__main__":
    unittest.main()
