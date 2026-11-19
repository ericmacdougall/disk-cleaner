#!/usr/bin/env python3
"""Read-only guard, second layer (spec AC-6). The first layer is clippy's disallowed-methods
list in each package's clippy.toml; this script checks that the list and its exceptions stay as
reviewed, and scans for what clippy cannot see.

Usage: check_readonly.py [ROOT]      exit 0 when clean, 1 with findings, 2 on usage errors.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

CLIPPY_FILES = [
    "crates/view/clippy.toml",
    "crates/tui/clippy.toml",
    "apps/desktop/src-tauri/clippy.toml",
]

REQUIRED_PATHS = [
    "std::fs::write",
    "std::fs::remove_file",
    "std::fs::remove_dir",
    "std::fs::remove_dir_all",
    "std::fs::rename",
    "std::fs::copy",
    "std::fs::hard_link",
    "std::fs::soft_link",
    "std::fs::set_permissions",
    "std::fs::create_dir",
    "std::fs::create_dir_all",
    "std::fs::DirBuilder::create",
    "std::fs::File::create",
    "std::fs::File::create_new",
    "std::fs::File::set_len",
    "std::fs::File::set_times",
    "std::fs::File::set_modified",
    "std::fs::File::set_permissions",
    "std::fs::OpenOptions::open",
    "std::os::unix::fs::symlink",
    "std::os::windows::fs::symlink_file",
    "std::os::windows::fs::symlink_dir",
    "std::process::Command::new",
    "disk_cleaner_core::write_json",
    "tokio::fs::write",
    "tokio::fs::remove_file",
    "tokio::fs::remove_dir",
    "tokio::fs::remove_dir_all",
    "tokio::fs::rename",
    "tokio::fs::copy",
    "tokio::fs::hard_link",
    "tokio::fs::symlink",
    "tokio::fs::set_permissions",
    "tokio::fs::create_dir",
    "tokio::fs::create_dir_all",
    "tokio::fs::File::create",
    "tokio::fs::OpenOptions::open",
    "tokio::process::Command::new",
]

# Source trees scanned. crates/cli holds only the tui dispatch from this stream; core's own
# writer (write_json) is outside this guard by design.
RUST_DIRS = [
    "crates/view",
    "crates/tui",
    "crates/cli",
    "apps/desktop/src-tauri",
]

OS_PREFIX = r"(?:libc|nix|rustix|windows_sys|windows)\s*::"
OS_SYMBOLS = (
    r"(?:unlink|unlinkat|rmdir|rename|renameat|renameat2|renamex_np|truncate|ftruncate|"
    r"symlink|symlinkat|link|linkat|mkdir|mkdirat|chmod|fchmod|fchmodat|chown|fchown|"
    r"DeleteFileW|DeleteFileA|RemoveDirectoryW|RemoveDirectoryA|MoveFileW|MoveFileA|"
    r"MoveFileExW|MoveFileExA|ReplaceFileW|CopyFileW|CopyFileExW|SetEndOfFile|"
    r"SHFileOperationW|SetFileInformationByHandle|CreateHardLinkW|CreateSymbolicLinkW)"
)
OS_CALL = re.compile(OS_PREFIX + r"[\w:]*?\b" + OS_SYMBOLS + r"\b")
OS_USE = re.compile(r"\buse\s+" + OS_PREFIX + r".*\b" + OS_SYMBOLS + r"\b")

ALLOW_ATTR = re.compile(r"#\s*\[\s*(?:cfg_attr\s*\(.*,\s*)?allow\s*\((?P<lints>[^\]]*)\)")
INNER_ALLOW = re.compile(r"#\s*!\s*\[\s*(?:cfg_attr\s*\(.*,\s*)?(?:allow|expect)\s*\((?P<lints>[^\]]*)\)")
EXPECT_ATTR = re.compile(r"#\s*!?\s*\[\s*(?:cfg_attr\s*\(.*,\s*)?expect\s*\((?P<lints>[^\]]*)\)")
GUARD_LINTS = re.compile(r"clippy\s*::\s*(?:disallowed_methods|disallowed_method|style|all)\b")
FN_LINE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+|async\s+|unsafe\s+)*fn\s+(\w+)")
TEST_MOD = re.compile(r"^\s*mod\s+tests\b")

BANNED_JS = re.compile(r"@tauri-apps/plugin-(?:fs|shell|opener|process)\b")
BANNED_RUST_DEPS = re.compile(r"^\s*(?:tauri-plugin-(?:fs|shell|opener|process)|remove_dir_all|fs_err|fs-err|tempfile)\s*=", re.M)
SPAWN = re.compile(r"\bCommand\s*::\s*new\b")
SPAWN_FILE = "apps/desktop/src-tauri/src/worker.rs"

ALLOWED_CORE_PERMISSIONS = {"core:event:allow-listen", "core:event:allow-unlisten"}


def strip_comment(line: str) -> str:
    i = line.find("//")
    return line if i < 0 else line[:i]


def non_test_lines(text: str) -> list[tuple[int, str]]:
    """Lines outside `#[cfg(test)] mod tests { ... }` blocks (matched by brace depth, so code
    after a test module is still scanned)."""
    lines = text.splitlines()
    out = []
    i = 0
    while i < len(lines):
        if lines[i].strip() == "#[cfg(test)]":
            j = i + 1
            while j < len(lines) and lines[j].strip().startswith("#["):
                j += 1
            if j < len(lines) and TEST_MOD.match(lines[j]):
                depth = 0
                opened = False
                k = j
                while k < len(lines):
                    code = strip_comment(lines[k])
                    depth += code.count("{") - code.count("}")
                    opened = opened or "{" in code
                    k += 1
                    if opened and depth <= 0:
                        break
                i = k
                continue
        out.append((i + 1, lines[i]))
        i += 1
    return out


def load_allow(root: Path, findings: list[str]) -> set[tuple[str, str]]:
    path = root / "scripts/readonly-allow.txt"
    allowed: set[tuple[str, str]] = set()
    if not path.is_file():
        findings.append("scripts/readonly-allow.txt is missing")
        return allowed
    for n, line in enumerate(path.read_text().splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) != 2:
            findings.append(f"scripts/readonly-allow.txt:{n}: expected '<file> <function>'")
            continue
        allowed.add((parts[0], parts[1]))
    return allowed


def check_clippy_lists(root: Path, findings: list[str]) -> None:
    for rel in CLIPPY_FILES:
        p = root / rel
        if not p.is_file():
            findings.append(f"{rel}: missing (the clippy read-only list)")
            continue
        text = p.read_text()
        have = set(re.findall(r'path\s*=\s*"([^"]+)"', text))
        for want in REQUIRED_PATHS:
            if want not in have:
                findings.append(f"{rel}: the entry for {want} was removed")


def check_lint_config(root: Path, findings: list[str]) -> None:
    """Lint levels set outside the source: Cargo.toml [lints] and rustflags."""
    tomls = [root / "Cargo.toml"] + [root / d / "Cargo.toml" for d in RUST_DIRS]
    tomls += [root / ".cargo/config.toml", root / ".cargo/config", root / "apps/desktop/src-tauri/.cargo/config.toml"]
    for p in tomls:
        if not p.is_file():
            continue
        text = p.read_text()
        rel = p.relative_to(root).as_posix()
        if re.search(r"\b(?:disallowed_methods|disallowed-methods)\b", text):
            findings.append(f"{rel}: sets the disallowed_methods lint level")
        if re.search(r"-[AW]\s*clippy::(?:disallowed_methods|style|all)\b", text) or re.search(
            r"--cap-lints", text
        ):
            findings.append(f"{rel}: rustflags weaken clippy lints")
        if re.search(r"^\s*\[\s*(?:workspace\.)?lints\.clippy\s*\]", text, re.M) and re.search(
            r"^\s*(?:style|all)\s*=", text, re.M
        ):
            findings.append(f"{rel}: [lints.clippy] changes a lint group that holds disallowed_methods")


def check_rust_sources(root: Path, allowed: set[tuple[str, str]], findings: list[str]) -> None:
    for d in RUST_DIRS:
        base = root / d
        if not base.is_dir():
            continue
        for p in sorted(base.rglob("*.rs")):
            rel = p.relative_to(root).as_posix()
            if "/target/" in rel or "/gen/" in rel:
                continue
            in_tests_dir = "/tests/" in rel or "/benches/" in rel or "/examples/" in rel
            text = p.read_text(errors="replace")
            lines = text.splitlines()
            # Inner (crate or module level) allow/expect of the guard lints.
            for n, line in enumerate(lines, 1):
                code = strip_comment(line)
                m = INNER_ALLOW.search(code)
                if m and GUARD_LINTS.search(m.group("lints")) and not in_tests_dir:
                    findings.append(f"{rel}:{n}: crate- or module-level allow of the read-only lint")
                m = EXPECT_ATTR.search(code)
                if m and GUARD_LINTS.search(m.group("lints")):
                    findings.append(f"{rel}:{n}: expect(...) of the read-only lint is not allowed")
            if in_tests_dir:
                continue
            # Outer allow attributes: only on listed functions or on the test module.
            for n, line in enumerate(lines, 1):
                code = strip_comment(line)
                m = ALLOW_ATTR.search(code)
                if not m or code.lstrip().startswith("#!") or not GUARD_LINTS.search(m.group("lints")):
                    continue
                j = n  # index of the next line (0-based == n)
                target = None
                while j < len(lines):
                    nxt = strip_comment(lines[j]).strip()
                    if not nxt or nxt.startswith("#[") or nxt.startswith("///"):
                        j += 1
                        continue
                    target = nxt
                    break
                if target and TEST_MOD.match(target) and _is_cfg_test(lines, n - 1):
                    continue
                fm = FN_LINE.match(target or "")
                if not fm or (rel, fm.group(1)) not in allowed:
                    name = fm.group(1) if fm else (target or "?")
                    findings.append(
                        f"{rel}:{n}: allow of the read-only lint on '{name}', which is not in "
                        "scripts/readonly-allow.txt"
                    )
            body = non_test_lines(text)
            for n, line in body:
                code = strip_comment(line)
                if OS_CALL.search(code) or OS_USE.search(code):
                    findings.append(f"{rel}:{n}: direct OS file-mutation call")
                if SPAWN.search(code) and rel != SPAWN_FILE:
                    findings.append(f"{rel}:{n}: process spawn outside the analysis worker")
                if SPAWN.search(code) and rel == SPAWN_FILE and "current_exe" not in text:
                    findings.append(f"{rel}:{n}: the worker must re-run the app's own executable")


def _is_cfg_test(lines: list[str], attr_index: int) -> bool:
    """Is there a #[cfg(test)] among the attributes around this one?"""
    i = attr_index
    while i >= 0 and lines[i].strip().startswith("#["):
        if lines[i].strip() == "#[cfg(test)]":
            return True
        i -= 1
    i = attr_index + 1
    while i < len(lines) and lines[i].strip().startswith("#["):
        if lines[i].strip() == "#[cfg(test)]":
            return True
        i += 1
    return False


