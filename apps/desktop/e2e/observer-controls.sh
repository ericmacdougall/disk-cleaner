#!/usr/bin/env bash
# The write observer's controls (PR review F11, F12, F15), Linux: each planted action of
# e2e/observer_controls.py runs under the same strace launch and survivor check as the app
# (trace_report.py --strace-args, strace -DD, traced.py settle), and must be caught by the check
# named for it; the positive controls must pass every check. Also: a log with a line strace
# never writes (UNPARSED), a log cut before the program's execve (LATE).
# usage: bash e2e/observer-controls.sh [--install] OUT
#   --install  installs strace, python3 and procps first (a bare container, as root)
# Exit 0 when every control gives its result, 1 when one does not (each is printed), 2 on bad
# usage. Results: OUT/controls.txt, and per control OUT/run-<name>/ (trace.log, trace.txt,
# manifest.txt, close.txt); OUT/ctl-<name>/ is the folder the action works in.
set -euo pipefail
if [ "${1:-}" = --install ]; then
  shift
  . /etc/os-release
  case " $ID ${ID_LIKE:-} " in
    *" fedora "*) dnf -y -q install --setopt=install_weak_deps=False strace python3 procps-ng findutils > /dev/null ;;
    *" debian "*) { apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends strace python3 procps; } > /dev/null ;;
    *) echo "unsupported host $ID"; exit 2 ;;
  esac
fi
OUT=${1:?out folder}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd -P)
RESULTS="$OUT/controls.txt"
: > "$RESULTS"
mapfile -t STRACE_ARGS < <(python3 "$HERE/trace_report.py" --strace-args)
echo "== $(strace -V | head -1); $(uname -sr)" | tee -a "$RESULTS"
failed=0

# What each control must produce (observer_controls.py, EXPECT): "pass" (every check passes),
# "TRACE <KIND> <path>" (a line of trace.txt with that kind and path; CTL is the control folder,
# TMP the O_TMPFILE folder), "TRACE-PREFIX <text>", "SNAPSHOT <line of manifest.txt>",
# "SURVIVOR <name>", "COVERAGE".
declare -A WANT=()
ACTIONS=()
listing=$(python3 -B -I "$HERE/observer_controls.py" --list) || { echo "::error::observer_controls.py --list failed" | tee -a "$RESULTS"; exit 1; }
while IFS=$'\t' read -r a w; do
  ACTIONS+=("$a")
  WANT[$a]=$w
done <<< "$listing"
[ "${#ACTIONS[@]}" -gt 30 ] || { echo "::error::only ${#ACTIONS[@]} observer controls listed" | tee -a "$RESULTS"; exit 1; }

prepare() { # the control folder of one action, and its results folder
  CTL="$OUT/ctl-$1"
  R="$OUT/run-$1"
  TMP=/dev/shm/prunedisk-ctl-$$-$1
  rm -rf "$CTL" "$R" "$TMP"
  mkdir -p "$CTL/sb/home" "$CTL/sb/subtree" "$CTL/outside-emptydir" "$R"
  : > "$CTL/stdout.log"
  mkdir -p "$TMP" 2> /dev/null || TMP="$CTL/tmpfile"
  mkdir -p "$TMP"
  echo victim > "$CTL/outside-victim.txt"
  echo src > "$CTL/outside-src.txt"
  : > "$CTL/outside-inherited.txt"
  head -c 8192 /dev/zero > "$CTL/outside-mapped.bin"
  ln -s "$CTL/outside-victim.txt" "$CTL/sb/declared-link"
  mapfile -t DECLARE < <(python3 -B -I "$HERE/observer_controls.py" declare "$CTL")
}

has_line() { # KIND PATH FILE: a line "KIND <calls>: PATH" in FILE
  python3 -c 'import sys
k, p = sys.argv[1], sys.argv[2]
sys.exit(0 if any(l.startswith(k + " ") and l.rstrip("\n").endswith(": " + p) for l in open(sys.argv[3])) else 1)' "$@"
}

