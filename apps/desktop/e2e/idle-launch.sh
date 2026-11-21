#!/usr/bin/env bash
# What the shipped app writes when launched and left idle (PR gate F10; PR review F11, F12, F15),
# Linux. Usage, under a display and a session bus, as a desktop session has them:
#   xvfb-run -a dbus-run-session -- bash e2e/idle-launch.sh <program> <out folder>
# <program> is the shipped build: the release binary, or an extracted AppImage's AppRun.
# The program runs with its home, config, data, cache, state and temp folders in a sandbox,
# under strace (every system call but a few frequent ones that change nothing: trace_report.py
# --strace-args), idles for IDLE_SECONDS (20), and is stopped with SIGTERM. Then:
#   - traced.py: every process of the launch must have ended 2 s after the program did;
#   - trace_report.py: every change any of its processes made, anywhere, must be one the
#     profile IDLE_PROFILE (shipped-idle) of e2e/declarations.py allows, each tied to its entry
#     of src/writes.ts; an unknown call, an unreadable line, a lost process or a path whose
#     object cannot be established fails;
#   - manifest.py snapshots before and after, judged by the same declarations: the sandbox, the
#     runtime folder and IDLE_SNAPSHOT (folders a process the trace does not follow may write
#     for the app: dconf-service, started by the session bus, writes the session's home).
# IDLE_DRIVER: a command run once the app has started (after IDLE_SECONDS) that drives the
#   window (xdotool, for example); the observer covers what the driven flow writes. It sees
#   SANDBOX (the sandbox folder) in its environment.
# IDLE_SESSION_HOME: the session's own home ($HOME); IDLE_SNAPSHOT: more folders to snapshot.
set -euo pipefail
PROG=${1:?program}
OUT=${2:?out folder}
SECS=${IDLE_SECONDS:-20}
PROFILE=${IDLE_PROFILE:-shipped-idle}
HERE=$(cd "$(dirname "$0")" && pwd)
# Without a session bus GLib starts one itself (dbus-launch), which writes ~/.dbus and /tmp
# (seen in the owner's run of the 682a0ff AppImage). Every desktop session has a bus.
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
  echo "::error::no session bus: run this under dbus-run-session (usage above)"; exit 2
fi
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
SB="$OUT/sandbox"
rm -rf "$SB" "$OUT/trace" "$OUT/close.txt" "$OUT/trace.txt" "$OUT/manifest.txt"
mkdir -p "$SB/home" "$SB/config" "$SB/data" "$SB/cache" "$SB/state" "$SB/tmp" "$OUT/trace"
RUNTIME=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
SESSION_HOME=${IDLE_SESSION_HOME:-$HOME}
LOG="$OUT/trace/strace.log"
DECLARE=(--profile "$PROFILE" --root DATA="$SB/data" --root CACHE="$SB/cache" --root CONFIG="$SB/config"
  --root HOME="$SB/home" --root RUNTIME="$RUNTIME" --root SESSION_HOME="$SESSION_HOME" --root STDIO="$OUT/launch.log")
# shellcheck disable=SC2206 # a list of folders, split on spaces
SNAP=("$SB" "$RUNTIME" ${IDLE_SNAPSHOT:-})
mapfile -t STRACE_ARGS < <(python3 "$HERE/trace_report.py" --strace-args)

echo "== snapshot before: ${SNAP[*]}"
python3 "$HERE/manifest.py" snapshot "$OUT/before.json" "${SNAP[@]}"
echo "== idle launch of $PROG for ${SECS} s (sandbox $SB)"
# strace -DD: the program keeps this background job's id and strace runs detached, so stopping
# the program stops the app itself, never strace (which would let its processes go untraced).
env HOME="$SB/home" XDG_CONFIG_HOME="$SB/config" XDG_DATA_HOME="$SB/data" XDG_CACHE_HOME="$SB/cache" \
  XDG_STATE_HOME="$SB/state" TMPDIR="$SB/tmp" PRUNEDISK_TRACE_SESSION="$LOG" \
  strace -DD -o "$LOG" "${STRACE_ARGS[@]}" "$PROG" > "$OUT/launch.log" 2>&1 &
app=$!
descendants() { # every process below $1
  local c
  for c in $(pgrep -P "$1" || true); do echo "$c"; descendants "$c"; done
}
sleep "$SECS"
if ! kill -0 "$app" 2> /dev/null; then
  echo "::error::the app exited during the idle launch"; cat "$OUT/launch.log"; wait "$app" || true; exit 1
fi
echo "running after ${SECS} s: $(ps -o comm= -p "$app") $( (descendants "$app" | xargs -r ps -o comm= -p) | sort | uniq -c | tr -s ' \n' ' ')"
driven=0
if [ -n "${IDLE_DRIVER:-}" ]; then
  echo "== driver: $IDLE_DRIVER"
  SANDBOX="$SB" bash -c "$IDLE_DRIVER" || driven=$?
fi
kill -TERM "$app"
# Every process of the launch must end within 2 s of the app; a survivor is killed and fails
# the check (it could write after the trace ends). The app itself gets 15 s for SIGTERM.
survivors=0
python3 "$HERE/traced.py" settle "$LOG" --wait-pid "$app" --wait-max 15 --grace 2 --record "$OUT/close.txt" --label idle-launch || survivors=$?
wait "$app" 2> /dev/null || true

echo "== snapshot after"
python3 "$HERE/manifest.py" snapshot "$OUT/after.json" "${SNAP[@]}"
echo "== files in the sandbox after the launch"
(cd "$SB" && find . -mindepth 1 -printf '%y %s %P\n' | sort -k3) > "$OUT/sandbox.txt"
grep -v ' cache/mesa_shader_cache' "$OUT/sandbox.txt" || true
echo "== trace"
trace=0
python3 "$HERE/trace_report.py" "$LOG" "${DECLARE[@]}" --links-from "$OUT/before.json" > "$OUT/trace.txt" || trace=$?
{ grep -E '^(LATE|LOST|UNKNOWN|UNSUPPORTED|UNPARSED|UNRESOLVED|OTHER|declared|unreadable|trace:|  at )' "$OUT/trace.txt" | grep -v mesa_shader_cache | head -200; } || true
echo "== snapshot compare"
nowrite=0
python3 "$HERE/trace_report.py" --snapshot "$OUT/before.json" "$OUT/after.json" "${DECLARE[@]}" > "$OUT/manifest.txt" || nowrite=$?
{ grep -v '^declared .*mesa_shader_cache' "$OUT/manifest.txt" | head -100; } || true
echo "idle launch: driver exit $driven, survivors exit $survivors, trace exit $trace, snapshot exit $nowrite"
[ "$driven" = 0 ] || { echo "::error::the driver failed (exit $driven)"; exit 1; }
[ "$survivors" = 0 ] || { echo "::error::a process of the launch outlived it (above; $OUT/close.txt)"; exit 1; }
[ "$trace" = 0 ] || { echo "::error::the launch changed what nobody declared, or the trace cannot vouch for it (above; $OUT/trace.txt)"; exit 1; }
[ "$nowrite" = 0 ] || { echo "::error::a folder changed in a way nobody declared (above; $OUT/manifest.txt)"; exit 1; }
echo "Every change of the launch is one of the declared ones."
