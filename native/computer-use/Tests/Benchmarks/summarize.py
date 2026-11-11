#!/usr/bin/env python3
"""Render measured latency-bench rows; fail on empty/incomplete/failed runs."""

import argparse
import csv
import re
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", type=Path)
    parser.add_argument("--expect-rows", type=int)
    parser.add_argument("--csv", type=Path)
    args = parser.parse_args()
    rows = []
    for path in args.logs:
        text = path.read_text()
        if not re.search(r"testScreenShareLoopbackLatency\]' passed", text):
            parser.error(f"{path}: benchmark did not pass or is incomplete")
        for line in text.splitlines():
            if line.startswith("latency-bench config="):
                row = dict(re.findall(r"(\w+)=(.*?)(?= [A-Za-z]\w*=|$)", line))
                row["log"] = path.name
                rows.append(row)
    if not rows or (args.expect_rows is not None and len(rows) != args.expect_rows):
        parser.error(f"expected {args.expect_rows or 'nonzero'} rows, found {len(rows)}")
    if args.csv:
        with args.csv.open("w", newline="") as output:
            writer = csv.DictWriter(output, fieldnames=list(rows[0]))
            writer.writeheader()
            writer.writerows(rows)
    columns = ["config", "maxBitrate", "sourceFPS", "outboundFPS", "encoder", "sps", "avgEncode",
               "median", "p90", "cpu", "pass95", "reasonInferred"]
    print("| " + " | ".join(columns) + " |")
    print("| " + " | ".join(["---"] * len(columns)) + " |")
    for row in rows:
        print("| " + " | ".join(row[key] for key in columns) + " |")
    print(f"\nMeasured rows: {len(rows)}; at least 95% of target: "
          f"{sum(row['pass95'] == 'yes' for row in rows)}. CPU: 100% = one core.")


if __name__ == "__main__":
    main()
