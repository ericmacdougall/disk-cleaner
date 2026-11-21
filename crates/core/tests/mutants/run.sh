#!/bin/sh
# Mutation check of the rescue's tests (GoodQA goodqa#16529, AC-8): each committed mutant breaks
# one rule of the spec's Mutants table, and its named test must fail on its named assertion.
#
# Usage: sh crates/core/tests/mutants/run.sh [NAME...]
#   NAME  a mutant: crates/core/tests/mutants/NAME.mutant. With no NAME, every mutant runs.
#
# Run it from the workspace root (/src in the dc-rust:lanes container, never on the Mac). A
# mutant file is a header, then a unified diff of exactly one file under crates/:
#
#   rule: the rule the mutant breaks (a row of the spec's Mutants table)
#   mutant: what the edit does
#   note: free text (optional, may repeat)
#   killing-test: TARGET FILTER   a test that must fail (TARGET and FILTER as verify.sh takes them)
#   assertion: SUBSTRING          text that test's failing output must contain
#
# Each killing-test line is followed by its assertion line. A mutant may name several pairs; it
# is KILLED only when every pair kills. For each mutant, on a fresh copy of the tree (no target
# and no .git, every mtime the copy's own, so cargo rebuilds what an earlier mutant changed):
#   1. every hunk's original lines must occur exactly once in the file, and the patch must apply
#      exactly (`patch -p1 -N -F0`, a dry run first); otherwise the mutant is an ERROR;
#   2. each killing target is built (`cargo test --no-run`); a build failure is an ERROR, never a
#      kill, and so is assertion text that already appears in the build output;
#   3. each killing test runs through verify.sh, so a filter that matches no test is an ERROR.
#      It passes: SURVIVED. It fails and its output holds the assertion: KILLED. It fails on
#      "Too many open files", times out, fails without the assertion, or exits other than as a
#      test failure: ERROR.
# Before the mutants every killing test runs on the unmutated copy: it must pass, without its
# assertion text in the output. After them it runs again, so a stale build cannot hide. The run
# prints a table and exits 0 only when every mutant is KILLED and both baselines pass.
#
# Environment:
#   MUTANTS_DIR          where the NAME.mutant files are, relative to the workspace root (default
#                        crates/core/tests/mutants). crates/core/tests/mutants/selfcheck holds the
#                        runner's own checks: each rule line says the outcome it must get, and
#                        that run exits 1 by design.
#   MUTANTS_WORK         the copy, the source snapshot and the logs (default ${TMPDIR:-/tmp}/dc-mutants)
#   MUTANTS_TARGET_DIR   the cargo target dir, reused across mutants and separate from the main
#                        build (default $MUTANTS_WORK/target); CARGO_TARGET_DIR is overridden
#   MUTANT_TIMEOUT       seconds per test run (default 900)
#   MUTANTS_MIN_FREE_KB  free space needed under MUTANTS_WORK before the run (default 4 GiB)
# Exit: 0 all KILLED, 1 otherwise, 2 for a usage or setup error.

set -u

src=$(pwd -P)
if [ ! -f "$src/Cargo.toml" ] || [ ! -f "$src/crates/core/tests/verify.sh" ]; then
    echo "run.sh: run it from the workspace root" >&2
    exit 2
fi
rel=${MUTANTS_DIR:-crates/core/tests/mutants}
case "/$rel/" in
    //* | */../*)
        echo "run.sh: MUTANTS_DIR is a path below the workspace root, without .." >&2
        exit 2
        ;;
esac
dir=$src/$rel
if [ ! -d "$dir" ]; then
    echo "run.sh: no directory $rel" >&2
    exit 2
