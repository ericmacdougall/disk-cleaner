#!/usr/bin/env python3
"""What the write observer allows, by check (PR review F15): typed, exact declarations, each tied
to the entry of src/writes.ts that discloses it.

  trace_report.py ... --profile NAME --root DATA=... --root CACHE=... ...
  declarations.py NAME VAR=PATH...      print the profile's declarations as trace_report options

A declaration is (kind, root, path under the root, the src/writes.ts path that discloses it).
Kinds are those of trace_report.py: "file" (exactly that file), "new" (a file made new by an
O_CREAT|O_EXCL open and written through it, never one that was there), "dir" (that folder may be
created, nothing in it), "socket", "subtree" (anything inside: only where src/writes.ts says the
whole folder is a library's or the app's data), "stdio" and "device". Paths may hold {XXXXXX}
(six letters or digits), {PID} (the writing process), {N} (digits), {STAMP} (YYYYMMDD-HHMMSS)
and {SEQ} (nothing, or a dash and digits). test_trace_report.py checks each tie: the entry
exists, it names the file, says "empty" for a folder and "whole folder" for a subtree, and every
Linux entry of src/writes.ts is declared somewhere or listed in NOT_OBSERVED with the reason.

Roots: DATA, CACHE, CONFIG and RUNTIME are the app's $XDG_DATA_HOME, $XDG_CACHE_HOME,
$XDG_CONFIG_HOME and $XDG_RUNTIME_DIR; HOME its home; SESSION_HOME the desktop session's own home,
where services the session bus starts (dconf) write; EXPORT the plan file the suite exports;
STDIO the harness's capture of the app's output.
"""

from __future__ import annotations

import sys

ID = "io.github.ericmacdougall.prunedisk"

# src/writes.ts paths, exactly as written there.
WEBVIEW = "~/.local/share/io.github.ericmacdougall.prunedisk/"
MESA = "~/.cache/mesa_shader_cache/"
MESA_DB = "~/.cache/mesa_shader_cache_db/"
FONTCONFIG = "~/.cache/fontconfig/"
DCONF_FLAG = "$XDG_RUNTIME_DIR/dconf/user"
AT_SPI = "$XDG_RUNTIME_DIR/at-spi2-XXXXXX/"
RECENT = "~/.local/share/recently-used.xbel"
GTK3 = "~/.config/gtk-3.0/"
GLIB_SETTINGS = "~/.config/glib-2.0/settings/"
DCONF_DB = "~/.config/dconf/user"
CONFIG_TOML = "disk-cleaner/config.toml (with config.toml.bak and config.toml.lock)"
APP_JSON = "disk-cleaner/prunedisk-app.json"
EXPORT = "the plan file you export"

# Not the app's writes: what the harness itself gives the app.
HARNESS = {
    "stdio": "the harness's capture of the app's standard output and error (launch.log): GTK's messages",
    "/dev/null": "the null device: writing to it changes nothing",
}
# The end-to-end build only (never shipped): incognito is off for WebKitWebDriver, so its webview
# keeps a profile in the sandbox (docs/DESKTOP.md, "End-to-end checks").
E2E_ONLY = "the e2e build's webview profile (not shipped; docs/DESKTOP.md, End-to-end checks)"

# Linux entries of src/writes.ts no check observes, and why.
NOT_OBSERVED = {
    "~/.dbus/session-bus/ and /tmp/dbus-*": "only without a session bus; every check runs under one "
    "(idle-launch.sh refuses to run without)",
}

