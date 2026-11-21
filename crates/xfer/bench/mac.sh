#!/usr/bin/env bash
# AC-8 on the Mac: xfer against the Mac baselines on the Mac's small workload, through the real
# rclone transport, with the Mac profile. Then the memory of the load phase and of a restore
# through rclone rcd and its JSON listing, with a repository whose pack listing is large: a
# synthetic chunk map of tiny listed packs (`prunedisk-xfer gen-index`) copied into the run's
# repository. --max-pack cannot go below 16 MiB, so 300 MiB of data alone gives about 20 packs.
# Keys stay inside rclone: this script never reads rclone's config; it only names a remote.
# Bash 3.2 compatible (macOS /bin/bash).
#
# usage: BIN=/path/to/ci-built/prunedisk-xfer bench/mac.sh
#   env: RUN, REPS (3), SIZE (300Mi), SEED (1), FIXTURE (1 runs the large-listing stage),
#        OUT (results dir), BASE (b2-archive2:prunebackup/xfer-bench), RCLONE, PROBE (32Mi)
#
# Results in $OUT: results.tsv (row.py's columns), mem.tsv (peak RSS of the engine and of rcd
# separately, df drop, cache and TMPDIR peaks, swap growth, per cell), checks.txt (the AC-8 and
# share verdicts), purge.txt (the empty listing after the final purge), env.txt.
# Disk: generated data, the staged tar, restore targets and the fixture live under
# $TMPDIR/xfer-mac/$RUN and are removed as each step ends; the whole run stays under 1 GiB
# (asserted from the du peaks). Remote: only $BASE/$RUN/ is written, and it is purged at the end.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:?BIN must name the CI-built macOS prunedisk-xfer}
SUMS=${SUMS:-$(dirname "$BIN")/SHA256SUMS}
RCLONE=${RCLONE:-$(command -v rclone)}
BASE=${BASE:-b2-archive2:prunebackup/xfer-bench}
RUN=${RUN:-mac-$(date -u +%Y%m%dT%H%M%SZ)}
REPS=${REPS:-3}
SIZE=${SIZE:-300Mi}
SEED=${SEED:-1}
FIXTURE=${FIXTURE:-1}
PROBE_MIB=${PROBE_MIB:-32}
T=${TMPDIR:-/tmp}
T=${T%/}
DATA=$T/xfer-mac/$RUN
OUT=${OUT:-$HOME/xfer-work/bench/$RUN}
PROFILE=(--mem-budget 640Mi --max-pack 16Mi --max-streams 4)
# The Mac profile's supported sizes (crates/xfer/README.md, pinned by supported_sizes_per_profile):
# backup 604,855 and restore 767,192 committed chunks at 16 chunks a pack.
FIX1=${FIX1:-580000}
FIX2=${FIX2:-760000}
NEED_FREE=$((3 << 30))
DISK_MAX=$((1 << 30))
RSS_MAX=$((650 << 20))
SHARE_ALLOW=$(((80 + 16) << 20)) # index share plus one fetch batch, as tests/hostile_restore.rs

die() { echo "mac.sh: $*" >&2; exit 1; }

[[ $RUN =~ ^[A-Za-z0-9_-]+$ ]] || die "RUN must be [A-Za-z0-9_-]+"
case "$BASE" in
  *disk-cleaner-archive*|*..*) die "refusing BASE $BASE" ;;
  */xfer-bench) ;;
  *) die "BASE must end in /xfer-bench" ;;
esac
guard() {
  case "$1" in *disk-cleaner-archive*|*..*|*//*) die "refusing remote path $1" ;; esac
  case "$1" in "$BASE/$RUN/"?*) ;; *) die "remote path $1 is outside $BASE/$RUN/" ;; esac
}
purge() { guard "$1"; "$RCLONE" purge "$1" >/dev/null 2>&1 || true; }
wipe() {
  local p
  for p in "$@"; do
    case "$p" in *..*) die "refusing to delete $p" ;; esac
    case "$p" in "$DATA"|"$DATA/"?*) ;; *) die "refusing to delete $p" ;; esac
    rm -rf -- "$p"
  done
}

# --- preflight: the binary, free space, the remote ---------------------------------------
[[ -x $BIN ]] || die "no binary at $BIN"
if [[ -f $SUMS ]]; then
  want=$(awk '$2 ~ /prunedisk-xfer$/ {print $1}' "$SUMS")
  have=$(shasum -a 256 "$BIN" | cut -d' ' -f1)
  [[ -n $want && $want == "$have" ]] || die "sha256 of $BIN does not match $SUMS"