fi
work=${MUTANTS_WORK:-${TMPDIR:-/tmp}/dc-mutants}
case "$work" in
    /*) ;;
    *)
        echo "run.sh: MUTANTS_WORK must be an absolute path" >&2
        exit 2
        ;;
esac
case "$work/" in
    "$src"/*)
        echo "run.sh: MUTANTS_WORK must lie outside the source tree" >&2
        exit 2
        ;;
esac
limit=${MUTANT_TIMEOUT:-900}
floor=${MUTANTS_MIN_FREE_KB:-4194304}
for v in "$limit" "$floor"; do
    case "$v" in
        "" | *[!0-9]*)
            echo "run.sh: MUTANT_TIMEOUT and MUTANTS_MIN_FREE_KB are whole numbers" >&2
            exit 2
            ;;
    esac
done
for tool in cargo patch tar awk; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "run.sh: $tool is not installed" >&2
        exit 2
    }
done
if command -v timeout >/dev/null 2>&1; then
    with_limit() { timeout "$limit" "$@"; }
else
    echo "run.sh: timeout is not installed; test runs are unbounded"
    with_limit() { "$@"; }
fi

# The mutants to run, by name.
if [ $# -eq 0 ]; then
    for f in "$dir"/*.mutant; do
        [ -f "$f" ] || continue
        n=${f##*/}
        set -- "$@" "${n%.mutant}"
    done
    if [ $# -eq 0 ]; then
        echo "run.sh: no mutants in $dir" >&2
        exit 1
    fi
fi
for n in "$@"; do
    case "$n" in
        "" | -* | *[!A-Za-z0-9_-]*)
            echo "run.sh: '$n' is not a mutant name" >&2
            exit 2
            ;;
    esac
    if [ ! -f "$dir/$n.mutant" ]; then
        echo "run.sh: no mutant $n ($dir/$n.mutant)" >&2
        exit 2
    fi
done

mkdir -p "$work" || exit 2
tree=$work/tree
mark=.dc-mutants-copy
if [ -e "$tree" ] && [ ! -f "$tree/$mark" ]; then
    echo "run.sh: $tree exists and is not a copy this script made; remove it or set MUTANTS_WORK" >&2
    exit 2
fi
avail=$(df -Pk "$work" | awk 'NR == 2 { print $4 }')
if [ "${avail:-0}" -lt "$floor" ]; then
    echo "run.sh: ${avail:-unknown} KiB free under $work, below the floor of $floor KiB" >&2
    exit 2
fi
CARGO_TARGET_DIR=${MUTANTS_TARGET_DIR:-$work/target}
export CARGO_TARGET_DIR
logs=$work/logs-$(date -u +%Y%m%dT%H%M%SZ)-$$
mkdir -p "$logs" || exit 2
echo "run.sh: logs in $logs; CARGO_TARGET_DIR=$CARGO_TARGET_DIR; ${avail} KiB free; timeout ${limit}s"

# One snapshot of the sources for the whole run, so every mutant and both baselines see the same
# tree even if the checkout changes meanwhile.
snap=$work/source.tar
if ! tar -C "$src" --exclude=./target --exclude=./.git -cf "$snap" .; then
    echo "run.sh: cannot snapshot $src" >&2
    exit 2
fi

# A fresh copy of the snapshot at $tree. `tar -m` gives every file the extraction time, which is
# newer than any earlier build, so cargo rebuilds a file an earlier mutant changed.
fresh_copy() {
    if [ -e "$tree" ]; then
        [ -f "$tree/$mark" ] || return 1
        rm -rf "$tree" || return 1
    fi
    mkdir -p "$tree" && tar -C "$tree" -xmf "$snap" && : >"$tree/$mark"
}