def check_dependencies(root: Path, findings: list[str]) -> None:
    for d in RUST_DIRS:
        p = root / d / "Cargo.toml"
        if not p.is_file():
            continue
        text = p.read_text()
        # Only [dependencies] sections count; dev-dependencies may hold test helpers.
        sections = re.split(r"^\s*\[", text, flags=re.M)
        for s in sections:
            header = s.split("]", 1)[0]
            if "dev-dependencies" in header or "build-dependencies" in header:
                continue
            if "dependencies" not in header:
                continue
            for m in BANNED_RUST_DEPS.finditer(s):
                findings.append(
                    f"{p.relative_to(root).as_posix()}: dependency {m.group(0).strip(' =')} is not allowed"
                )
    pkg = root / "apps/desktop/package.json"
    if pkg.is_file() and BANNED_JS.search(pkg.read_text()):
        findings.append("apps/desktop/package.json: a Tauri fs, shell, opener or process plugin")
    src = root / "apps/desktop/src"
    if src.is_dir():
        for p in sorted(src.rglob("*")):
            if p.suffix in {".ts", ".js", ".mjs", ".html"} and BANNED_JS.search(p.read_text(errors="replace")):
                findings.append(f"{p.relative_to(root).as_posix()}: imports a banned Tauri plugin")