else
  die "no $SUMS beside the binary (the CI artifact carries one)"
fi
free=$(( $(df -k "$T" | awk 'NR==2 {print $4}') * 1024 ))
(( free >= NEED_FREE )) || die "only $((free >> 20)) MiB free under $T; need $((NEED_FREE >> 20))"
mkdir -p "$OUT" "$DATA"
TSV=$OUT/results.tsv
MEM=$OUT/mem.tsv
CHK=$OUT/checks.txt
CACHE=$HOME/Library/Caches/rclone
{
  echo "run=$RUN date=$(date -u +%FT%TZ) host=$(hostname -s) cpu=$(sysctl -n machdep.cpu.brand_string)"
  echo "mem=$(($(sysctl -n hw.memsize) >> 20)) MiB free_before=$((free >> 20)) MiB under $T"
  echo "xfer=$("$BIN" --version) sha256=$have"
  echo "rclone=$("$RCLONE" version | head -1) zstd=$(zstd --version 2>&1 | head -1)"
  echo "profile=${PROFILE[*]} size=$SIZE reps=$REPS seed=$SEED base=$BASE fixture=$FIXTURE"
  echo "swap=$(sysctl -n vm.swapusage)"
} >>"$OUT/env.txt"
[[ -f $MEM ]] || printf 'cell\trep\trc\twall_s\tpeak_tree_mib\tpeak_engine_mib\tpeak_rcd_mib\tdf_drop_mib\tdata_peak_mib\tcache_peak_mib\tswap_growth_mib\n' >"$MEM"

meas() { # meas CELL REP -- CMD...: measure.py with the Mac watches, then a mem.tsv row
  local cell=$1 rep=$2 c=$OUT/cell-$1-$2
  shift 3
  mkdir -p "$c"
  local w=(--df "$T" --watch "$T/xfer-mac")
  [[ -d $CACHE ]] && w+=(--watch "$CACHE")
  python3 "$HERE/measure.py" "$c/measure.json" "${w[@]}" -- "$@" || true
  python3 - "$c/measure.json" "$cell" "$rep" "$T/xfer-mac" "$CACHE" >>"$MEM" <<'PY'
import json, sys
m = json.load(open(sys.argv[1])); cell, rep, data, cache = sys.argv[2:6]
mib = lambda b: "" if b is None else f"{b / 1048576:.1f}"
dp = m.get("disk_peak_kib", {})
print("\t".join([cell, rep, str(m["rc"]), str(m["wall_s"]), mib(m["peak_rss"]),
    mib(m["peak_rss_engine"]), mib(m["peak_rss_children"]),
    mib(sum(m.get("df_drop_bytes", {}).values())), mib(dp.get(data, 0) * 1024),
    mib(dp.get(cache, 0) * 1024) if cache in dp else "", mib(m.get("swap_growth_bytes"))]))
PY
}

probe() { # a single-stream upload of PROBE_MIB incompressible MiB: the uplink at that moment
  local r=$1 when=$2 c=$OUT/cell-probe-$1-$2 p=$BASE/$RUN/probe/$1-$2
  guard "$p"
  mkdir -p "$c"
  [[ -f $DATA/probe.bin ]] || head -c $((PROBE_MIB << 20)) /dev/urandom >"$DATA/probe.bin"
  python3 "$HERE/measure.py" "$c/measure.json" -- "$RCLONE" copyto "$DATA/probe.bin" \
    "$p/probe.bin" --transfers 1 --use-json-log -v --stats 1000h 2>"$c/rclone.log" || true
  python3 "$HERE/row.py" "$TSV" "$RUN" uplink "probe-$when" "$r" "$c/measure.json" rclone \
    "$c/rclone.log" - "single-stream upload of ${PROBE_MIB} MiB; MB/s = bytes_sent / wall_s"
  purge "$p"
}

SRC=$DATA/src
"$BIN" gen --kind mac --size "$SIZE" --seed "$SEED" --dir "$SRC" >"$OUT/gen.txt"
du -sk "$SRC" | sed 's/^/src KiB: /' >>"$OUT/env.txt"
find "$SRC" -type f | wc -l | sed 's/^/src files: /' >>"$OUT/env.txt"

