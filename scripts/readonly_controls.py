#!/usr/bin/env python3
"""Planted-violation controls for the read-only guard (spec AC-6, NEG-9).

Each control copies the checkout, plants one violation, and requires the guard to fail on it:
a guard that passes everything would fail these controls.

Usage: readonly_controls.py ROOT [--clippy]
  default   script-layer controls (scripts/check_readonly.py), no Rust toolchain needed
  --clippy  clippy-layer controls: one planted call per disallowed path in a copy of
            crates/view, then `cargo clippy` must report every one (needs cargo)
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SKIP = {".git", "target", "node_modules", "dist", "gen", "icons"}


def copy_tree(root: Path, dest: Path) -> Path:
    def ignore(_dir: str, names: list[str]) -> set[str]:
        return {n for n in names if n in SKIP}

    out = dest / "repo"
    shutil.copytree(root, out, ignore=ignore, symlinks=True)
    return out


def append(path: Path, text: str) -> None:
    with path.open("a") as f:
        f.write(text)


def replace(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    if old not in text:
        raise SystemExit(f"control setup: '{old}' not found in {path}")
    path.write_text(text.replace(old, new, 1))


def remove_line(path: Path, needle: str) -> None:
    lines = path.read_text().splitlines(keepends=True)
    kept = [l for l in lines if needle not in l]
    if len(kept) == len(lines):
        raise SystemExit(f"control setup: '{needle}' not found in {path}")
    path.write_text("".join(kept))


# (name, expected text in the finding, how to plant it)
SCRIPT_CONTROLS = [
    (
        "allow attribute on an unlisted function",
        "not in scripts/readonly-allow.txt",
        lambda r: append(
            r / "crates/view/src/table.rs",
            "\n#[allow(clippy::disallowed_methods)]\nfn planted() {}\n",
        ),
    ),
    (
        "crate-level allow",
        "crate- or module-level allow",
        lambda r: replace(
            r / "crates/view/src/lib.rs",
            "#![forbid(unsafe_code)]",
            "#![forbid(unsafe_code)]\n#![allow(clippy::disallowed_methods)]",
        ),
    ),
    (
        "crate-level allow of the style group",
        "crate- or module-level allow",
        lambda r: replace(
            r / "crates/tui/src/lib.rs",
            "#![forbid(unsafe_code)]",
            "#![forbid(unsafe_code)]\n#![allow(clippy::style)]",
        ),
    ),
    (
        "allow(warnings) on an unlisted function",
        "not in scripts/readonly-allow.txt",
        lambda r: append(
            r / "crates/view/src/table.rs",
            "\n#[allow(warnings)]\nfn planted() {}\n",
        ),
    ),
    (
        "multi-line allow attribute on an unlisted function",
        "not in scripts/readonly-allow.txt",
        lambda r: append(
            r / "crates/tui/src/app.rs",
            "\n#[allow(\n    dead_code,\n    clippy::disallowed_methods\n)]\nfn planted() {}\n",
        ),
    ),
    (
        "crate-level allow(warnings)",
        "crate- or module-level allow",
        lambda r: replace(
            r / "crates/view/src/lib.rs",
            "#![forbid(unsafe_code)]",
            "#![forbid(unsafe_code)]\n#![allow(warnings)]",
        ),
    ),
    (
        "unsafe block outside the worker spawn",
        "unsafe or pre_exec outside worker::spawn_worker",
        lambda r: append(
            r / "crates/tui/src/worker.rs",
            "\npub fn planted() -> i32 { unsafe { libc::getpid() } }\n",
        ),
    ),
    (
        "pre_exec in the desktop crate outside spawn_worker",
        "unsafe or pre_exec outside worker::spawn_worker",
        lambda r: append(
            r / "apps/desktop/src-tauri/src/lib.rs",
            "\npub fn planted(c: &mut std::process::Command) { c.pre_exec(|| Ok(())); }\n",
        ),
    ),
    (
        "expect attribute",
        "expect(...)",
        lambda r: append(
            r / "crates/view/src/table.rs",
            "\n#[expect(clippy::disallowed_methods)]\nfn planted() {}\n",
        ),
    ),
    (
        "entry removed from clippy.toml",
        "the entry for std::fs::remove_file was removed",
        lambda r: remove_line(r / "crates/tui/clippy.toml", '"std::fs::remove_file"'),
    ),
    (
        "lint level in Cargo.toml",
        "sets the disallowed_methods lint level",
        lambda r: append(
            r / "crates/view/Cargo.toml", '\n[lints.clippy]\ndisallowed_methods = "allow"\n'
        ),
    ),
    (
        "Command::new(\"rm\") outside the worker",
        "process spawn outside the analysis worker",
        lambda r: append(
            r / "crates/tui/src/app.rs",
            '\npub fn planted() { let _ = std::process::Command::new("rm"); }\n',
        ),
    ),
    (
        # Appended after walk.rs's test module: code after a test module is still scanned.
        "libc unlink",
        "direct OS file-mutation call",
        lambda r: append(
            r / "crates/view/src/walk.rs",
            "\npub fn planted(p: *const i8) -> i32 { libc::unlink(p) }\n",
        ),
    ),
    (
        "windows-sys DeleteFileW",
        "direct OS file-mutation call",
        lambda r: append(
            r / "crates/tui/src/ui.rs",
            "\nuse windows_sys::Win32::Storage::FileSystem::DeleteFileW;\n",
        ),
    ),
    (
        "@tauri-apps/plugin-fs dependency",
        "a Tauri fs, shell, opener or process plugin",
        lambda r: replace(
            r / "apps/desktop/package.json",
            '"dependencies": {',
            '"dependencies": {\n    "@tauri-apps/plugin-fs": "2.0.0",',
        ),
    ),
    (
        "tauri-plugin-shell dependency",
        "dependency tauri-plugin-shell is not allowed",
        lambda r: replace(
            r / "apps/desktop/src-tauri/Cargo.toml",
            "[dependencies]",
            '[dependencies]\ntauri-plugin-shell = "2"',
        ),
    ),
    (
        "capability granting fs:default",
        "permission 'fs:default'",
        lambda r: replace(
            r / "apps/desktop/src-tauri/capabilities/default.json",
            '"core:event:allow-listen",',
            '"core:event:allow-listen",\n    "fs:default",',
        ),
    ),
]


def run_check(repo: Path) -> tuple[int, str]:
    p = subprocess.run(
        [sys.executable, str(repo / "scripts/check_readonly.py"), str(repo)],
        capture_output=True,
        text=True,
    )
    return p.returncode, p.stdout + p.stderr


def script_controls(root: Path) -> int:
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        clean = copy_tree(root, Path(tmp) / "clean")
        code, out = run_check(clean)
        if code != 0:
            print(f"control baseline: the unmodified copy must pass, got {code}:\n{out}")
            return 1
        print("control baseline: clean copy passes")
    for name, expect, plant in SCRIPT_CONTROLS:
        with tempfile.TemporaryDirectory() as tmp:
            repo = copy_tree(root, Path(tmp))
            plant(repo)
            code, out = run_check(repo)
            ok = code == 1 and expect in out
            print(f"control {'caught' if ok else 'MISSED'}: {name}")
            if not ok:
                print(out)
                failures += 1
    return 1 if failures else 0


# One call per disallowed std and core path. `// PLANT` marks every line clippy must report.
PLANTED_RS = r"""
#![allow(dead_code, unused_must_use)]
use std::fs::write as aliased_write; // an aliased import must still be caught