LIBRARIES = [
    # webkit2gtk's private (incognito) session still keeps random salts, an empty blob folder
    # and its HSTS store (with its journal while it writes).
    ("file", "DATA", f"{ID}/storage/salt", WEBVIEW),
    ("file", "DATA", f"{ID}/mediakeys/v1/salt", WEBVIEW),
    ("file", "DATA", f"{ID}/CacheStorage/salt", WEBVIEW),
    ("file", "DATA", f"{ID}/WebKitCache/Version 17/salt", WEBVIEW),
    ("dir", "DATA", f"{ID}/WebKitCache/Version 17/Blobs", WEBVIEW),
    ("file", "DATA", f"{ID}/hsts-storage.sqlite", WEBVIEW),
    ("file", "DATA", f"{ID}/hsts-storage.sqlite-journal", WEBVIEW),
    # Libraries' own caches: the whole folder is the library's.
    ("subtree", "CACHE", "mesa_shader_cache", MESA),
    ("subtree", "CACHE", "mesa_shader_cache_db", MESA_DB),
    ("subtree", "CACHE", "fontconfig", FONTCONFIG),
    # GLib's dconf client: the change-flag file in the runtime folder (a memory file system).
    ("file", "RUNTIME", "dconf/user", DCONF_FLAG),
    # The accessibility bridge, when an assistive technology is on: its folder and socket.
    ("dir", "RUNTIME", "at-spi2-{XXXXXX}", AT_SPI),
    ("socket", "RUNTIME", "at-spi2-{XXXXXX}/socket", AT_SPI),
    ("stdio", "STDIO", "", "stdio"),
    ("device", "", "/dev/null", "/dev/null"),
]

DIALOG = [
    # GTK's file dialog: the recent-files list (through a temporary renamed over it), its
    # bookmarks folder, GLib's settings folder, and dconf's database (written by dconf-service).
    ("file", "DATA", "recently-used.xbel", RECENT),
    ("file", "DATA", "recently-used.xbel.{XXXXXX}", RECENT),
    ("dir", "CONFIG", "gtk-3.0", GTK3),
    ("dir", "CONFIG", "glib-2.0/settings", GLIB_SETTINGS),
    ("file", "SESSION_HOME", ".config/dconf/user", DCONF_DB),
]

APP = [
    # core's settings: the folder, the files, the temporaries a save renames over them.
    ("dir", "CONFIG", "disk-cleaner", CONFIG_TOML),
    ("file", "CONFIG", "disk-cleaner/config.toml", CONFIG_TOML),
    ("file", "CONFIG", "disk-cleaner/config.toml.bak", CONFIG_TOML),
    ("file", "CONFIG", "disk-cleaner/config.toml.lock", CONFIG_TOML),
    ("file", "CONFIG", "disk-cleaner/config.toml.{PID}.{N}.tmp", CONFIG_TOML),
    ("file", "CONFIG", "disk-cleaner/config.toml.bak.{PID}.{N}.tmp", CONFIG_TOML),
    ("file", "CONFIG", "disk-cleaner/prunedisk-app.json", APP_JSON),
    ("file", "CONFIG", "disk-cleaner/prunedisk-app.json.{PID}.{N}.tmp", APP_JSON),
    ("file", "EXPORT", "", EXPORT),
]

PROFILES = {
    # The shipped build, launched and left idle (idle-launch.sh).
    "shipped-idle": LIBRARIES,
    # The shipped AppImage on a bare host, its folder dialog used (appimage-dialog.sh).
    "dialog": LIBRARIES + DIALOG,
    # The end-to-end build under the WebDriver suite (check.sh).
    "e2e-suite": LIBRARIES + APP + [
        ("subtree", "DATA", ID, E2E_ONLY),
        ("subtree", "CACHE", ID, E2E_ONLY),
    ],
}


def path_of(root: str, rel: str, roots: dict[str, str]) -> str:
    if root == "":
        return rel
    if root not in roots:
        raise KeyError(f"the profile needs --root {root}=PATH")
    base = roots[root].rstrip("/")
    return f"{base}/{rel}" if rel else base


def expand(profile: str, roots: dict[str, str]) -> list[tuple[str, str]]:
    """(kind, path) of every declaration of the profile; a root given as "-" has none (the suite's
    app writes its output to the driver, not to a file)."""
    if profile not in PROFILES:
        raise KeyError(f"unknown profile {profile} (one of {', '.join(PROFILES)})")
    return [(kind, path_of(root, rel, roots)) for kind, root, rel, _ in PROFILES[profile] if roots.get(root) != "-"]


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    roots = dict(a.split("=", 1) for a in argv[1:])
    for kind, path in expand(argv[0], roots):
        print(f"--{kind} {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