# Parse mutant $1 into $2.pairs (TARGET<tab>FILTER<tab>ASSERTION), $2.diff, $2.rule and $2.path.
# Prints what is wrong and fails when the file is malformed.
parse() {
    awk -v out="$2" '
        BEGIN { head = 1; pend = ""; bad = ""; rule = ""; pairs = 0; files = 0 }
        head && /^--- / { head = 0 }
        head {
            if ($0 ~ /^#/ || $0 ~ /^[ \t]*$/) next
            k = index($0, ": ")
            if (k == 0) { bad = bad "a header line is not \"key: value\": " $0 "\n"; next }
            key = substr($0, 1, k - 1)
            val = substr($0, k + 2)
            if (key == "rule") {
                if (rule != "") bad = bad "two rule headers\n"
                rule = val
            } else if (key == "mutant" || key == "note") {
            } else if (key == "killing-test") {
                if (pend != "") bad = bad "killing-test without an assertion: " pendv "\n"
                n = split(val, w, " ")
                if (n != 2 || val != w[1] " " w[2] || w[1] !~ /^[A-Za-z0-9_][A-Za-z0-9_-]*$/ || w[2] ~ /^-/)
                    bad = bad "killing-test is not \"TARGET FILTER\": " val "\n"
                pend = w[1] "\t" w[2]
                pendv = val
            } else if (key == "assertion") {
                if (pend == "") bad = bad "assertion without a killing-test: " val "\n"
                else if (val ~ /^[ \t]*$/ || index(val, "\t") > 0) bad = bad "an assertion is empty or holds a tab\n"
                else { print pend "\t" val > (out ".pairs"); pairs++ }
                pend = ""
            } else bad = bad "unknown header: " key "\n"
            next
        }
        {
            print > (out ".diff")
            if ($0 ~ /^\+\+\+ /) { files++; p = substr($0, 5); sub(/\t.*$/, "", p); print p > (out ".path") }
        }
        END {
            if (rule != "") print rule > (out ".rule")
            if (pend != "") bad = bad "killing-test without an assertion: " pendv "\n"
            if (rule == "") bad = bad "no rule header\n"
            if (pairs == 0) bad = bad "no killing-test with an assertion\n"
            if (head) bad = bad "no diff\n"
            else if (files != 1) bad = bad "the diff must change exactly one file\n"
            if (bad != "") { printf "%s", bad; exit 1 }
        }' "$1" || return 1
    p=$(cat "$2.path")
    case "$p" in
        b/crates/*) ;;
        *)
            echo "the diff's file is not b/crates/...: $p"
            return 1
            ;;
    esac
    case "/$p/" in
        */../* | */./* | *//*)
            echo "the diff's path has a . or .. component: $p"
            return 1
            ;;
    esac
    if ! grep -q -x -F -- "--- a/${p#b/}" "$2.diff"; then
        echo "the diff's --- line does not name a/${p#b/}"
        return 1
    fi
    printf '%s\n' "${p#b/}" >"$2.path"
}

# Every hunk of diff $1 must find its original lines exactly once in file $2.
hunks_unique() {
    awk -v target="$2" '
        function flush(   i, j, c, ok) {
            if (!inh) return
            inh = 0
            hunks++
            c = 0
            for (i = 1; i + k - 1 <= n; i++) {
                ok = 1
                for (j = 1; j <= k; j++) if (T[i + j - 1] != B[j]) { ok = 0; break }
                if (ok) c++
            }
            if (k == 0 || c != 1) {
                printf "hunk %d: its %d original lines occur %d times in %s\n", hunks, k, c, target
                bad = 1
            }
        }
        BEGIN {
            n = 0
            while ((r = (getline l < target)) > 0) T[++n] = l
            if (r < 0) { printf "cannot read %s\n", target; bad = 1 }
        }
        /^@@ / { flush(); inh = 1; k = 0; next }
        inh && /^[ -]/ { B[++k] = substr($0, 2); next }
        inh && $0 == "" { B[++k] = ""; next }
        inh && /^[+\\]/ { next }
        { flush() }
        END {
            flush()
            if (hunks == 0) { print "the diff has no hunk"; bad = 1 }
            exit bad
        }
    ' "$1"
}

# cargo test of one target, from the copy.
cargo_build() {
    if [ "$1" = lib ]; then
        (cd "$tree" && cargo test --locked -p disk-cleaner-core --lib --no-run) </dev/null
    else
        (cd "$tree" && cargo test --locked -p disk-cleaner-core --test "$1" --no-run) </dev/null
    fi
}

# Run one killing test from the copy; the exit status is verify.sh's (cargo's), 124 on timeout.
run_test() {
    (cd "$tree" && with_limit sh crates/core/tests/verify.sh "$1" "$2") </dev/null
}

# Add to the current mutant's detail.
note() { detail="${detail:+$detail; }$1"; }

# Is the run in log $2 (exit status $1) a kill on assertion $3? Prints OUTCOME, then why.
classify() {
    if [ "$1" -eq 0 ]; then
        echo "SURVIVED the test passed"
    elif grep -q -F "Too many open files" "$2"; then
        echo "ERROR it hit the descriptor limit (Too many open files)"
    elif [ "$1" -eq 124 ]; then
        echo "ERROR it timed out after ${limit}s"
    elif [ "$1" -ne 101 ]; then
        echo "ERROR exit $1, not a test failure (see verify.sh's message)"
    elif grep -q -F -- "$3" "$2"; then
        echo "KILLED $(grep -F -m 1 -- "$3" "$2" | tr '\t' ' ' | cut -c 1-160)"
    else
        echo "ERROR it failed without the assertion"
    fi
}

# Parse every mutant first: a malformed one is an ERROR before anything is built.
tab=$(printf '\t')
: >"$logs/results"
: >"$logs/baselines"
for n in "$@"; do
    if ! why=$(parse "$dir/$n.mutant" "$logs/$n"); then
        printf '%s\t%s\t%s\n' "$n" ERROR "malformed: $(echo "$why" | tr '\n' ' ')" >>"$logs/results"
        continue
    fi
    while IFS="$tab" read -r t f a; do
        if ! awk -F "$tab" -v t="$t" -v f="$f" '$2 == t && $3 == f { found = 1 } END { exit !found }' \
            "$logs/baselines"; then
            k=$(($(wc -l <"$logs/baselines") + 1))
            printf '%s\t%s\t%s\n' "$k" "$t" "$f" >>"$logs/baselines"
        fi
    done <"$logs/$n.pairs"
done

baseline_of() {
    awk -F "$tab" -v t="$1" -v f="$2" '$2 == t && $3 == f { print $1 }' "$logs/baselines"
}

# The baseline: every killing test passes on the unmutated copy. Prints one line per test.
baseline() {
    phase=$1
    ok=0
    if ! fresh_copy; then
        echo "run.sh: cannot make the copy at $tree" >&2
        return 2
    fi
    echo "run.sh: $phase baseline, sources sha256 $(cd "$tree" && find crates -type f -name '*.rs' |
        LC_ALL=C sort | xargs sha256sum | sha256sum | cut -c 1-16)"
    while IFS="$tab" read -r k t f; do
        log=$logs/$phase.$k.log
        if ! cargo_build "$t" >"$log.build" 2>&1; then
            echo "  $t $f: the unmutated tree does not build ($log.build)"
            echo 1 >"$logs/$phase.$k.status"
            ok=1
            continue
        fi
        run_test "$t" "$f" >"$log" 2>&1
        st=$?
        echo "$st" >"$logs/$phase.$k.status"
        if [ "$st" -eq 0 ]; then
            echo "  $t $f: passes"
        else
            echo "  $t $f: exit $st on the unmutated tree ($log)"
            tail -n 30 "$log" | sed 's/^/    | /'
            ok=1
        fi
    done <"$logs/baselines"
    return "$ok"
}

start=$(date +%s)
baseline before
base_status=$?
[ "$base_status" -eq 2 ] && exit 2

for n in "$@"; do
    grep -q "^$n$tab" "$logs/results" && continue
    t0=$(date +%s)
    outcome=KILLED
    detail=""
    # Every pair's test must pass unmutated, without the assertion text in its output.
    while IFS="$tab" read -r t f a; do
        k=$(baseline_of "$t" "$f")
        if [ "$(cat "$logs/before.$k.status")" -ne 0 ]; then
            outcome=ERROR
            note "$t $f fails unmutated"
        elif grep -q -F -- "$a" "$logs/before.$k.log"; then
            outcome=ERROR
            note "$t $f prints the assertion unmutated"
        fi
    done <"$logs/$n.pairs"
    path=$(cat "$logs/$n.path")
    if [ "$outcome" = KILLED ] && ! fresh_copy; then
        echo "run.sh: cannot make the copy at $tree" >&2
        exit 2
    fi
    if [ "$outcome" = KILLED ]; then
        if [ ! -f "$tree/$path" ]; then
            outcome=ERROR
            note "no file $path"
        elif ! why=$(hunks_unique "$logs/$n.diff" "$tree/$path"); then
            outcome=ERROR
            note "$(echo "$why" | tr '\n' ' ')"
        elif ! patch -d "$tree" -p1 -N -F0 --no-backup-if-mismatch --dry-run <"$logs/$n.diff" \
            >"$logs/$n.patch" 2>&1; then
            outcome=ERROR
            note "the patch does not apply exactly: $(tail -n 1 "$logs/$n.patch") ($logs/$n.patch)"
        elif ! patch -d "$tree" -p1 -N -F0 --no-backup-if-mismatch <"$logs/$n.diff" \
            >>"$logs/$n.patch" 2>&1; then
            outcome=ERROR
            note "the patch failed after its dry run ($logs/$n.patch)"
        fi
    fi
    if [ "$outcome" = KILLED ]; then
        for t in $(cut -f 1 "$logs/$n.pairs" | sort -u); do
            if ! cargo_build "$t" >"$logs/$n.build.$t" 2>&1; then
                outcome=ERROR
                note "$t does not compile with the mutant ($logs/$n.build.$t)"
                sed -n '/^error/,/^$/p' "$logs/$n.build.$t" | head -n 20 | sed 's/^/    | /'
            fi
        done
    fi
    if [ "$outcome" = KILLED ]; then
        i=0
        while IFS="$tab" read -r t f a; do
            i=$((i + 1))
            if grep -q -F -- "$a" "$logs/$n.build.$t"; then
                outcome=ERROR
                note "$t $f: the assertion text is in the build output"
                continue
            fi
            log=$logs/$n.$i.log
            run_test "$t" "$f" >"$log" 2>&1
            st=$?
            res=$(classify "$st" "$log" "$a")
            case "$res" in
                KILLED*) ;;
                SURVIVED*) [ "$outcome" = ERROR ] || outcome=SURVIVED ;;
                *) outcome=ERROR ;;
            esac
            note "$t $f: $res"
            case "$res" in
                KILLED*) ;;
                *)
                    echo "run.sh: $n, $t $f: $res ($log), its last lines:"
                    tail -n 40 "$log" | sed 's/^/    | /'
                    ;;
            esac
        done <"$logs/$n.pairs"
    fi
    printf '%s\t%s\t%s (%ss)\n' "$n" "$outcome" "$detail" "$(($(date +%s) - t0))" >>"$logs/results"
    echo "run.sh: $n $outcome"
done

baseline after
after_status=$?
[ "$after_status" -eq 2 ] && exit 2

echo
echo "run.sh: $(wc -l <"$logs/results" | tr -d ' ') mutants in $(($(date +%s) - start))s"
printf '%-18s %-9s %s\n' MUTANT RESULT "RULE: KILLING TEST: OUTCOME"
status=0
while IFS="$tab" read -r n outcome detail; do
    rule=$(cat "$logs/$n.rule" 2>/dev/null)
    printf '%-18s %-9s %s: %s\n' "$n" "$outcome" "${rule:-?}" "$detail"
    [ "$outcome" = KILLED ] || status=1
done <"$logs/results"
if [ "$base_status" -ne 0 ] || [ "$after_status" -ne 0 ]; then
    echo "run.sh: a killing test failed on the unmutated tree (before: $base_status, after: $after_status)"
    status=1
fi
echo "run.sh: exit $status"
exit "$status"
