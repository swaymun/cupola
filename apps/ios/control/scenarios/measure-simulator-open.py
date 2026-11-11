#!/usr/bin/env python3
"""Time the Simulators tool from choosing a simulator to its first decoded frame.

Live check, not a fixture: the lane's Diagnostics app must already be paired with a
real computer (scripts/wonderctl pair-host) that has the simulator booted. It reads
the app's `open-latency` log (ComputerOpenTimeline) for each hop in milliseconds
since the opening began: session-created, receiver-start, offer-received,
answer-accepted, peer-connected and first-frame, and fails when the median
first-frame is over --bound-ms or a run never gets a picture. Use a simulator that
is not repainting (an idle home screen) as well as a busy one: a still one is what
exposes a host that only sends frames when the screen changes.

  measure-simulator-open.py --udid UDID --project-disclosure ID --thread ID [--runs 3] [--bound-ms 2000]
"""
import argparse
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
WONDER = [str(ROOT / "scripts/wonderctl")]


def run(*args):
    return subprocess.run([*WONDER, *args], capture_output=True, text=True, cwd=ROOT).stdout


def wait_for(needle, seconds, gone=False):
    end = time.time() + seconds
    while time.time() < end:
        tree = run("tree")
        if (needle in tree) != gone:
            return tree
    return None


def one_open(args, first):
    if first:
        run("--driver", "launch")
        time.sleep(4)
        run("sidebar", "open")
        run("tap", "--id", args.project_disclosure)
        time.sleep(1)
        run("tap", "--id", args.thread)
        wait_for("conversation-simulators-pill", 20)
        run("tap", "--id", "conversation-simulators-pill")
    choice = "simulator-choice:simulator:" + args.udid.upper()
    if wait_for(choice, 20) is None:
        raise SystemExit("the simulator is not in the list: boot it first")
    began = time.time()
    run("tap", "--id", choice)
    tree = None
    while time.time() - began < 60:
        tree = run("tree")
        if "| Live |" in tree or "simulator-notice" in tree:
            break
    logs = run("logs", "--last", "2m", "--lines", "3000")
    # The newest opening's hops: everything after the last `begin`.
    latest = logs.rsplit("open-latency begin", 1)[-1]
    hops = {m.group(1): int(m.group(2)) for m in re.finditer(r"open-latency ([a-z-]+) (\d+) ms", latest)}
    run("tap", "--id", "simulator-picker")
    time.sleep(1.5)
    return hops


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--udid", required=True)
    parser.add_argument("--project-disclosure", required=True, help="sidebar id, e.g. project-disclosure:<id>")
    parser.add_argument("--thread", required=True, help="sidebar id, e.g. project-thread:claude:<id>")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--bound-ms", type=int, default=2000)
    args = parser.parse_args()
    runs = [one_open(args, first=(n == 0)) for n in range(args.runs)]
    order = ["session-created", "receiver-start", "offer-received", "answer-accepted", "peer-connected", "first-frame"]
    for hop in order:
        values = [r[hop] for r in runs if hop in r]
        print("%-16s %s" % (hop, "median %5.0f ms  %s" % (statistics.median(values), values) if values else "never"))
    frames = [r.get("first-frame") for r in runs]
    if None in frames:
        print("FAIL: a run never got a picture", file=sys.stderr)
        return 1
    if statistics.median(frames) > args.bound_ms:
        print("FAIL: median first frame %.0f ms > %d ms" % (statistics.median(frames), args.bound_ms), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
