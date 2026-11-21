#!/usr/bin/env python3
"""Broken variants for the window-close check (desktop-close.yml): each one removes one part
of the shutdown path in a CI checkout, so the check must fail on exactly the routes that part
covers. Each edit is an exact text replacement that must match once; a variant whose text no
longer matches fails here instead of silently testing the unbroken app.

usage: variant.py NAME   (from apps/desktop; never run in a developer checkout)

  worker-ignores-eof   the worker's watchdog no longer ends it when its input closes (the
                       parent-death backstop): every route where the app dies without running
                       its shutdown must fail during the analysis
  exit-skips-shutdown  the run loop's Exit no longer runs end_app (Inner::shutdown): every
                       route that ends through Exit must fail
"""

from __future__ import annotations

import sys
from pathlib import Path

SRC = Path("src-tauri/src")

VARIANTS = {
    "worker-ignores-eof": [(
        SRC / "worker.rs",
        "            Ok(0) | Err(_) => break,\n",
        "            Ok(0) | Err(_) => loop {\n                std::thread::park();\n            },\n",
    )],
    "exit-skips-shutdown": [(
        SRC / "lib.rs",
        "        if let tauri::RunEvent::Exit = event {\n            end_app(&on_exit);\n",
        "        if let tauri::RunEvent::Exit = event {\n            let _ = &on_exit;\n",
    )],
}


def main() -> int:
    if len(sys.argv) != 2 or sys.argv[1] not in VARIANTS:
        print(__doc__)
        return 2
    for path, old, new in VARIANTS[sys.argv[1]]:
        text = path.read_text()
        if text.count(old) != 1:
            print(f"variant {sys.argv[1]}: the text to replace is not in {path} exactly once")
            return 1
        path.write_text(text.replace(old, new))
        print(f"variant {sys.argv[1]}: edited {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
