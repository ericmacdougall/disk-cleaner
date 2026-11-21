#!/usr/bin/env bash
# Read-only guard (spec AC-6), second layer: see scripts/check_readonly.py.
#   scripts/check-readonly.sh            check this checkout
#   scripts/check-readonly.sh --controls plant one violation per category in a copy and
#                                        require each to fail (add --clippy for the clippy layer)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
if [ "${1:-}" = "--controls" ]; then
  shift
  exec python3 "$here/readonly_controls.py" "$root" "$@"
fi
exec python3 "$here/check_readonly.py" "$root"
