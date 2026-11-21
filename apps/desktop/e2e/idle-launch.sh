#!/usr/bin/env bash
# What the shipped app writes when launched and left idle (PR gate F10), Linux.
# Usage, under a display and a session bus, as a desktop session has them:
#   xvfb-run -a dbus-run-session -- bash e2e/idle-launch.sh <program> <out folder>
# <program> is the shipped build: the release binary, or an extracted AppImage's AppRun.
# The program runs with its home, config, data, cache, state and temp folders in a sandbox
# under strace (-f -y, every call e2e/trace_report.py reads), idles for IDLE_SECONDS (20),
# and is stopped with SIGTERM. e2e/trace_report.py then fails on any write not listed below.
# Each listed path is what the shipped config writes, and why (docs/DESKTOP.md, "Webview
# data"); none of it is the user's data, and none is outside the sandbox but dconf's flag file.
# IDLE_DRIVER: a command run once the app has started (after IDLE_SECONDS) that drives the
#   window (xdotool, for example); the observer covers what the driven flow writes. It sees
#   SANDBOX (the sandbox folder) in its environment.
# IDLE_DECLARE: more paths the driven flow may write (one per line; the caller states why).
set -euo pipefail
PROG=${1:?program}
OUT=${2:?out folder}
SECS=${IDLE_SECONDS:-20}
ID=io.github.ericmacdougall.prunedisk
HERE=$(cd "$(dirname "$0")" && pwd)
# Without a session bus GLib starts one itself (dbus-launch), which writes ~/.dbus and /tmp
# (seen in the owner's run of the 682a0ff AppImage). Every desktop session has a bus.
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
  echo "::error::no session bus: run this under dbus-run-session (usage above)"; exit 2
fi
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
SB="$OUT/sandbox"
rm -rf "$SB" "$OUT/trace"
mkdir -p "$SB/home" "$SB/config" "$SB/data" "$SB/cache" "$SB/state" "$SB/tmp" "$OUT/trace"
RUNTIME=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
D="$SB/data/$ID"
DECLARED=(
  # webkit2gtk's ephemeral (incognito) session still keeps these salts: random bytes it hashes
  # storage, media-key and cache ids with. No browsing data.
  --declared "$D/storage/salt" --declared "$D/mediakeys/v1/salt" --declared "$D/WebKitCache/Version 17/salt"
  --declared "$D/CacheStorage/salt"
  # The ephemeral network cache's blob folder (empty).
  --declared "$D/WebKitCache/Version 17/Blobs"
  # webkit2gtk's HTTP Strict Transport Security store (and its SQLite journal); the app loads
  # only its own bundled page, so it holds no site.
  --declared "$D/hsts-storage.sqlite" --declared "$D/hsts-storage.sqlite-journal"
  # Mesa's shader cache and fontconfig's font cache: written by those libraries for any program
  # that draws text and graphics with them; the app does not ask for either.
  --declared "$SB/cache/mesa_shader_cache" --declared "$SB/cache/fontconfig"
  # GLib's dconf settings client opens its shared-memory flag file read-write (in the runtime
  # folder, a memory file system); the app writes no setting.
  --declared "$RUNTIME/dconf/user"
)
while IFS= read -r extra; do
  if [ -n "$extra" ]; then DECLARED+=(--declared "$extra"); fi
done <<< "${IDLE_DECLARE:-}"
IGNORE=(--ignore /dev/null)
CALLS=$(cd "$HERE" && python3 trace_report.py --strace-calls)

echo "== idle launch of $PROG for ${SECS} s (sandbox $SB)"
env HOME="$SB/home" XDG_CONFIG_HOME="$SB/config" XDG_DATA_HOME="$SB/data" XDG_CACHE_HOME="$SB/cache" \
  XDG_STATE_HOME="$SB/state" TMPDIR="$SB/tmp" \
  strace -f -qq --seccomp-bpf -y -o "$OUT/trace/strace.log" -e trace="$CALLS" "$PROG" > "$OUT/launch.log" 2>&1 &
tracer=$!
descendants() { # every process below $1
  local c
  for c in $(pgrep -P "$1" || true); do echo "$c"; descendants "$c"; done
}
sleep "$SECS"
app=$(pgrep -P "$tracer" | head -1 || true)
if [ -z "$app" ]; then
  echo "::error::the app exited during the idle launch"; cat "$OUT/launch.log"; wait "$tracer" || true; exit 1
fi
echo "running after ${SECS} s: $(descendants "$tracer" | xargs -r ps -o comm= -p | sort | uniq -c | tr -s ' \n' ' ')"
driven=0
if [ -n "${IDLE_DRIVER:-}" ]; then
  echo "== driver: $IDLE_DRIVER"
  SANDBOX="$SB" bash -c "$IDLE_DRIVER" || driven=$?
fi
kill -TERM "$app"
# The webview's own processes end with the app; strace ends when they have.
for _ in $(seq 1 15); do kill -0 "$tracer" 2> /dev/null || break; sleep 1; done
if kill -0 "$tracer" 2> /dev/null; then
  echo "still running 15 s after SIGTERM: $(descendants "$tracer" | xargs -r ps -o comm= -p | tr '\n' ' '); SIGTERM to each"
  descendants "$tracer" | xargs -r kill -TERM 2> /dev/null || true
  for _ in $(seq 1 15); do kill -0 "$tracer" 2> /dev/null || break; sleep 1; done
  descendants "$tracer" | xargs -r kill -KILL 2> /dev/null || true
fi
if kill -0 "$tracer" 2> /dev/null; then
  # A traced process that left the tree (a daemon) keeps strace waiting; the log is complete
  # up to now, so stop strace itself.
  echo "strace still waiting after its processes were stopped; stopping strace"
  kill -TERM "$tracer" 2> /dev/null || true
fi
wait "$tracer" || true

echo "== files in the sandbox after the launch"
(cd "$SB" && find . -mindepth 1 -printf '%y %s %P\n' | sort -k3) | tee "$OUT/sandbox.txt"
echo "== trace"
trace=0
python3 "$HERE/trace_report.py" "$OUT/trace/strace.log" "${DECLARED[@]}" "${IGNORE[@]}" > "$OUT/trace.txt" || trace=$?
{ grep -E '^(OTHER|UNRESOLVED|UNOBSERVABLE|UNPARSED|declared|trace:)' "$OUT/trace.txt" | grep -v mesa_shader_cache/ | head -200; } || true
[ "$driven" = 0 ] || { echo "::error::the driver failed (exit $driven)"; exit 1; }
[ "$trace" = 0 ] || { echo "::error::the launch wrote what nobody declared (above; $OUT/trace.txt)"; exit 1; }
echo "Every write of the launch is one of the declared paths."
