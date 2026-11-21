#!/usr/bin/env bash
# AC-7 benchmark: xfer against the baselines on generated workloads, interleaved and repeated.
# Runs on the host that holds the rclone remote (t2-dev). Keys stay inside rclone: this script
# never reads rclone's config; it only names a remote.
#
# usage: bench/run.sh            (env knobs below; RUN defaults to a timestamp)
#   RUN=r1 WORKLOADS="w2 w1" SIZE=1Gi REPS=1 bench/run.sh      # thin slice
#   RUN=r2 bench/run.sh                                         # full: w1 w2 w3 w4, 4 GiB, 3 reps
#
# Results: $OUT/results.tsv (one row per workload/tool/rep; rows already present are skipped,
# so a rerun with the same RUN resumes). Remote prefixes: $BASE/$RUN/<workload>/<tool>/<rep>,
# each purged by its literal path after the cell.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-/dev/shm/xfer-work/target/release/prunedisk-xfer}
RCLONE=${RCLONE:-/usr/bin/rclone}
BASE=${BASE:-b2-archive2:prunebackup/xfer-bench}
RUN=${RUN:-$(date -u +%Y%m%dT%H%M%SZ)}
WORKLOADS=${WORKLOADS:-w1 w2 w3 w4}
SIZE=${SIZE:-4Gi}       # W1 and W2 (W4 is W1 plus a change)
SIZE_W3=${SIZE_W3:-800Mi}
REPS=${REPS:-3}
DATA=${DATA:-/dev/shm/xfer-bench/$RUN}
OUT=${OUT:-$HOME/xfer-work/bench/$RUN}
SEED=${SEED:-1}
XFER_ARGS=${XFER_ARGS:-}
KEEP_DATA=${KEEP_DATA:-0}

die() { echo "run.sh: $*" >&2; exit 1; }

[[ $RUN =~ ^[A-Za-z0-9_-]+$ ]] || die "RUN must be [A-Za-z0-9_-]+"
case "$BASE" in
  *disk-cleaner-archive*|*..*) die "refusing BASE $BASE" ;;
  */xfer-bench) ;;
  *) die "BASE must end in /xfer-bench" ;;
esac

# The only remote paths this script writes or purges are under $BASE/$RUN/.
guard() {
  local p=$1
  case "$p" in *disk-cleaner-archive*|*..*|*//*) die "refusing remote path $p" ;; esac
  case "$p" in "$BASE/$RUN/"?*) ;; *) die "remote path $p is outside $BASE/$RUN/" ;; esac
}
purge() { guard "$1"; "$RCLONE" purge "$1" >/dev/null 2>&1 || true; }

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
  echo "workloads=$WORKLOADS size=$SIZE size_w3=$SIZE_W3 reps=$REPS seed=$SEED base=$BASE"
} >>"$OUT/env.txt"

gen() { # gen KIND DIR (the marker lives beside the tree, so it is not part of the workload)
  [[ -f $2.gen-done ]] && return
  rm -rf "$2"
  local size=$SIZE
  [[ $1 == w3 ]] && size=$SIZE_W3
  "$BIN" gen --kind "$1" --size "$size" --seed "$SEED" --dir "$2" >"$OUT/gen-$1.txt"
  touch "$2.gen-done"
}

src_of() { case $1 in w1|w4) echo "$DATA/src-w1" ;; w2) echo "$DATA/src-w2" ;; w3) echo "$DATA/src-w3" ;; esac; }
tools_of() {
  case $1 in
    w1) echo "xfer xfer-abl a b bplus" ;;
    w2) echo "xfer b bplus" ;;
    w3) echo "xfer a b" ;;
    w4) echo "xfer b" ;;
  esac
}

for w in $WORKLOADS; do
  case $w in w1|w4) gen w1 "$DATA/src-w1" ;; w2) gen w2 "$DATA/src-w2" ;; w3) gen w3 "$DATA/src-w3" ;; *) die "workload $w" ;; esac
done
du -sb "$DATA"/src-* >>"$OUT/env.txt" 2>/dev/null || true

xfer_backup() { # xfer_backup PREFIX SRC STATE JSON [extra...]
  local p=$1 s=$2 st=$3 j=$4; shift 4
  # shellcheck disable=SC2086
  "$BIN" backup --repo "$p" --rclone "$RCLONE" --source "$s" --state "$st" --json "$j" \
    --receipts "$st/receipts.jsonl" $XFER_ARGS "$@" >/dev/null
}