pub fn planted(p: &std::path::Path, f: &std::fs::File) {
    std::fs::write(p, b"x"); // PLANT
    aliased_write(p, b"x"); // PLANT
    std::fs::remove_file(p); // PLANT
    std::fs::remove_dir(p); // PLANT
    std::fs::remove_dir_all(p); // PLANT
    std::fs::rename(p, p); // PLANT
    std::fs::copy(p, p); // PLANT
    std::fs::hard_link(p, p); // PLANT
    #[allow(deprecated)]
    std::fs::soft_link(p, p); // PLANT
    std::fs::set_permissions(p, f.metadata().unwrap().permissions()); // PLANT
    std::fs::create_dir(p); // PLANT
    std::fs::create_dir_all(p); // PLANT
    std::fs::DirBuilder::new().create(p); // PLANT
    std::fs::File::create(p); // PLANT
    std::fs::File::create_new(p); // PLANT
    f.set_len(0); // PLANT
    f.set_times(std::fs::FileTimes::new()); // PLANT
    f.set_modified(std::time::SystemTime::now()); // PLANT
    f.set_permissions(f.metadata().unwrap().permissions()); // PLANT
    std::fs::OpenOptions::new().write(true).open(p); // PLANT
    std::process::Command::new("rm"); // PLANT
    disk_cleaner_core::write_json(p, &1u8); // PLANT
    tokio::fs::remove_file(p); // PLANT
    #[cfg(unix)]
    std::os::unix::fs::symlink(p, p); // PLANT unix
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(p, p); // PLANT windows
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(p, p); // PLANT windows
}
"""


def clippy_controls(root: Path) -> int:
    on_windows = os.name == "nt"
    with tempfile.TemporaryDirectory() as tmp:
        repo = copy_tree(root, Path(tmp))
        planted = repo / "crates/view/src/planted.rs"
        planted.write_text(PLANTED_RS)
        replace(repo / "crates/view/src/lib.rs", "pub mod dev;", "pub mod dev;\npub mod planted;")
        # The tokio control needs tokio in this copy only.
        replace(
            repo / "crates/view/Cargo.toml",
            "[dependencies]",
            '[dependencies]\ntokio = { version = "1", features = ["fs"] }',
        )
        expected = []
        # File line numbers are 1-based; PLANTED_RS starts with an empty line, as written.
        for n, line in enumerate(PLANTED_RS.splitlines(), 1):
            if "// PLANT" not in line:
                continue
            if line.rstrip().endswith("unix") and on_windows:
                continue
            if line.rstrip().endswith("windows") and not on_windows:
                continue
            expected.append(n)
        env = dict(os.environ)
        env.setdefault("CARGO_TARGET_DIR", str(root / "target" / "readonly-controls"))
        p = subprocess.run(
            [
                "cargo",
                "clippy",
                "--color",
                "never",
                "-p",
                "disk-cleaner-view",
                "--message-format=short",
                "--",
                "-D",
                "warnings",
            ],
            cwd=repo,
            capture_output=True,
            text=True,
            env=env,
        )
        out = re.sub(r"\x1b\[[0-9;]*m", "", p.stdout + p.stderr)
        reported = set()
        for m in re.finditer(r"planted\.rs:(\d+):\d+: (?:error|warning): use of a disallowed method", out):
            reported.add(int(m.group(1)))
        missing = [n for n in expected if n not in reported]
        print(f"clippy controls: {len(expected)} planted, {len(expected) - len(missing)} reported")
        if p.returncode == 0 or missing:
            for n in missing:
                print(f"clippy control MISSED: line {n}: {PLANTED_RS.splitlines()[n - 1].strip()}")
            if p.returncode == 0:
                print("clippy control MISSED: clippy passed on planted code")
            print(out[-6000:])
            return 1
        return 0


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    root = Path(argv[1]).resolve()
    if "--clippy" in argv[2:]:
        return clippy_controls(root)
    return script_controls(root)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
