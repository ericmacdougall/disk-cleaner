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
            f'1234  openat(AT_FDCWD</>, "{SB}/data/app/x.db", O_RDWR|O_CREAT|O_CLOEXEC, 0644) = 7</{SB}/data/app/x.db>',
            f'1234  openat(AT_FDCWD</>, "/etc/hosts", O_RDONLY|O_CLOEXEC) = 8</etc/hosts>',
            '1234  openat(AT_FDCWD</>, "/dev/null", O_WRONLY) = 9</dev/null>',
            f'1234  openat(AT_FDCWD</>, "/home/u/x", O_WRONLY|O_CREAT, 0644) = -1 EACCES (Permission denied)',
        ])
        self.assertEqual(code, 0, out)
        self.assertNotIn("OTHER", out)

    def test_an_undeclared_absolute_write_fails(self):
        code, out = report(['1234  openat(AT_FDCWD</>, "/tmp/prunedisk-planted-outside", O_WRONLY|O_CREAT|O_TRUNC, 0644) = 7</tmp/prunedisk-planted-outside>'])
        self.assertEqual(code, 1, out)
        self.assertIn("OTHER", out)
        self.assertIn("/tmp/prunedisk-planted-outside", out)

    def test_a_path_relative_to_a_directory_descriptor_is_resolved(self):
        # The webview's unlinkat(dirfd, "origin") seen at 682a0ff: inside the declared folder, it passes.
        code, out = report([f'1234  unlinkat(12</{SB}/data/app/storage/ab/cd>, "origin", 0) = 0'])
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
        self.assertIn("UNRESOLVED", out)

    def test_an_ignore_is_exact_or_a_folder_not_a_prefix_of_a_name(self):
        # --ignore /dev/null must not let /dev/nullified (or anything else under /dev) pass.
        code, out = report(['1234  openat(AT_FDCWD</>, "/dev/nullified", O_WRONLY|O_CREAT, 0644) = 3</dev/nullified>'])
        self.assertEqual(code, 1, out)

    def test_an_empty_or_unparsable_log_fails(self):
        code, out = report([])
        self.assertEqual(code, 1, out)
        code, out = report(["this is not strace output"])
        self.assertEqual(code, 1, out)


if __name__ == "__main__":
    unittest.main()
