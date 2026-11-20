#!/usr/bin/env bash
# End-to-end checks on the packaged app (PR gate F1), Linux and Windows (Git Bash).
# Run from apps/desktop after building:
#   RELEASE_BIN  the release binary (production config, no e2e feature)
#   E2E_BIN      the same app built with --features e2e; on Linux also with
#                --config e2e/automation.conf.json (incognito off: WebKitWebDriver cannot drive
#                an incognito webkit2gtk webview; see docs/DESKTOP.md)
#   CLI_BIN      the disk-cleaner CLI (writes the valid scan file the app opens)
#   OUT          a new folder for results, logs and the sandbox
# Steps: string check of both binaries, fixture, probe of the release binary, snapshot,
# the WebDriver suite on the e2e binary (with its home, config, data, cache and temp folders
# moved into the sandbox; on Linux under strace), snapshot again, compare.
set -euo pipefail

: "${RELEASE_BIN:?}" "${E2E_BIN:?}" "${CLI_BIN:?}" "${OUT:?}"
ID=io.github.ericmacdougall.prunedisk
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
SB="$OUT/sandbox"
FIX="$SB/fixture"
INPUTS="$OUT/inputs"
rm -rf "$SB" "$INPUTS"
mkdir -p "$SB/home/exports" "$SB/config" "$SB/data" "$SB/cache" "$SB/state" "$SB/tmp" "$SB/appdata" "$INPUTS"

win=false
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) win=true ;; esac
PY=python3
if $win; then PY=python; fi
native() { if $win; then cygpath -w "$1"; else printf '%s' "$1"; fi; }

echo "== binaries: the release binary holds no e2e hook names; the e2e binary does"
# RELEASE_BIN=none: a broken-variant run (e2e/variants), which builds only the e2e binary.
if [ "$RELEASE_BIN" != none ] && grep -a -q PRUNEDISK_E2E_ "$RELEASE_BIN"; then
  echo "::error::the release binary contains PRUNEDISK_E2E_ strings"; exit 1
fi
grep -a -q PRUNEDISK_E2E_FOLDERS "$E2E_BIN" || { echo "::error::the e2e binary lacks its hooks (control)"; exit 1; }

echo "== fixture (seed 16452)"
"$PY" e2e/fixture.py "$FIX"
"$CLI_BIN" scan --no-git --out "$(native "$INPUTS/valid-scan.json")" "$(native "$FIX")" > /dev/null
printf '{"version": 1, "roots": ["unterminated' > "$INPUTS/malformed-scan.json"

if [ "$RELEASE_BIN" != none ]; then
  echo "== probe: the release binary as shipped"
  node e2e/probe.mjs --app "$(native "$RELEASE_BIN")" --out "$OUT/probe-release.json" || echo "probe of the release binary failed (see $OUT/probe-release.json)"
fi

if $win; then
  folder() { cygpath -u "$(powershell -NoProfile -Command "[Environment]::GetFolderPath('$1')" | tr -d '\r')"; }
  REAL_LOCAL=$(folder LocalApplicationData)
  REAL_ROAMING=$(folder ApplicationData)
  # The real roaming folder is watched too: the app must use the APPDATA it is given.
  ROOTS=("$FIX" "$SB" "$REAL_LOCAL/$ID" "$REAL_ROAMING/disk-cleaner")
  DECLARED=(--declared "$SB/appdata/disk-cleaner" --declared "$SB/home/exports/plan.json" --declared "$REAL_LOCAL/$ID/EBWebView")
  BIG='C:\Windows'
  APP="$E2E_BIN"
else
  ROOTS=("$FIX" "$SB")
  # On Linux the e2e binary runs with incognito off, so the webview keeps its data in the
  # sandbox's data and cache folders.
  # The graphics driver (Mesa) keeps its shader cache in XDG_CACHE_HOME for any program that
  # draws with it, the shipped app included (docs/DESKTOP.md).
  DECLARED=(--declared "$SB/config/disk-cleaner" --declared "$SB/home/exports/plan.json"
    --declared "$SB/data/$ID" --declared "$SB/cache/$ID" --declared "$SB/cache/mesa_shader_cache")
  BIG=/usr
  APP="$OUT/app-traced.sh"
  mkdir -p "$OUT/trace"
  cat > "$APP" << EOF
#!/bin/sh
export HOME="$SB/home" XDG_CONFIG_HOME="$SB/config" XDG_DATA_HOME="$SB/data" XDG_CACHE_HOME="$SB/cache" XDG_STATE_HOME="$SB/state" TMPDIR="$SB/tmp"
exec strace -f -qq --seccomp-bpf -o "$OUT/trace/strace.log" -e status=successful \
  -e trace=open,openat,openat2,creat,mkdir,mkdirat,rename,renameat,renameat2,link,linkat,symlink,symlinkat,unlink,unlinkat,rmdir,truncate,chmod,fchmodat,chown,lchown,fchownat,utimensat,utimes,utime \
  "$E2E_BIN" "\$@"
EOF
  chmod +x "$APP"
fi

echo "== snapshot before"
"$PY" e2e/manifest.py snapshot "$OUT/before.json" "${ROOTS[@]}"

echo "== WebDriver suite"
status=0
if $win; then
  APPDATA=$(native "$SB/appdata") node e2e/run.mjs --app "$(native "$APP")" --fixture "$(native "$FIX")" --big "$BIG" \
    --valid "$(native "$INPUTS/valid-scan.json")" --malformed "$(native "$INPUTS/malformed-scan.json")" \
    --export "$(native "$SB/home/exports/plan.json")" --out "$OUT/e2e.json" || status=$?
else
  node e2e/run.mjs --app "$APP" --fixture "$FIX" --big "$BIG" --valid "$INPUTS/valid-scan.json" \
    --malformed "$INPUTS/malformed-scan.json" --export "$SB/home/exports/plan.json" --out "$OUT/e2e.json" || status=$?
fi
sleep 2

echo "== snapshot after and compare"
"$PY" e2e/manifest.py snapshot "$OUT/after.json" "${ROOTS[@]}"
nowrite=0
"$PY" e2e/manifest.py compare "$OUT/before.json" "$OUT/after.json" "${DECLARED[@]}" | tee "$OUT/manifest.txt" || nowrite=$?

if ! $win; then
  echo "== trace (report only)"
  "$PY" e2e/trace_report.py "$OUT/trace/strace.log" "${DECLARED[@]}" \
    --ignore /dev/ --ignore /proc/ --ignore /sys/ --ignore /run/user/ > "$OUT/trace.txt" || true
  grep -E '^(OTHER|trace:)' "$OUT/trace.txt" || true
fi

echo "suite exit $status, no-write exit $nowrite"
[ "$status" = 0 ] && [ "$nowrite" = 0 ]