cell() { # cell TOOL REP
  local t=$1 r=$2 p=$BASE/$RUN/mac/$1/$2 c=$OUT/cell-$1-$2
  guard "$p"
  purge "$p"
  mkdir -p "$c"
  case $t in
    xfer)
      meas xfer "$r" -- "$BIN" backup --repo "$p" --rclone "$RCLONE" --source "$SRC" \
        --state "$c/state" --json "$c/report.json" --receipts "$c/state/receipts.jsonl" \
        "${PROFILE[@]}"
      python3 "$HERE/row.py" "$TSV" "$RUN" mac xfer "$r" "$c/measure.json" xfer \
        "$c/report.json" - "Mac profile"
      ;;
    b|bplus)
      local n=4
      [[ $t == bplus ]] && n=16
      meas "$t" "$r" -- "$RCLONE" copy "$SRC" "$p" --transfers "$n" --use-json-log -v \
        --stats 1000h 2>"$c/rclone.log"
      python3 "$HERE/row.py" "$TSV" "$RUN" mac "$t" "$r" "$c/measure.json" rclone \
        "$c/rclone.log" - "requests=files transferred"
      ;;
    a)
      local arch=$DATA/a.tar.zst
      wipe "$arch"
      meas a "$r" -- sh -c 'tar -C "$1" -cf - . | zstd -3 -T0 -q -o "$2" &&
        "$3" copy "$2" "$4" --use-json-log -v --stats 1000h' sh "$SRC" "$arch" "$RCLONE" "$p" \
        2>"$c/rclone.log"
      wipe "$arch"
      python3 "$HERE/row.py" "$TSV" "$RUN" mac a "$r" "$c/measure.json" rclone \
        "$c/rclone.log" - "tar+zstd staged in TMPDIR; requests=files transferred"
      ;;
    *) die "tool $t" ;;
  esac
  "$RCLONE" size --json "$p" >"$c/stored.json" 2>/dev/null || echo '{}' >"$c/stored.json"
}

restore_check() { # restore_check CELL REPO SRC: restore, then compare by full manifest
  local cell=$1 p=$2 s=$3 tgt=$DATA/restore c=$OUT/cell-$1-1 verdict
  wipe "$tgt"
  meas "$cell" 1 -- "$BIN" restore --repo "$p" --rclone "$RCLONE" --target "$tgt" \
    --json "$OUT/cell-$cell-1/restore.json" "${PROFILE[@]}"
  if python3 "$HERE/manifest.py" "$s" "$tgt" >"$c/manifest.txt" 2>&1; then verdict=match; else verdict=MISMATCH; fi
  echo "$cell: manifest $verdict ($(head -1 "$c/manifest.txt"))" >>"$CHK"
  wipe "$tgt"
}

TOOLS=(xfer a b bplus)
for r in $(seq 1 "$REPS"); do
  probe "$r" before
  for i in 0 1 2 3; do
    t=${TOOLS[$(((i + r - 1) % 4))]} # rotate the order each rep
    echo "== $t rep $r $(date -u +%T)"
    cell "$t" "$r"
    [[ $t == xfer && $r == 1 ]] || purge "$BASE/$RUN/mac/$t/$r"
  done
  probe "$r" after
done

# The load phase and a restore, on rep 1's repository: an unchanged rerun loads the whole
# chunk map and uploads nothing.
P1=$BASE/$RUN/mac/xfer/1
guard "$P1"
echo "== load and restore $(date -u +%T)"
meas xfer-rerun 1 -- "$BIN" backup --repo "$P1" --rclone "$RCLONE" --source "$SRC" \
  --state "$OUT/cell-xfer-1/state" --json "$OUT/cell-xfer-rerun-1.json" \
  --receipts "$OUT/cell-xfer-1/state/receipts.jsonl" "${PROFILE[@]}"
restore_check xfer-restore "$P1" "$SRC"