judge() { # one action: run it traced, then every check
  local a=$1 want=${WANT[$1]:-pass} kind rest got=""
  prepare "$a"
  local log="$R/trace.log"
  python3 "$HERE/manifest.py" snapshot "$R/before.json" "$CTL" > /dev/null
  if [ "$a" = ask-service ]; then
    python3 -B -I "$HERE/observer_controls.py" service "$CTL" &
  fi
  PRUNEDISK_TRACE_SESSION="$log" CTL_TMPFILE_DIR="$TMP" strace -DD -o "$log" "${STRACE_ARGS[@]}" \
    python3 -B -I "$HERE/observer_controls.py" "$a" "$CTL" 3>> "$CTL/outside-inherited.txt" 4<> "$CTL/outside-mapped.bin" \
    > "$CTL/stdout.log" 2>&1 &
  local app=$!
  if [ "$a" = slow-write ]; then
    # Kill strace (SIGKILL: strace blocks the others) while the program sleeps: the kernel lets
    # it go and it writes untraced. The report must say it lost the process.
    sleep 1
    local t
    for t in $(python3 "$HERE/traced.py" tracers "$log"); do kill -KILL "$t"; done
  fi
  local survivors=0 trace=0 snap=0
  python3 "$HERE/traced.py" settle "$log" --wait-pid "$app" --wait-max 90 --grace 2 --record "$R/close.txt" --label "$a" > /dev/null || survivors=$?
  local code=0
  wait "$app" || code=$?
  if [ "$code" != 0 ]; then
    echo "::error::control $a: the action itself failed (exit $code: $(head -c 300 "$CTL/stdout.log" | tr '\n' ' '))" | tee -a "$RESULTS"
    failed=1
  fi
  wait # the service, if any
  python3 "$HERE/manifest.py" snapshot "$R/after.json" "$CTL" > /dev/null
  python3 "$HERE/trace_report.py" "$log" "${DECLARE[@]}" --links-from "$R/before.json" > "$R/trace.txt" || trace=$?
  python3 "$HERE/trace_report.py" --snapshot "$R/before.json" "$R/after.json" "${DECLARE[@]}" > "$R/manifest.txt" || snap=$?
  kind=${want%% *}
  rest=${want#* }
  rest=${rest//CTL/$CTL}
  rest=${rest//TMP/$TMP}
  case "$kind" in
    pass) [ "$trace$snap$survivors" = 000 ] && got=yes ;;
    TRACE) [ "$trace" = 1 ] && has_line "${rest%% *}" "${rest#* }" "$R/trace.txt" && got=yes ;;
    TRACE-PREFIX) [ "$trace" = 1 ] && grep -q -F -- "$rest" "$R/trace.txt" && got=yes ;;
    SNAPSHOT) [ "$snap" = 1 ] && grep -q -x -F -- "$rest" "$R/manifest.txt" && got=yes ;;
    SURVIVOR) [ "$survivors" = 1 ] && grep -q "^survivor $a: .* name=$rest " "$R/close.txt" && got=yes ;;
    COVERAGE) python3 -B -I "$HERE/observer_controls.py" check-coverage "$CTL" "$log" | tee "$R/coverage.txt" | grep -v '^NOT' || true
      [ "$survivors" = 0 ] && python3 -B -I "$HERE/observer_controls.py" check-coverage "$CTL" "$log" > /dev/null && got=yes ;;
  esac
  local verdict="caught"
  [ "$kind" = pass ] || [ "$kind" = COVERAGE ] && verdict="passed"
  if [ "$got" = yes ]; then
    echo "control $a: $verdict ($want; trace exit $trace, snapshot exit $snap, survivors exit $survivors)" | tee -a "$RESULTS"
  else
    echo "::error::control $a: NOT $verdict (wanted $want; trace exit $trace, snapshot exit $snap, survivors exit $survivors)" | tee -a "$RESULTS"
    grep -E '^(LATE|LOST|UNKNOWN|UNSUPPORTED|UNPARSED|UNRESOLVED|OTHER|trace:)' "$R/trace.txt" | head -20 || true
    grep -E '^(UNDECLARED|manifest:)' "$R/manifest.txt" | head -10 || true
    grep '^survivor' "$R/close.txt" | head -5 || true
    failed=1
  fi
  rm -rf "$TMP"
}

for a in "${ACTIONS[@]}"; do judge "$a"; done

# Two controls on the log itself, from the positive control's log.
P="$OUT/ctl-positive"
PR="$OUT/run-positive"
mapfile -t DECLARE < <(python3 -B -I "$HERE/observer_controls.py" declare "$P")
log_control() { # name, wanted text, the log
  local code=0
  python3 "$HERE/trace_report.py" "$3" "${DECLARE[@]}" --links-from "$PR/before.json" > "$3.txt" || code=$?
  if [ "$code" = 1 ] && grep -q -F -- "$2" "$3.txt"; then
    echo "control $1: caught ($2)" | tee -a "$RESULTS"
  else
    echo "::error::control $1: NOT caught (wanted $2, exit $code)" | tee -a "$RESULTS"; failed=1
  fi
}
{ cat "$PR/trace.log"; echo "$(head -1 "$PR/trace.log" | cut -d' ' -f1) 1700000000.000000 this line is not strace's"; } > "$OUT/planted-unparsed.log"
log_control unparsed-line "UNPARSED line: this line is not strace's" "$OUT/planted-unparsed.log"
tail -n +2 "$PR/trace.log" > "$OUT/planted-late.log"
log_control late-log "LATE " "$OUT/planted-late.log"
# The same call as unknown-syscall, as a line in a log: a number no kernel has.
{ cat "$PR/trace.log"; echo "$(head -1 "$PR/trace.log" | cut -d' ' -f1) 1700000000.000000 syscall_0x3e8(0xfffffffffffffff9, 0, 0, 0, 0, 0) = -1 ENOSYS (Function not implemented)"; } > "$OUT/planted-unknown.log"
log_control unknown-call "UNKNOWN syscall_0x3e8: " "$OUT/planted-unknown.log"

if [ "$failed" = 0 ]; then
  echo "observer controls: every control gave its result ($(grep -c '^control ' "$RESULTS") controls)" | tee -a "$RESULTS"
else
  echo "::error::observer controls: a control did not give its result (above; $RESULTS)" | tee -a "$RESULTS"
fi
exit "$failed"
