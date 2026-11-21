#!/usr/bin/env python3
"""Tests for trace_report.py, the Linux write observer of the end-to-end checks (PR gate F10).

The observer's default for a write nobody declared is failure: an absolute path outside the
declared paths, a path relative to a directory descriptor that resolves outside them, a call
that names its file only by descriptor (fchmod, ftruncate, fsetxattr ...), and a relative path
whose directory the trace cannot tell all make it exit 1. Declared and ignored paths, reads and
failed calls do not.

usage: python3 -m unittest e2e/test_trace_report.py      (from apps/desktop)
"""

from __future__ import annotations

import io
import os
import re
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import trace_report  # noqa: E402

SB = "/tmp/sb"


def report(lines: list[str], *extra: str) -> tuple[int, str]:
    with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as f:
        f.write("\n".join(lines) + "\n")
        name = f.name
    out = io.StringIO()
    try:
        with redirect_stdout(out):
            code = trace_report.main([name, "--declared", f"{SB}/data/app", "--declared", f"{SB}/home/exports/plan.json",
                                      "--ignore", "/dev/null", *extra])
    finally:
        os.unlink(name)
    return code, out.getvalue()


class Observer(unittest.TestCase):
    def test_declared_writes_and_reads_pass(self):
        code, out = report([
            f'1234  openat(AT_FDCWD</>, "{SB}/data/app/x.db", O_RDWR|O_CREAT|O_CLOEXEC, 0644) = 7<{SB}/data/app/x.db>',
            f'1234  openat(AT_FDCWD</>, "/etc/hosts", O_RDONLY|O_CLOEXEC) = 8</etc/hosts>',
            '1234  openat(AT_FDCWD</>, "/dev/null", O_WRONLY) = 9</dev/null>',
            f'1234  openat(AT_FDCWD</>, "/home/u/x", O_WRONLY|O_CREAT, 0644) = -1 EACCES (Permission denied)',
        ])
        self.assertEqual(code, 0, out)
        self.assertNotRegex(out, r"(?m)^OTHER ")

    def test_an_undeclared_absolute_write_fails(self):
        code, out = report(['1234  openat(AT_FDCWD</>, "/tmp/prunedisk-planted-outside", O_WRONLY|O_CREAT|O_TRUNC, 0644) = 7</tmp/prunedisk-planted-outside>'])
        self.assertEqual(code, 1, out)
        self.assertRegex(out, r"(?m)^OTHER ")
        self.assertIn("/tmp/prunedisk-planted-outside", out)

    def test_a_path_relative_to_a_directory_descriptor_is_resolved(self):
        # The webview's unlinkat(dirfd, "origin") seen at 682a0ff: inside the declared folder, it passes.
        code, out = report([f'1234  unlinkat(12<{SB}/data/app/storage/ab/cd>, "origin", 0) = 0'])
        self.assertEqual(code, 0, out)
        self.assertIn(f"{SB}/data/app/storage/ab/cd/origin", out)
        # The same call against a folder outside every declared path fails.
        code, out = report(['1234  unlinkat(12</home/u/project>, "origin", 0) = 0'])
        self.assertEqual(code, 1, out)
        self.assertIn("/home/u/project/origin", out)

    def test_both_paths_of_a_rename_are_checked(self):
        code, out = report([f'1234  renameat2(AT_FDCWD</>, "{SB}/data/app/t.tmp", 5</home/u>, "victim", RENAME_NOREPLACE) = 0'])
        self.assertEqual(code, 1, out)
        self.assertIn("/home/u/victim", out)

    def test_a_call_that_names_its_file_by_descriptor_only_is_seen(self):
        for line in ('1234  fchmod(7</home/u/secret.txt>, 0600) = 0',
                     '1234  ftruncate(7</home/u/secret.txt>, 0) = 0',
                     '1234  fsetxattr(7</home/u/secret.txt>, "user.x", "1", 1, 0) = 0',
                     '1234  fchown(7</home/u/secret.txt>, 1000, 1000) = 0',
                     '1234  utimensat(7</home/u/secret.txt>, NULL, NULL, 0) = 0'):
            code, out = report([line])
            self.assertEqual(code, 1, line + "\n" + out)
            self.assertIn("/home/u/secret.txt", out)

    def test_a_relative_path_the_trace_cannot_place_fails(self):
        code, out = report(['1234  openat(AT_FDCWD, "rel.txt", O_WRONLY|O_CREAT, 0644) = 3'])
        self.assertEqual(code, 1, out)
        self.assertRegex(out, r"(?m)^UNRESOLVED ")

    def test_an_ignore_is_exact_or_a_folder_not_a_prefix_of_a_name(self):
        # --ignore /dev/null must not let /dev/nullified (or anything else under /dev) pass.
        code, out = report(['1234  openat(AT_FDCWD</>, "/dev/nullified", O_WRONLY|O_CREAT, 0644) = 3</dev/nullified>'])
        self.assertEqual(code, 1, out)

    def test_an_empty_or_unparsable_log_fails(self):
        code, out = report([])
        self.assertEqual(code, 1, out)
        code, out = report(["this is not strace output"])
        self.assertEqual(code, 1, out)