if [[ $FIXTURE == 1 ]]; then
  fixture() { # fixture CHUNKS SEED: generate, copy into P1, remove the local copy
    local f=$DATA/fixture
    wipe "$f"
    "$BIN" gen-index --dir "$f" --chunks "$1" --per-pack 16 --hash sha1 --seed "$2" \
      >>"$OUT/fixture.txt"
    du -sk "$f" | sed 's/^/fixture KiB: /' >>"$OUT/fixture.txt"
    local t0=$SECONDS
    "$RCLONE" copy "$f" "$P1" --transfers 32 --checkers 32 >/dev/null 2>"$OUT/fixture-copy.log"
    echo "copied in $((SECONDS - t0)) s" >>"$OUT/fixture.txt"
    wipe "$f"
  }
  echo "== fixture $FIX1 chunks $(date -u +%T)"
  fixture "$FIX1" 11
  meas fixture-rerun 1 -- "$BIN" backup --repo "$P1" --rclone "$RCLONE" --source "$SRC" \
    --state "$OUT/cell-xfer-1/state" --json "$OUT/cell-fixture-rerun-1.json" \
    --receipts "$OUT/cell-xfer-1/state/receipts.jsonl" "${PROFILE[@]}"
  restore_check fixture-restore "$P1" "$SRC"
  echo "== fixture to $FIX2 chunks $(date -u +%T)"
  fixture $((FIX2 - FIX1)) 12
  restore_check fixture-restore-full "$P1" "$SRC"
fi

# --- verdicts -------------------------------------------------------------------------------
python3 - "$MEM" "$SHARE_ALLOW" "$RSS_MAX" "$DISK_MAX" >>"$CHK" <<'PY'
import csv, sys
rows = list(csv.DictReader(open(sys.argv[1]), delimiter="\t"))
allow, rss_max, disk_max = (int(x) / 1048576 for x in sys.argv[2:5])
get = lambda c: next((r for r in rows if r["cell"] == c and r["rep"] == "1"), None)
for r in rows:
    if r["cell"].startswith(("xfer", "fixture")) and r["peak_tree_mib"]:
        ok = float(r["peak_tree_mib"]) <= rss_max
        print(f"{r['cell']} rep {r['rep']}: engine {r['peak_engine_mib']} MiB, rcd "
              f"{r['peak_rcd_mib']} MiB, both {r['peak_tree_mib']} MiB <= {rss_max:.0f}: "
              f"{'PASS' if ok else 'FAIL'}")
peak_disk = max((float(r["data_peak_mib"] or 0) for r in rows), default=0)
print(f"disk under TMPDIR/xfer-mac peak {peak_disk:.0f} MiB <= {disk_max:.0f}: "
      f"{'PASS' if peak_disk <= disk_max else 'FAIL'}")
for big, small in (("fixture-restore-full", "xfer-restore"), ("fixture-rerun", "xfer-rerun")):
    b, s = get(big), get(small)
    if b and s:
        grew = float(b["peak_engine_mib"]) - float(s["peak_engine_mib"])
        print(f"{big} engine grew {grew:.1f} MiB over {small} <= {allow:.0f} (share plus one "
              f"fetch batch): {'PASS' if grew <= allow else 'FAIL'}")
PY

python3 - "$TSV" >>"$CHK" <<'PY'
import csv, statistics, sys
rows = [r for r in csv.DictReader(open(sys.argv[1]), delimiter="\t") if r["workload"] == "mac"]
med = lambda t, k: statistics.median(float(r[k]) for r in rows if r["tool"] == t and r[k])
try:
    best = min(("a", "b", "bplus"), key=lambda t: med(t, "wall_s"))
    x, bw = med("xfer", "wall_s"), med(best, "wall_s")
    print(f"AC-8 wall: xfer median {x:.1f} s <= 0.8 x best baseline {best} median {bw:.1f} s: "
          f"{'PASS' if x <= 0.8 * bw else 'FAIL'}")
    xs, bs = med("xfer", "bytes_sent"), med("b", "bytes_sent")
    print(f"AC-8 bytes: xfer median {xs:.0f} <= 0.65 x (b) median {bs:.0f}: "
          f"{'PASS' if xs <= 0.65 * bs else 'FAIL'}")
except (ValueError, statistics.StatisticsError) as e:
    print(f"AC-8 wall/bytes: not computable ({e})")
PY

case "$BASE/$RUN" in *disk-cleaner-archive*|*..*) die "refusing final purge" ;; esac
"$RCLONE" purge "$BASE/$RUN" >/dev/null 2>&1 || true
{
  echo "purged $BASE/$RUN at $(date -u +%FT%TZ)"
  echo "remaining objects under $BASE/$RUN: $("$RCLONE" lsf -R "$BASE/$RUN" 2>/dev/null | wc -l | tr -d ' ')"
  echo "entries named $RUN/ in a listing of $BASE/: $("$RCLONE" lsf "$BASE/" 2>/dev/null | grep -c "^$RUN/" || true)"
} >>"$OUT/purge.txt"
cat "$OUT/purge.txt" "$CHK"
wipe "$DATA"
echo "results: $OUT"