def app_commands(root: Path) -> set[str]:
    build = root / "apps/desktop/src-tauri/build.rs"
    if not build.is_file():
        return set()
    m = re.search(r"COMMANDS\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\]", build.read_text(), re.S)
    return set(re.findall(r'"([a-z0-9_]+)"', m.group(1))) if m else set()


def check_capabilities(root: Path, findings: list[str]) -> None:
    cap_dir = root / "apps/desktop/src-tauri/capabilities"
    if not cap_dir.is_dir():
        return
    commands = {"allow-" + c.replace("_", "-") for c in app_commands(root)}
    for p in sorted(cap_dir.iterdir()):
        rel = p.relative_to(root).as_posix()
        if p.suffix not in {".json", ".toml"}:
            continue
        if p.suffix == ".toml":
            findings.append(f"{rel}: capabilities must be JSON so this check can read them")
            continue
        try:
            data = json.loads(p.read_text())
        except json.JSONDecodeError as e:
            findings.append(f"{rel}: not JSON ({e})")
            continue
        if data.get("remote"):
            findings.append(f"{rel}: grants remote URLs access")
        for perm in data.get("permissions", []):
            ident = perm if isinstance(perm, str) else perm.get("identifier", "?")
            if ident in ALLOWED_CORE_PERMISSIONS or ident in commands:
                continue
            findings.append(f"{rel}: permission '{ident}' is not one of the app's own commands")


def main(argv: list[str]) -> int:
    if len(argv) > 2:
        print(__doc__, file=sys.stderr)
        return 2
    root = Path(argv[1] if len(argv) == 2 else ".").resolve()
    findings: list[str] = []
    allowed = load_allow(root, findings)
    check_clippy_lists(root, findings)
    check_lint_config(root, findings)
    check_rust_sources(root, allowed, findings)
    check_dependencies(root, findings)
    check_capabilities(root, findings)
    for f in findings:
        print(f"read-only check: {f}")
    if findings:
        print(f"read-only check: FAILED with {len(findings)} finding(s)")
        return 1
    print("read-only check: passed")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