cell() { # cell WORKLOAD TOOL REP
  local w=$1 t=$2 r=$3 src p c kind out note=""
  src=$(src_of "$w")
  p=$BASE/$RUN/$w/$t/$r
  guard "$p"
  c=$OUT/cell-$w-$t-$r
  rm -rf "$c" && mkdir -p "$c"
  purge "$p"
  case $t in
    xfer|xfer-abl)
      local extra=()
      [[ $t == xfer-abl ]] && extra=(--level 3 --streams 4)
      local s=$src
      if [[ $w == w4 ]]; then
        s=$DATA/w4-src
        rm -rf "$s" && cp -a "$src" "$s"
        xfer_backup "$p" "$s" "$c/state" "$c/first.json" "${extra[@]}"
        "$BIN" mutate --dir "$s" --seed "$SEED" >/dev/null
      fi
      python3 "$HERE/measure.py" "$c/measure.json" -- \
        "$BIN" backup --repo "$p" --rclone "$RCLONE" --source "$s" --state "$c/state" \
        --json "$c/report.json" --receipts "$c/state/receipts.jsonl" $XFER_ARGS "${extra[@]}" \
        >/dev/null || true
      kind=xfer out=$c/report.json
      ;;
    b|bplus)
      local n=4
      [[ $t == bplus ]] && n=16
      local s=$src
      if [[ $w == w4 ]]; then
        s=$DATA/w4-src
        rm -rf "$s" && cp -a "$src" "$s"
        "$RCLONE" copy "$s" "$p" --transfers "$n" >/dev/null 2>&1
        "$BIN" mutate --dir "$s" --seed "$SEED" >/dev/null
      fi
      python3 "$HERE/measure.py" "$c/measure.json" -- \
        "$RCLONE" copy "$s" "$p" --transfers "$n" --use-json-log -v --stats 1000h \
        2>"$c/rclone.log" || true
      kind=rclone out=$c/rclone.log note="requests=files transferred"
      ;;
    a)
      local arch=$DATA/a-$w.tar.zst
      rm -f "$arch"
      python3 "$HERE/measure.py" "$c/measure.json" -- sh -c '
        tar -C "$1" -cf - . | zstd -3 -T0 -q -o "$2" && "$3" copy "$2" "$4" --use-json-log -v --stats 1000h' \
        sh "$src" "$arch" "$RCLONE" "$p" 2>"$c/rclone.log" || true
      rm -f "$arch"
      kind=rclone out=$c/rclone.log note="tar+zstd staged in /dev/shm; requests=files transferred"
      ;;
    *) die "tool $t" ;;
  esac
  "$RCLONE" size --json "$p" >"$c/stored.json" 2>/dev/null || echo '{}' >"$c/stored.json"
  python3 "$HERE/row.py" "$TSV" "$RUN" "$w" "$t" "$r" "$c/measure.json" "$kind" "$out" \
    "$c/stored.json" "$note"
  if [[ $t == xfer && $r == 1 ]]; then restore_check "$w" "$p" "$c"; fi
  purge "$p"
  rm -rf "$DATA/w4-src"
}

restore_check() { # restore of rep 1, compared by full manifest with the source it backed up
  local w=$1 p=$2 c=$3 tgt=$DATA/restore s
  s=$(src_of "$w")
  [[ $w == w4 ]] && s=$DATA/w4-src
  rm -rf "$tgt"
  python3 "$HERE/measure.py" "$c/restore-measure.json" -- \
    "$BIN" restore --repo "$p" --rclone "$RCLONE" --target "$tgt" --json "$c/restore.json" \
    >/dev/null || true
  local verdict
  if python3 "$HERE/manifest.py" "$s" "$tgt" >"$c/manifest.txt" 2>&1; then verdict=match; else verdict=MISMATCH; fi
  python3 "$HERE/row.py" "$TSV" "$RUN" "$w" "xfer-restore" 1 "$c/restore-measure.json" none - - \
    "manifest $verdict: $(head -1 "$c/manifest.txt")"
  rm -rf "$tgt"
}

for r in $(seq 1 "$REPS"); do
  for w in $WORKLOADS; do
    read -r -a tools <<<"$(tools_of "$w")"
    n=${#tools[@]}
    for i in $(seq 0 $((n - 1))); do
      t=${tools[$(((i + r - 1) % n))]} # rotate the order each rep
      done_row "$w" "$t" "$r" && continue
      echo "== $w $t rep $r"
      cell "$w" "$t" "$r"
    done
  done
done

if [[ $KEEP_DATA != 1 ]]; then rm -rf "$DATA"; fi
echo "results: $TSV"
