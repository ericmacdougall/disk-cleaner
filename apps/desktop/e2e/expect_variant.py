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
    # PR gate F9: the size tree renders up to 1000 rows, not at most 200 (spec AC-9). Both 1M
    # launches enforce the limit: the untraced timed one and the traced one (Linux, where the
    # variants run).
    "row-cap-removed": (["timing.synth_1m.scroll_p95", "steps.synth_1m.traced_scroll"], None),
    # PR gate F10: a write outside every snapshot root; only the trace can see it.
    "planted-outside": ([], None),
    # Every timing is taken untraced (main's ruling): with the untraced switch never honoured,
    # every launch is traced, and each timing check must refuse to time it (see DETAIL).
    "untraced-ignored": (
        ["timing.ipc_tree_page_p95", "timing.scroll_p95", "timing.synth_1m.ipc_tree_page_p95", "timing.synth_1m.scroll_p95"],
        None,
    ),
    # The default-fixture scans (first run, untraced timing, main) must read the same conditions:
    # here the first-run and untraced scans run with git facts off.
    "scan-settings-differ": (["flow.default_scan_settings_match"], None),
    # A second app instance (the shipped binary's name) alive at the timed sessions' start. The
    # one-million-row checks are untraced under either wrapper, so they reach the other-instance
    # precondition; the default ones may stop at the trace state first.
    "other-instance-alive": (["timing.synth_1m.ipc_tree_page_p95", "timing.synth_1m.scroll_p95"], None),
}
# variant -> the texts each of its failed checks must report (all of them): caught by the
# reason the variant breaks, not by something incidental
DETAIL = {
    "untraced-ignored": ["not shown to be untraced; not measured"],
    "scan-settings-differ": ["scanned under other settings than the main session"],
    "other-instance-alive": ["another app instance is alive at the session start", "prunedisk-deskt"],
}
# A check that failed on one of its preconditions reports this; it was not measured, so for a
# variant without a DETAIL it is not a catch.
NOT_MEASURED = "not measured"
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
    details = {r.get("id"): str(r.get("detail", "")) for r in result.get("results", []) if not r.get("ok", True)}
    caught = True
    for c in checks:
        print(f"{'caught' if c in failed else 'MISSED'}: {c}")
        caught &= c in failed
        if c not in failed:
            continue
        detail = details.get(c, "")
        if variant in DETAIL:
            for text in DETAIL[variant]:
                hit = text in detail
                print(f"{'caught' if hit else 'MISSED'}: {c} reports {text!r}: {detail[:200]!r}")
                caught &= hit
        elif NOT_MEASURED in detail:
            print(f"MISSED: {c} failed on a precondition, not on this variant: {detail[:200]!r}")
            caught = False
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
