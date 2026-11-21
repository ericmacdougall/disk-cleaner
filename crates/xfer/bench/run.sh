#!/usr/bin/env bash
# AC-7 benchmark: xfer against the baselines on generated workloads, interleaved and repeated.
# Runs on the host that holds the rclone remote (t2-dev). Keys stay inside rclone: this script
# never reads rclone's config; it only names a remote.
#
# usage: bench/run.sh            (env knobs below; RUN defaults to a timestamp)
#   RUN=r2 bench/run.sh                                   # main's ruled layout, 3 reps
#   RUN=big WORKLOADS=w1 SIZE=20Gi REPS=1 TOOLS_W1="xfer a" RESTORE=0 bench/run.sh
#
# Scale (main's ruling, ASSUMPTION / TARGET_MISMATCH against the spec's table): W1 1 GiB (spec
# 4 GiB), W2 4 GiB (as specced), W3 80 MiB of about 4 KiB files, about 20k files (spec 200k),
# W4 = W1 after the change. Per rep W1 is regenerated, its cells run, the tree is changed in
# place, and W4 runs against the same rep's W1 xfer repository and W1 (b) prefix.
#
# Results: $OUT/results.tsv (one row per workload/tool/rep; rows already present are skipped,
# so a rerun with the same RUN resumes; a W4 cell whose W1 base is gone uploads a fresh base
# first, unmeasured). Remote prefixes: $BASE/$RUN/<workload>/<tool>/<rep>, purged by literal path;
# at the end $BASE/$RUN itself is purged and the empty listing is recorded in $OUT/purge.txt.
# A single-stream upload probe before and after each rep records the uplink the bench had.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-/dev/shm/xfer-work/target/release/prunedisk-xfer}
RCLONE=${RCLONE:-/usr/bin/rclone}
BASE=${BASE:-b2-archive2:prunebackup/xfer-bench}
RUN=${RUN:-$(date -u +%Y%m%dT%H%M%SZ)}
WORKLOADS=${WORKLOADS:-w1 w2 w3 w4}
SIZE=${SIZE:-1Gi}        # W1 (and so W4)
SIZE_W2=${SIZE_W2:-4Gi}
SIZE_W3=${SIZE_W3:-80Mi} # mean about 4 KiB, so about 20k files
REPS=${REPS:-3}
DATA=/dev/shm/xfer-bench/$RUN
OUT=${OUT:-$HOME/xfer-work/bench/$RUN}
SEED=${SEED:-1}
XFER_ARGS=${XFER_ARGS:-}
KEEP_DATA=${KEEP_DATA:-0}
RESTORE=${RESTORE:-1}
PROBE=${PROBE:-128Mi}
TOOLS_W1=${TOOLS_W1:-xfer xfer-abl a b bplus}
TOOLS_W2=${TOOLS_W2:-xfer b bplus}
TOOLS_W3=${TOOLS_W3:-xfer a b}
TOOLS_W4=${TOOLS_W4:-xfer b}

die() { echo "run.sh: $*" >&2; exit 1; }

[[ $RUN =~ ^[A-Za-z0-9_-]+$ ]] || die "RUN must be [A-Za-z0-9_-]+"
case "$BASE" in
  *disk-cleaner-archive*|*..*) die "refusing BASE $BASE" ;;
  */xfer-bench) ;;
  *) die "BASE must end in /xfer-bench" ;;
esac

# The only remote paths this script writes or purges are under $BASE/$RUN/ (and, at the very
# end, $BASE/$RUN itself).
guard() {
  local p=$1
  case "$p" in *disk-cleaner-archive*|*..*|*//*) die "refusing remote path $p" ;; esac
  case "$p" in "$BASE/$RUN/"?*) ;; *) die "remote path $p is outside $BASE/$RUN/" ;; esac
}
purge() { guard "$1"; "$RCLONE" purge "$1" >/dev/null 2>&1 || true; }

# The only local deletions: generated data under /dev/shm/xfer-bench/$RUN and this run's own
# cell directories. Anything else is refused.
wipe() {
  local p
  for p in "$@"; do
    case "$p" in *..*) die "refusing to delete $p" ;; esac
    case "$p" in
      "/dev/shm/xfer-bench/$RUN"|"/dev/shm/xfer-bench/$RUN/"?*|"$OUT/cell-"?*|"$OUT/probe-"?*) ;;
      *) die "refusing to delete $p" ;;
    esac
    rm -rf -- "$p"
  done
}

mkdir -p "$OUT" "$DATA"
TSV=$OUT/results.tsv
done_row() { [[ -f $TSV ]] && awk -F'\t' -v w="$1" -v t="$2" -v r="$3" '$2==w && $3==t && $4==r {f=1} END {exit !f}' "$TSV"; }

