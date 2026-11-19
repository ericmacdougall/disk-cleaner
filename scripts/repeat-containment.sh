#!/usr/bin/env bash
# Run the desktop worker containment test N times (default 10) from apps/desktop/src-tauri,
# report how many runs failed, and fail if any did. A flaky containment test is a leak that
# happens sometimes, so the threshold is zero failures.
set -u
runs="${1:-10}"
failed=0
for i in $(seq 1 "$runs"); do
  if cargo test --locked --test containment -- --nocapture > "containment-run-$i.log" 2>&1; then
    echo "run $i: ok"
  else
    failed=$((failed + 1))
    echo "run $i: FAILED"
    grep -E "panicked|still running|never started|sweep found|outlived" "containment-run-$i.log" || true
  fi
done
echo "containment: $failed of $runs runs failed"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  echo "containment ($RUNNER_OS): $failed of $runs runs failed" >> "$GITHUB_STEP_SUMMARY"
fi
[ "$failed" -eq 0 ]
