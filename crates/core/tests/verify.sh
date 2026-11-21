#!/bin/sh
# Run the tests of one disk-cleaner-core test target by name filter, and fail when a filter
# matches no test (GoodQA goodqa#16529, AC-8 and NEG-28). Every `Verify by` of the rescue spec
# that names a cargo test filter goes through this script, so a misspelled or renamed test can
# never pass by matching nothing.
#
# Usage: sh crates/core/tests/verify.sh TARGET [FILTER...]
#   TARGET  a test target of disk-cleaner-core (rescue, guard, source_rules, ...) or `lib` for the
#           library's unit tests.
#   FILTER  a libtest name filter: a test runs when its full name contains FILTER.
#
# It lists the target's tests (`cargo test ... -- --list`, which lists ignored tests too). It
# exits 1 when a FILTER matches no listed test, or, with no FILTER, when the target has no test.
# Otherwise it runs `cargo test --locked -p disk-cleaner-core (--test TARGET | --lib) --
# --include-ignored --nocapture FILTER...` and exits with cargo's own status. Run it from the
# workspace root. CARGO_TARGET_DIR and other cargo settings come from the environment.

set -u

usage() {
    echo "usage: sh crates/core/tests/verify.sh TARGET [FILTER...]" >&2
    exit 2
}

[ $# -ge 1 ] || usage
target=$1
shift
case "$target" in
    "" | -* | *[!A-Za-z0-9_-]*)
        echo "verify.sh: '$target' is not a test target name (or lib)" >&2
        exit 2
        ;;
esac
for f in "$@"; do
    case "$f" in
        "" | -*)
            echo "verify.sh: '$f' is not a test name filter (empty or an option)" >&2
            exit 2
            ;;
    esac
done

# One cargo invocation for the chosen target; its arguments follow.
cargo_target() {
    if [ "$target" = lib ]; then
        cargo test --locked -p disk-cleaner-core --lib "$@"
    else
        cargo test --locked -p disk-cleaner-core --test "$target" "$@"
    fi
}

work=$(mktemp -d "${TMPDIR:-/tmp}/verify.XXXXXX") || exit 2
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM

# The listing: stdout holds `NAME: test` lines (and benchmarks, and a summary line).
cargo_target -- --list >"$work/list" 2>"$work/list.err"
status=$?
if [ "$status" -ne 0 ]; then
    cat "$work/list.err" >&2
    echo "verify.sh: listing the tests of $target failed (exit $status)" >&2
    exit 1
fi
sed -n 's/: test$//p' "$work/list" >"$work/names"
count=$(wc -l <"$work/names" | tr -d ' ')
echo "verify.sh: $target lists $count tests"

if [ $# -eq 0 ]; then
    if [ "$count" -eq 0 ]; then
        echo "verify.sh: $target has no tests" >&2
        exit 1
    fi
else
    missing=0
    for f in "$@"; do
        # libtest runs a test when its name contains the filter; test names hold no newline.
        n=$(grep -F -c -- "$f" "$work/names")
        if [ "$n" -eq 0 ]; then
            echo "verify.sh: filter '$f' matches no test of $target" >&2
            missing=1
        else
            echo "verify.sh: filter '$f' matches $n tests of $target"
        fi
    done
    [ "$missing" -eq 0 ] || exit 1
fi

cargo_target -- --include-ignored --nocapture "$@"
status=$?
echo "verify.sh: $target exit $status"
exit "$status"