{
  echo "run=$RUN date=$(date -u +%FT%TZ) host=$(hostname) nproc=$(nproc)"
  echo "cpu=$(awk -F: '/model name/ {print $2; exit}' /proc/cpuinfo 2>/dev/null | sed 's/^ *//')"
  echo "mem=$(awk '/MemTotal/ {print $2 " kB"}' /proc/meminfo 2>/dev/null)"
  echo "load=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null)"
  echo "xfer=$("$BIN" --version) engine_args=${XFER_ARGS:-defaults}"
  echo "rclone=$("$RCLONE" version | head -1) zstd=$(zstd --version 2>&1 | head -1)"
  echo "restic=$(command -v restic >/dev/null && restic version || echo absent)"
  echo "workloads=$WORKLOADS size_w1=$SIZE size_w2=$SIZE_W2 size_w3=$SIZE_W3 reps=$REPS seed=$SEED base=$BASE"
} >>"$OUT/env.txt"

gen() { # gen KIND DIR SIZE (the marker lives beside the tree, so it is not part of the workload)
  [[ -f $2.gen-done ]] && return
  wipe "$2"
  "$BIN" gen --kind "$1" --size "$3" --seed "$SEED" --dir "$2" >"$OUT/gen-$1.txt"
  touch "$2.gen-done"
  du -sb "$2" >>"$OUT/env.txt"
  find "$2" -type f | wc -l | sed "s|^|files $2: |" >>"$OUT/env.txt"
}

tools_of() {
  case $1 in w1) echo "$TOOLS_W1" ;; w2) echo "$TOOLS_W2" ;; w3) echo "$TOOLS_W3" ;; w4) echo "$TOOLS_W4" ;; esac
}

# W1 cells whose remote state this rep's W4 cells reuse (tool -> prefix and state dir)
declare -A W1_PREFIX=()
declare -A W1_STATE=()

probe() { # a single-stream upload of PROBE incompressible bytes: the uplink at that moment
  local r=$1 when=$2
  local c=$OUT/probe-$r-$when p=$BASE/$RUN/probe/$r-$when
  done_row uplink "probe-$when" "$r" && return
  guard "$p"
  mkdir -p "$c"
  [[ -f $DATA/probe.bin ]] || head -c "$(numfmt --from=iec-i "$PROBE")" /dev/urandom >"$DATA/probe.bin"
  python3 "$HERE/measure.py" "$c/measure.json" -- \
    "$RCLONE" copyto "$DATA/probe.bin" "$p/probe.bin" --transfers 1 --use-json-log -v \
    --stats 1000h 2>"$c/rclone.log" || true
  python3 "$HERE/row.py" "$TSV" "$RUN" uplink "probe-$when" "$r" "$c/measure.json" rclone \
    "$c/rclone.log" - "single-stream upload of $PROBE; MB/s = bytes_sent / wall_s"
  purge "$p"
}

cell() { # cell WORKLOAD TOOL REP
  local w=$1 t=$2 r=$3 src p c kind out note=""
  case $w in w1|w4) src=$DATA/src-w1 ;; w2) src=$DATA/src-w2 ;; w3) src=$DATA/src-w3 ;; esac
  p=$BASE/$RUN/$w/$t/$r
  c=$OUT/cell-$w-$t-$r
  wipe "$c" && mkdir -p "$c"
  local s=$src state=$c/state
  if [[ $w == w4 ]]; then
    if [[ -n ${W1_PREFIX[$t]:-} ]]; then
      p=${W1_PREFIX[$t]}
      state=${W1_STATE[$t]}
      note="against this rep's W1 $t state;"
    else
      # resumed run: the W1 base is gone, so upload a fresh base first (unmeasured)
      s=$DATA/w4-src
      wipe "$s"
      "$BIN" gen --kind w1 --size "$SIZE" --seed "$SEED" --dir "$s" >/dev/null
      purge "$p"
      if [[ $t == xfer ]]; then
        "$BIN" backup --repo "$p" --rclone "$RCLONE" --source "$s" --state "$state" \
          --json "$c/first.json" --receipts "$state/receipts.jsonl" $XFER_ARGS >/dev/null
      else
        "$RCLONE" copy "$s" "$p" --transfers 4 >/dev/null 2>&1
      fi
      "$BIN" mutate --dir "$s" --seed "$SEED" >/dev/null
      note="fresh base (resumed);"
    fi
  else
    purge "$p"
  fi
  guard "$p"
  case $t in
    xfer|xfer-abl)
      local extra=()
      [[ $t == xfer-abl ]] && extra=(--level 3 --streams 4)
      python3 "$HERE/measure.py" "$c/measure.json" -- \
        "$BIN" backup --repo "$p" --rclone "$RCLONE" --source "$s" --state "$state" \
        --json "$c/report.json" --receipts "$state/receipts.jsonl" $XFER_ARGS "${extra[@]}" \
        >/dev/null || true
      kind=xfer out=$c/report.json
      ;;
    b|bplus)
      local n=4
      [[ $t == bplus ]] && n=16
      python3 "$HERE/measure.py" "$c/measure.json" -- \
        "$RCLONE" copy "$s" "$p" --transfers "$n" --use-json-log -v --stats 1000h \
        2>"$c/rclone.log" || true
      kind=rclone out=$c/rclone.log note="$note requests=files transferred"
      ;;
    a)
      local arch=$DATA/a-$w.tar.zst
      wipe "$arch"
      python3 "$HERE/measure.py" "$c/measure.json" --watch "$DATA" -- sh -c '
        tar -C "$1" -cf - . | zstd -3 -T0 -q -o "$2" && "$3" copy "$2" "$4" --use-json-log -v --stats 1000h' \
        sh "$s" "$arch" "$RCLONE" "$p" 2>"$c/rclone.log" || true
      wipe "$arch"
      kind=rclone out=$c/rclone.log note="tar+zstd staged in /dev/shm; requests=files transferred"
      ;;
    *) die "tool $t" ;;
  esac
  "$RCLONE" size --json "$p" >"$c/stored.json" 2>/dev/null || echo '{}' >"$c/stored.json"
  python3 "$HERE/row.py" "$TSV" "$RUN" "$w" "$t" "$r" "$c/measure.json" "$kind" "$out" \
    "$c/stored.json" "$note"
  if [[ $t == xfer && $RESTORE == 1 ]] && [[ $r == 1 || $w == w4 ]]; then
    restore_check "$w" "$p" "$c" "$s"
  fi
  if [[ $w == w1 && ( $t == xfer || $t == b ) && " $WORKLOADS " == *" w4 "* ]]; then
    W1_PREFIX[$t]=$p # kept for this rep's W4 cells, purged after them
    W1_STATE[$t]=$state
  else
    purge "$p"
  fi
  wipe "$DATA/w4-src"
}

