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
# The untraced switch (AC-9) by its exact name: the shipped check must scan for it itself.
want=('holds "PRUNEDISK_E2E_"' 'holds "PRUNEDISK_E2E_UNTRACED"' 'holds "PruneDisk (end-to-end build)"')
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
  # One list for both observers (the manifest watches the roots; the trace sees every path):
  # - core's config folder: config.toml, its .bak and .lock, and the temp files of a save;
  # - the export the suite asks for;
  # - the e2e build's webview profile: incognito is off for WebKitWebDriver, so the webview
  #   keeps its data in the sandbox's data and cache folders (docs/DESKTOP.md);
  # - Mesa's shader cache, written for any program that draws with Mesa (docs/DESKTOP.md);
  # - GLib's dconf flag file, opened read-write in the runtime folder (a memory file system).
  DECLARED=(--declared "$SB/config/disk-cleaner" --declared "$SB/home/exports/plan.json"
    --declared "$SB/data/$ID" --declared "$SB/cache/$ID" --declared "$SB/cache/mesa_shader_cache"
    --declared "$RUNTIME/dconf/user")
  TRACE_IGNORE=(--ignore /dev/null)
  BIG=/usr
  APP="$OUT/app-traced.sh"
  rm -rf "$OUT/trace"
  mkdir -p "$OUT/trace"
  CALLS=$("$PY" e2e/trace_report.py --strace-calls)
  # Every launch of the app (the suite starts several) writes its own log: strace.<pid>.log.
  # One exception, the AC-9 running-app scroll gate's timed launch (docs/DESKTOP.md): a launch
  # with PRUNEDISK_E2E_UNTRACED=1 and PRUNEDISK_E2E_SYNTH_NODES set runs without strace, leaves a
  # marker in $OUT/untraced/, and stays in the sandbox (the snapshot covers it). run.mjs runs the
  # same steps again in a traced launch, which must leave its trace log (checked below). This
  # script and its wrapper are CI only and never ship. The wrapper honours the switch only when
  # E2E_BIN holds its name, which only a --features e2e build does (src-tauri/src/e2e.rs).
  rm -rf "$OUT/untraced" "$OUT/traced-synth"
  mkdir -p "$OUT/untraced" "$OUT/traced-synth"
  UNTRACED_BRANCH=""
  if grep -a -q PRUNEDISK_E2E_UNTRACED "$E2E_BIN"; then
    UNTRACED_BRANCH="if [ \"\${PRUNEDISK_E2E_UNTRACED:-}\" = 1 ] && [ -n \"\${PRUNEDISK_E2E_SYNTH_NODES:-}\" ]; then
  : > \"$OUT/untraced/launch.\$\$\"
  exec \"$E2E_BIN\" \"\$@\"
fi"
  else
    echo "the e2e binary does not hold PRUNEDISK_E2E_UNTRACED: every launch is traced"
  fi
  cat > "$APP" << EOF
#!/bin/sh
export HOME="$SB/home" XDG_CONFIG_HOME="$SB/config" XDG_DATA_HOME="$SB/data" XDG_CACHE_HOME="$SB/cache" XDG_STATE_HOME="$SB/state" TMPDIR="$SB/tmp"
$UNTRACED_BRANCH
if [ -n "\${PRUNEDISK_E2E_SYNTH_NODES:-}" ]; then : > "$OUT/traced-synth/launch.\$\$"; fi
exec strace -f -qq --seccomp-bpf -y -o "$OUT/trace/strace.\$\$.log" -e "trace=$CALLS" "$E2E_BIN" "\$@"
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

trace=0
if ! $win; then
  # PR gate F10: the write observer is a gate. Any write it cannot place inside a declared
  # path, anywhere on the machine, fails the check.
  echo "== trace: every write of the app and its child processes"
  logs=("$OUT"/trace/strace.*.log)
  echo "trace logs: ${#logs[@]} (one per traced launch)"
  untraced=("$OUT"/untraced/launch.*)
  [ -e "${untraced[0]}" ] || untraced=()
  echo "untraced launches (the AC-9 timed launch and investigation sessions, outside this trace): ${#untraced[@]}"
  # Each one-million-row launch that went through strace must have left its log (the wrapper
  # execs strace in its own process, so the marker and the log share the pid).
  synth=("$OUT"/traced-synth/launch.*)
  [ -e "${synth[0]}" ] || synth=()
  echo "traced one-million-row launches: ${#synth[@]}"
  if [ "${#synth[@]}" = 0 ]; then
    echo "::error::no traced one-million-row launch ran: the AC-9 steps were not checked for writes"; trace=1
  fi
  for m in "${synth[@]}"; do
    p=${m##*.}
    [ -s "$OUT/trace/strace.$p.log" ] || { echo "::error::traced one-million-row launch $p left no trace log"; trace=1; }
  done
  "$PY" e2e/trace_report.py "${logs[@]}" "${DECLARED[@]}" "${TRACE_IGNORE[@]}" > "$OUT/trace.txt" || trace=$?
  { grep -E '^(OTHER|UNRESOLVED|UNOBSERVABLE|UNPARSED|trace:)' "$OUT/trace.txt" | head -200; } || true
fi

echo "suite exit $status, no-write exit $nowrite, trace exit $trace, idle-launch exit $idle"
[ "$status" = 0 ] && [ "$nowrite" = 0 ] && [ "$trace" = 0 ] && [ "$idle" = 0 ]
