#!/usr/bin/env python3
"""Replay the canonical native reachability flow on the folded Duo cover.

Build first and use a fully slimmed, unit-owned WONDER_CONTROL_SIMULATOR_SUFFIX.
The cover's keyboard moves the computer picker into the phone probe's blank
point. Safari's Close is on the Duo rail. Keep one canonical set of assertions;
only the three cover-specific native tap points differ. Inner-display native
input remains unqualified under #387; use launch/posture captures there. Every tap has a
resulting-state assertion, so changed SDK geometry fails visibly.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[4]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--angle", type=int, choices=[0], required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    suffix = os.environ.get("WONDER_CONTROL_SIMULATOR_SUFFIX", "").strip()
    if not suffix:
        parser.error("Use your unit's WONDER_CONTROL_SIMULATOR_SUFFIX lane")
    output = args.output or ROOT / ".local/control/duo-layout" / suffix / (str(args.angle) + "-" + str(time.time_ns()))
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    canonical = Path(__file__).with_name("layout-reachability.wctl")
    source = canonical.read_text()
    overrides = {
        "tap --xy 40,150": "tap --xy 4,100",
        "tap --xy 4,250": "tap --xy 4,150",
        "tap --xy 35,70": "tap --xy 410,190",
    }
    for old, new in overrides.items():
        if source.splitlines().count(old) != 1:
            raise SystemExit("The canonical reachability taps changed; recapture the Duo blank, margin and browser Close points")
        source = source.replace(old, new)
    source = "\n".join(line for line in source.splitlines() if not line.startswith("#"))
    scenario = output / ("duo-layout-" + str(args.angle) + ".wctl")
    source = "hinge " + str(args.angle) + "\norient portrait\n" + source
    source = "\n".join("shot duo-" + str(args.angle) + "-" + line[5:] if line.startswith("shot ") else line for line in source.splitlines())
    scenario.write_text(source + "\n")
    (output / "provenance.json").write_text(json.dumps({"angle": args.angle, "suffix": suffix,
        "canonicalSha256": hashlib.sha256(canonical.read_bytes()).hexdigest(), "nativeTapOverrides": overrides}, indent=2) + "\n")
    control = [str(ROOT / "scripts/wonderctl"), "--device", "duo"]
    subprocess.run(control + ["install"], cwd=ROOT, check=True)
    result = subprocess.run(control + ["run", str(scenario), "--input", "axe"], cwd=ROOT)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
