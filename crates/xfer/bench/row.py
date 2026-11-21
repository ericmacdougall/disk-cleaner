#!/usr/bin/env python3
"""Append one results.tsv row from a measure.py record plus the tool's own output.

usage: row.py TSV RUN WORKLOAD TOOL REP MEASURE.json KIND OUTPUT STORED.json [NOTE]
  KIND xfer:   OUTPUT is the engine's --json report (bytes sent and requests from its counters)
  KIND rclone: OUTPUT is rclone's --use-json-log stderr (bytes and transfers from the final stats)
  KIND none:   no tool counters
  STORED.json is `rclone size --json` of the run's prefix ("-" when not measured)
"""
import json
import os
import sys

COLS = [
    "run", "workload", "tool", "rep", "rc", "wall_s", "cpu_s", "peak_rss_mib", "bytes_sent",
    "requests", "stored_objects", "stored_bytes", "t50_s", "receipted", "note",
]


def last_stats(path):
    stats = None
    with open(path, errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if isinstance(rec.get("stats"), dict):
                stats = rec["stats"]
    return stats or {}


def main():
    a = sys.argv[1:]
    tsv, run, wl, tool, rep, meas, kind, output, stored = a[:9]
    note = a[9] if len(a) > 9 else ""
    m = json.load(open(meas))
    row = {
        "run": run, "workload": wl, "tool": tool, "rep": rep, "rc": m["rc"],
        "wall_s": m["wall_s"], "cpu_s": m["cpu_s"],
        "peak_rss_mib": round(m["peak_rss"] / 2**20, 1),
        "bytes_sent": "", "requests": "", "stored_objects": "", "stored_bytes": "",
        "t50_s": "", "receipted": "", "note": note,
    }
    if kind == "xfer" and os.path.exists(output):
        try:
            o = json.load(open(output))
            r, q = o.get("report", {}), o.get("requests", {})
            row["bytes_sent"] = q.get("put_bytes", "")
            row["requests"] = sum(q.get(k, 0) for k in ("puts", "stats", "lists", "gets"))
            if r.get("t50_ms") is not None:
                row["t50_s"] = round(r["t50_ms"] / 1000, 3)
            row["receipted"] = r.get("receipted", "")
        except ValueError:
            row["note"] = (note + " unparsable-report").strip()
    elif kind == "rclone" and os.path.exists(output):
        s = last_stats(output)
        row["bytes_sent"] = s.get("bytes", "")
        row["requests"] = s.get("transfers", "")
    if stored != "-" and os.path.exists(stored):
        try:
            s = json.load(open(stored))
            row["stored_objects"], row["stored_bytes"] = s.get("count", ""), s.get("bytes", "")
        except ValueError:
            pass
    new = not os.path.exists(tsv)
    with open(tsv, "a") as f:
        if new:
            f.write("\t".join(COLS) + "\n")
        f.write("\t".join(str(row[c]) for c in COLS) + "\n")


if __name__ == "__main__":
    main()