class Format(unittest.TestCase):
    """Lines as Ubuntu 22.04's strace prints them (the CI runner's), from a probe run under
    -f -y on t2-dev: unfinished and resumed calls, clone3, chdir, memfd and calls without a
    directory descriptor."""

    def test_an_unfinished_call_is_joined_to_its_resumption(self):
        code, out = report([
            '410   openat(AT_FDCWD</work/c>, "/home/u/t0-4", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666 <unfinished ...>',
            '411   openat(AT_FDCWD</work/c>, "/etc/hosts", O_RDONLY <unfinished ...>',
            '410   <... openat resumed>)             = 7</home/u/t0-4>',
            '411   <... openat resumed>)             = 8</etc/hosts>',
        ])
        self.assertEqual(code, 1, out)
        self.assertRegex(out, r"(?m)^OTHER openat: /home/u/t0-4$")
        self.assertNotIn("/etc/hosts", out)

    def test_a_call_the_process_never_returned_from_is_judged(self):
        code, out = report(['410   unlinkat(5</home/u>, "victim", 0 <unfinished ...>'])
        self.assertEqual(code, 1, out)
        self.assertIn("/home/u/victim", out)

    def test_clone3_and_its_arrow_parse_and_the_child_keeps_the_folder(self):
        code, out = report([
            f'389   openat(AT_FDCWD<{SB}/data/app>, "x", O_WRONLY|O_CREAT, 0644) = 3<{SB}/data/app/x>',
            '389   clone3({flags=CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM|CLONE_SETTLS'
            '|CLONE_PARENT_SETTID|CLONE_CHILD_CLEARTID, child_tid=0x7f0b5b7fe910, parent_tid=0x7f0b5b7fe910, '
            'exit_signal=0, stack=0x7f0b5affe000, stack_size=0x7fff00, tls=0x7f0b5b7fe640} => {parent_tid=[390]}, 88) = 390',
            '390   mkdir("y", 0777) = 0',
        ])
        self.assertEqual(code, 0, out)
        self.assertIn(f"declared mkdir: {SB}/data/app/y", out)

    def test_a_call_without_a_directory_descriptor_uses_the_known_folder(self):
        # link, symlink, truncate and chmod name relative paths with no descriptor at all.
        code, out = report([
            '389   openat(AT_FDCWD</home/u>, "/etc/hosts", O_RDONLY|O_CLOEXEC) = 3</etc/hosts>',
            '389   chmod("moved.txt", 0644)          = 0',
        ])
        self.assertEqual(code, 1, out)
        self.assertIn("OTHER chmod: /home/u/moved.txt", out)

    def test_chdir_moves_the_folder_and_fchdir_too(self):
        code, out = report([
            '389   openat(AT_FDCWD</home/u>, "/etc/hosts", O_RDONLY) = 3</etc/hosts>',
            f'389   chdir("{SB}/data/app") = 0',
            '389   unlink("a") = 0',
            '389   fchdir(4</home/u/elsewhere>) = 0',
            '389   unlink("b") = 0',
        ])
        self.assertEqual(code, 1, out)
        self.assertIn(f"declared unlink: {SB}/data/app/a", out)
        self.assertIn("OTHER unlink: /home/u/elsewhere/b", out)

    def test_a_chdir_in_one_thread_unsettles_the_others(self):
        code, out = report([
            f'389   openat(AT_FDCWD<{SB}/data/app>, "/etc/hosts", O_RDONLY) = 3</etc/hosts>',
            f'390   openat(AT_FDCWD<{SB}/data/app>, "/etc/hosts", O_RDONLY) = 3</etc/hosts>',
            '389   chdir("/home/u") = 0',
            '390   unlink("a") = 0',
        ])
        self.assertEqual(code, 1, out)
        self.assertRegex(out, r"(?m)^UNRESOLVED ")

    def test_symlink_judges_the_new_name_only(self):
        code, out = report([f'389   symlinkat("/home/u/target", 3<{SB}/data/app>, "link") = 0'])
        self.assertEqual(code, 0, out)

    def test_memfd_and_sockets_are_not_files(self):
        code, out = report(['389   ftruncate(6</memfd:probe-shm (deleted)>, 4096) = 0',
                            '389   fchmod(5<socket:[12345]>, 0600) = 0'])
        self.assertEqual(code, 0, out)
        self.assertRegex(out, r"(?m)^anonymous ")

    def test_strace_6_marks_a_removed_file_after_the_brackets(self):
        # Ubuntu 24.04's strace 6.8, from the owner's idle launch of the 682a0ff AppImage.
        code, out = report(['ftruncate(14</memfd:WebKitSharedMemory>(deleted), 27049) = 0',
                            '1234  ftruncate(9</home/u/gone.tmp>(deleted), 0) = 0'])
        self.assertEqual(code, 1, out)
        self.assertRegex(out, r"(?m)^anonymous ftruncate: /memfd:WebKitSharedMemory$")
        self.assertRegex(out, r"(?m)^OTHER ftruncate: /home/u/gone.tmp$")
        self.assertNotRegex(out, r"(?m)^UNPARSED ")

    def test_a_deleted_file_is_judged_by_its_path(self):
        code, out = report(['389   ftruncate(6</home/u/x.tmp (deleted)>, 0) = 0'])
        self.assertEqual(code, 1, out)
        self.assertIn("OTHER ftruncate: /home/u/x.tmp", out)

    def test_folders_on_the_way_to_a_declared_path_may_be_created(self):
        code, out = report([f'389   mkdir("{SB}/home", 0777) = 0', f'389   mkdir("{SB}/home/exports", 0777) = 0'])
        self.assertEqual(code, 0, out)
        # Only created: removing or changing them is not on the way to anything.
        code, out = report([f'389   rmdir("{SB}/home/exports") = 0'])
        self.assertEqual(code, 1, out)
        code, out = report([f'389   mkdir("{SB}/elsewhere", 0777) = 0'])
        self.assertEqual(code, 1, out)

    def test_the_file_an_open_reached_is_judged_too(self):
        # A declared name that is a link to a user's file: the kernel's answer is what counts.
        code, out = report([f'389   openat(AT_FDCWD</>, "{SB}/data/app/link", O_WRONLY|O_TRUNC) = 3</home/u/notes.txt>'])
        self.assertEqual(code, 1, out)
        self.assertIn("OTHER openat: /home/u/notes.txt", out)

    def test_io_uring_cannot_be_observed_so_it_fails(self):
        code, out = report(['389   io_uring_setup(8, {flags=0, sq_thread_cpu=0, sq_thread_idle=0}) = 5<anon_inode:[io_uring]>'])
        self.assertEqual(code, 1, out)
        self.assertRegex(out, r"(?m)^UNOBSERVABLE ")

    def test_a_line_of_a_traced_call_that_cannot_be_read_fails(self):
        code, out = report(['389   openat(AT_FDCWD</>, "/home/u/x", O_WRONLY = 3'])
        self.assertEqual(code, 1, out)
        self.assertRegex(out, r"(?m)^UNPARSED ")

    def test_escaped_paths_are_decoded(self):
        code, out = report(['389   mkdir("/home/u/a\\"b\\303\\251", 0777) = 0'])
        self.assertEqual(code, 1, out)
        self.assertIn('OTHER mkdir: /home/u/a"bé', out)


class Contract(unittest.TestCase):
    def test_strace_is_asked_for_every_call_the_observer_reads(self):
        listed = {c.lstrip("?") for c in trace_report.STRACE_CALLS.split(",")}
        self.assertEqual(listed, trace_report.TRACED)
        for call in trace_report.CALLS:
            self.assertIn(call, listed)

    def test_the_scripts_take_the_call_list_from_the_observer_and_gate_on_it(self):
        here = Path(__file__).resolve().parent
        for script in ("check.sh", "idle-launch.sh"):
            text = (here / script).read_text()
            self.assertTrue("trace_report.py --strace-calls" in text, f"{script} does not take the call list from the observer")
            self.assertRegex(text, r"strace [^\n]*-y ", f"{script} does not decode descriptors")
            self.assertIsNone(re.search(r"trace_report\.py[^\n]*\|\|\s*true", text), f"{script} ignores the observer's exit")


if __name__ == "__main__":
    unittest.main()
