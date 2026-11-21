#!/usr/bin/env python3
"""A broken variant must be caught by the check written for it (PR gate F1).

usage: expect_variant.py VARIANT OUT

OUT is the folder e2e/check.sh wrote. Exit 0 when the named check failed (the variant was
caught), 1 when it passed (the check is vacuous), 2 on bad usage or missing results.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

# variant -> (suite check ids that must fail, text the manifest report must contain)
EXPECT = {
    "planted-write": ([], "UNDECLARED added: "),
    "offered-check-removed": (["ipc.refused.root_not_offered", "ipc.refused.subfolder_of_offered_root"], None),
    "capability-broadened": (["ipc.refused.not_in_capabilities.path"], None),
    "focus-broken": (["settings.save_confirm_keep_escape"], None),
    # PR gate F8: the question opened without showModal, inert or the Tab trap.
    "modal-trap-removed": (["settings.confirm_focus_contained"], None),
    # PR gate F9: the size tree renders up to 1000 rows, not at most 200 (spec AC-9).
    "row-cap-removed": (["timing.synth_1m.scroll_p95"], None),
    # PR gate F10: a write outside every snapshot root; only the trace can see it.
    "planted-outside": ([], None),
}
# variant -> the line the trace report (trace.txt) must hold
TRACE = {
    "planted-outside": "OTHER openat: /tmp/prunedisk-planted-outside",
}


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[0] not in EXPECT:
        print(__doc__, file=sys.stderr)
        return 2
    variant, out = argv[0], Path(argv[1])
    checks, manifest_text = EXPECT[variant]
    try:
        result = json.loads((out / "e2e.json").read_text())
    except (OSError, ValueError) as e:
        print(f"no suite results: {e}", file=sys.stderr)
        return 2
    failed = set(result["failed"])
    caught = True
    for c in checks:
        print(f"{'caught' if c in failed else 'MISSED'}: {c}")
        caught &= c in failed
    if manifest_text is not None:
        report = (out / "manifest.txt").read_text() if (out / "manifest.txt").exists() else ""
        hit = [line for line in report.splitlines() if line.startswith(manifest_text) and ".planted-by-variant" in line]
        print(f"{'caught' if hit else 'MISSED'}: manifest {hit[:1]}")
        caught &= bool(hit)
    if variant in TRACE:
        trace = (out / "trace.txt").read_text() if (out / "trace.txt").exists() else ""
        hit = TRACE[variant] in trace.splitlines()
        print(f"{'caught' if hit else 'MISSED'}: trace {TRACE[variant]!r}")
        caught &= hit
        # The snapshot roots do not hold it, so the manifest must not be what caught it.
        report = (out / "manifest.txt").read_text() if (out / "manifest.txt").exists() else ""
        if "prunedisk-planted-outside" in report:
            print("unexpected: the manifest saw the write outside its roots")
            caught = False
    others = sorted(failed - set(checks))
    print(f"other failed checks: {others}")
    print(f"variant {variant}: {'caught' if caught else 'NOT caught'}")
    return 0 if caught else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
