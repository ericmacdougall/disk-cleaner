#!/usr/bin/env bash
# End-to-end checks on the packaged app (PR gate F1), Linux and Windows (Git Bash).
# Run from apps/desktop after building:
#   RELEASE_BIN  the release binary (production config, no e2e feature)
#   E2E_BIN      the same app built with --features e2e and an automation config: on Linux
#                e2e/automation.conf.json (incognito off: WebKitWebDriver cannot drive an
#                incognito webkit2gtk webview), on Windows e2e/windows-automation.conf.json
#                (browser arguments that open a DevTools port for the Edge driver); see
#                docs/DESKTOP.md
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
# On Windows the e2e binary opens a DevTools port for the Edge driver
# (e2e/windows-automation.conf.json). The shipped binary must not.
if [ "$RELEASE_BIN" != none ] && grep -a -q remote-debugging-port "$RELEASE_BIN"; then
  echo "::error::the release binary contains remote-debugging-port"; exit 1
fi
# e2e/check-shipped.sh is the bundle job's proof that a shipped program is no e2e build. Its
# control: on the e2e binary it must fail and name the strings that build holds (the
# automation config's window title and the hooks everywhere, the DevTools switch on Windows).
# The release binary must pass it.
ext=""; if $win; then ext=.exe; fi
mkdir -p "$OUT/shipped-e2e" "$OUT/shipped-release"
cp "$E2E_BIN" "$OUT/shipped-e2e/prunedisk-desktop$ext"
if bash e2e/check-shipped.sh "$OUT/shipped-e2e" "$OUT/shipped-e2e-work" > "$OUT/shipped-control.txt" 2>&1; then
  cat "$OUT/shipped-control.txt"; echo "::error::check-shipped.sh passed the e2e binary (control)"; exit 1
fi
want=('holds "PRUNEDISK_E2E_"' 'holds "PruneDisk (end-to-end build)"')
if $win; then want+=('holds "remote-debugging-port"'); fi
for w in "${want[@]}"; do
  grep -q -F "$w" "$OUT/shipped-control.txt" || { cat "$OUT/shipped-control.txt"; echo "::error::check-shipped.sh did not report: $w (control)"; exit 1; }
done
echo "control: check-shipped.sh fails the e2e binary, naming ${#want[@]} strings"
if [ "$RELEASE_BIN" != none ]; then
  cp "$RELEASE_BIN" "$OUT/shipped-release/prunedisk-desktop$ext"
  bash e2e/check-shipped.sh "$OUT/shipped-release" "$OUT/shipped-release-work"
fi

echo "== fixture (seed 16452)"
"$PY" e2e/fixture.py "$FIX"
"$CLI_BIN" scan --no-git --out "$(native "$INPUTS/valid-scan.json")" "$(native "$FIX")" > /dev/null
printf '{"version": 1, "roots": ["unterminated' > "$INPUTS/malformed-scan.json"