restore_check() { # restore, compared by full manifest with the tree it backed up
  local w=$1 p=$2 c=$3 s=$4 tgt=$DATA/restore verdict
  wipe "$tgt"
  python3 "$HERE/measure.py" "$c/restore-measure.json" -- \
    "$BIN" restore --repo "$p" --rclone "$RCLONE" --target "$tgt" --json "$c/restore.json" \
    >/dev/null || true
  if python3 "$HERE/manifest.py" "$s" "$tgt" >"$c/manifest.txt" 2>&1; then verdict=match; else verdict=MISMATCH; fi
  python3 "$HERE/row.py" "$TSV" "$RUN" "$w" "xfer-restore" "$(basename "$c" | sed 's/.*-//')" \
    "$c/restore-measure.json" none - - "manifest $verdict: $(head -1 "$c/manifest.txt")"
  wipe "$tgt"
}

ordered=""
for w in w1 w2 w3 w4; do [[ " $WORKLOADS " == *" $w "* ]] && ordered="$ordered $w"; done
[[ " $WORKLOADS " == *" w2 "* ]] && gen w2 "$DATA/src-w2" "$SIZE_W2"
[[ " $WORKLOADS " == *" w3 "* ]] && gen w3 "$DATA/src-w3" "$SIZE_W3"

for r in $(seq 1 "$REPS"); do
  W1_PREFIX=() W1_STATE=()
  probe "$r" before
  for w in $ordered; do
    if [[ $w == w1 ]]; then
      wipe "$DATA/src-w1.gen-done" # a fresh, unchanged W1 tree each rep
      gen w1 "$DATA/src-w1" "$SIZE"
    fi
    if [[ $w == w4 ]]; then
      [[ -d $DATA/src-w1 ]] || gen w1 "$DATA/src-w1" "$SIZE"
      "$BIN" mutate --dir "$DATA/src-w1" --seed "$SEED" >/dev/null
    fi
    read -r -a tools <<<"$(tools_of "$w")"
    n=${#tools[@]}
    for i in $(seq 0 $((n - 1))); do
      t=${tools[$(((i + r - 1) % n))]} # rotate the order each rep
      done_row "$w" "$t" "$r" && continue
      echo "== $w $t rep $r $(date -u +%T)"
      cell "$w" "$t" "$r"
    done
  done
  for t in "${!W1_PREFIX[@]}"; do purge "${W1_PREFIX[$t]}"; done
  probe "$r" after
done

# Purge the run's whole prefix and record that nothing is left.
case "$BASE/$RUN" in *disk-cleaner-archive*|*..*) die "refusing final purge" ;; esac
"$RCLONE" purge "$BASE/$RUN" >/dev/null 2>&1 || true
{
  echo "purged $BASE/$RUN at $(date -u +%FT%TZ)"
  echo "remaining objects under $BASE/$RUN: $("$RCLONE" lsf -R "$BASE/$RUN" 2>/dev/null | wc -l)"
  echo "entries named $RUN/ in a listing of $BASE/: $("$RCLONE" lsf "$BASE/" 2>/dev/null | grep -c "^$RUN/" || true)"
} >>"$OUT/purge.txt"
cat "$OUT/purge.txt"

if [[ $KEEP_DATA != 1 ]]; then wipe "$DATA"; fi
echo "results: $TSV"