idle=0
if [ "$RELEASE_BIN" != none ]; then
  echo "== probe: the release binary as shipped"
  # In a sandbox of its own, so not even the probe writes to the runner's real home folders.
  PSB="$OUT/probe-sandbox"
  rm -rf "$PSB"; mkdir -p "$PSB/home" "$PSB/config" "$PSB/data" "$PSB/cache" "$PSB/state" "$PSB/tmp" "$PSB/appdata"
  if $win; then
    APPDATA=$(native "$PSB/appdata") node e2e/probe.mjs --app "$(native "$RELEASE_BIN")" --out "$OUT/probe-release.json" || echo "probe of the release binary failed (see $OUT/probe-release.json)"
  else
    HOME="$PSB/home" XDG_CONFIG_HOME="$PSB/config" XDG_DATA_HOME="$PSB/data" XDG_CACHE_HOME="$PSB/cache" \
      XDG_STATE_HOME="$PSB/state" TMPDIR="$PSB/tmp" \
      node e2e/probe.mjs --app "$RELEASE_BIN" --out "$OUT/probe-release.json" || echo "probe of the release binary failed (see $OUT/probe-release.json)"
    # PR gate F10: what the shipped config itself writes, idle, under the write observer.
    echo "== idle launch of the release binary (shipped config) under the write observer"
    # The snapshots also watch the runner's own settings and data folders, where a service the
    # session bus starts for the app (dconf-service) would write.
    export IDLE_SNAPSHOT="$HOME/.config $HOME/.local/share"
    if [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
      bash e2e/idle-launch.sh "$RELEASE_BIN" "$OUT/idle-release" || idle=$?
    else
      dbus-run-session -- bash e2e/idle-launch.sh "$RELEASE_BIN" "$OUT/idle-release" || idle=$?
    fi
  fi
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
  RUNTIME=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
  # One list of typed declarations for both observers (the snapshots watch the roots; the trace
  # sees every path), each tied to its entry of src/writes.ts: e2e/declarations.py, profile
  # e2e-suite (core's settings files and the temporaries a save renames over them, the export
  # the suite asks for, the libraries' caches, GLib's dconf flag file, and the e2e build's own
  # webview profile: incognito is off for WebKitWebDriver, docs/DESKTOP.md).
  DECLARE=(--profile e2e-suite --root DATA="$SB/data" --root CACHE="$SB/cache" --root CONFIG="$SB/config"
    --root HOME="$SB/home" --root RUNTIME="$RUNTIME" --root EXPORT="$SB/home/exports/plan.json" --root STDIO=-)
  BIG=/usr
  APP="$OUT/app-traced.sh"
  rm -rf "$OUT/trace" "$OUT/controls"
  mkdir -p "$OUT/trace"
  # PR review F11, F12, F15: first the observer's own controls, under the same strace launch:
  # each planted change must be caught and the positive control must pass.
  echo "== write observer controls"
  controls=0
  bash e2e/observer-controls.sh "$OUT/controls" > "$OUT/controls.log" 2>&1 || controls=$?
  grep -E '^(==|::error|observer controls|coverage:)' "$OUT/controls.log" || true
  mapfile -t STRACE_ARGS < <("$PY" e2e/trace_report.py --strace-args)
  STRACE_Q=$(printf ' %q' "${STRACE_ARGS[@]}")
  E2E_DIR=$(pwd)/e2e
  # Every launch of the app (the suite starts several) writes its own log, strace.<pid>.log.
  # strace -DD: the driver's process is the app itself and strace a detached grandchild, so the
  # driver ending the app never ends strace first (which let traced processes run on untraced).
  # Each launch's processes carry PRUNEDISK_TRACE_SESSION, and a detached e2e/traced.py waits
  # for the app to end, then for every one of them: one alive 2 s later is a survivor (killed,
  # recorded in close.<pid>.txt, and the check fails).
  cat > "$APP" << EOF
#!/bin/bash
export HOME="$SB/home" XDG_CONFIG_HOME="$SB/config" XDG_DATA_HOME="$SB/data" XDG_CACHE_HOME="$SB/cache" XDG_STATE_HOME="$SB/state" TMPDIR="$SB/tmp"
( setsid python3 "$E2E_DIR/traced.py" settle "$OUT/trace/strace.\$\$.log" --wait-pid \$\$ --grace 2 --record "$OUT/trace/close.\$\$.txt" --label "launch \$\$" < /dev/null > /dev/null 2>&1 & )
export PRUNEDISK_TRACE_SESSION="$OUT/trace/strace.\$\$.log"
exec strace -DD -o "$OUT/trace/strace.\$\$.log"$STRACE_Q "$E2E_BIN" "\$@"
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

survive=0
controls=${controls:-0}
if ! $win; then
  # Every launch's survivor check must have finished (its app ended) and found nothing; then
  # nothing of any launch may be left at all.
  echo "== survivors: every process of every launch ended within 2 s of its app"
  logs=("$OUT"/trace/strace.*.log)
  [ -e "${logs[0]}" ] || logs=()
  for log in "${logs[@]}"; do
    p=${log##*/strace.}
    p=${p%.log}
    for _ in $(seq 1 150); do grep -q '^done ' "$OUT/trace/close.$p.txt" 2> /dev/null && break; sleep 0.1; done
    if ! grep -q '^done ' "$OUT/trace/close.$p.txt" 2> /dev/null; then
      echo "::error::launch $p: its app had not ended when the suite did"; survive=1
    fi
    if grep -q '^survivor' "$OUT/trace/close.$p.txt" 2> /dev/null; then survive=1; fi
    cat "$OUT/trace/close.$p.txt" 2> /dev/null || true
  done
  "$PY" e2e/traced.py sweep "$OUT/trace" --record "$OUT/trace/sweep.txt" || survive=1
fi

echo "== snapshot after and compare"
"$PY" e2e/manifest.py snapshot "$OUT/after.json" "${ROOTS[@]}"
nowrite=0
if $win; then
  "$PY" e2e/manifest.py compare "$OUT/before.json" "$OUT/after.json" "${DECLARED[@]}" | tee "$OUT/manifest.txt" || nowrite=$?
else
  # The same typed declarations as the trace (e2e/declarations.py), not folder prefixes.
  "$PY" e2e/trace_report.py --snapshot "$OUT/before.json" "$OUT/after.json" "${DECLARE[@]}" > "$OUT/manifest.txt" || nowrite=$?
  grep -v '^declared .*mesa_shader_cache' "$OUT/manifest.txt" | head -200 || true
fi

trace=0
if ! $win; then
  # PR gate F10: the write observer is a gate. Any change it cannot place on a declared path,
  # anywhere on the machine, any call it does not know and any process it lost fails the check.
  echo "== trace: every change by the app and its child processes"
  echo "trace logs: ${#logs[@]} (one per launch)"
  if [ "${#logs[@]}" = 0 ]; then
    echo "::error::no trace log: the suite launched no traced app"; trace=1
  else
    "$PY" e2e/trace_report.py "${logs[@]}" "${DECLARE[@]}" --links-from "$OUT/before.json" > "$OUT/trace.txt" || trace=$?
    { grep -E '^(LATE|LOST|UNKNOWN|UNSUPPORTED|UNPARSED|UNRESOLVED|OTHER|unreadable|trace:|  at )' "$OUT/trace.txt" | head -200; } || true
  fi
fi

echo "suite exit $status, no-write exit $nowrite, trace exit $trace, survivors exit $survive, observer controls exit $controls, idle-launch exit $idle"
[ "$status" = 0 ] && [ "$nowrite" = 0 ] && [ "$trace" = 0 ] && [ "$survive" = 0 ] && [ "$controls" = 0 ] && [ "$idle" = 0 ]
