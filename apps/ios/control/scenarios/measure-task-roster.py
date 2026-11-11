#!/usr/bin/env python3
"""Measure roster/pill proximity from a settled `wonderctl tree` capture.

Run beside the actual screenshot. This checks AX frame distance, not the
arrow's pixels; inspect the screenshot for the arrow's direction and tip.
"""
import argparse
import json
import math
import re
import subprocess
import time
from pathlib import Path


def measure(tree):
    frames = {}
    for identifier in ("project-subagent-status-pill", "project-subagent-roster-list"):
        matches = re.findall(r"^\w+ \| " + re.escape(identifier)
                             + r" \| .* \| (-?[\d.]+),(-?[\d.]+) ([\d.]+)x([\d.]+)$",
                             tree, re.MULTILINE)
        if len(matches) != 1:
            raise ValueError(f"Expected one {identifier} AX frame, found {len(matches)}")
        frames[identifier] = tuple(map(float, matches[0]))
    pill, roster = frames.values()
    px, py, pw, ph = pill
    rx, ry, rw, rh = roster
    if min(pw, ph, rw, rh) <= 0:
        raise ValueError("Capture settled, nonempty AX frames")
    dx = max(px - rx - rw, rx - px - pw, 0)
    dy = max(py - ry - rh, ry - py - ph, 0)
    return {"frames": frames, "nearestFrameGapPoints": math.hypot(dx, dy)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tree", type=Path)
    parser.add_argument("--max-gap", type=float, default=24)
    parser.add_argument("--still-pair", type=Path, nargs=2,
                        help="Same-state captures 0.5 s apart; require identical pill pixels")
    parser.add_argument("--scale", type=float, default=3, help="Screenshot pixels per AX point")
    parser.add_argument("--capture-udid", help="Capture --still-pair on this simulator, 0.5 s apart")
    parser.add_argument("--display", default="internal", help="simctl display (Duo inner: primary-1)")
    parser.add_argument("--symbol", help="Require this AX Image symbol inside the pill's frame")
    args = parser.parse_args()
    result = measure(args.tree.read_text())
    if args.symbol:
        x, y, width, height = result["frames"]["project-subagent-status-pill"]
        images = re.findall(r"^Image \| " + re.escape(args.symbol)
                            + r" \| .* \| (-?[\d.]+),(-?[\d.]+) ([\d.]+)x([\d.]+)$",
                            args.tree.read_text(), re.MULTILINE)
        inside = [(sx, sy, sw, sh) for sx, sy, sw, sh in (map(float, frame) for frame in images)
                  if sx >= x and sy >= y and sx + sw <= x + width and sy + sh <= y + height]
        if len(inside) != 1:
            raise ValueError(f"Expected one {args.symbol} AX Image inside the pill, found {len(inside)}")
        result["pillSymbol"] = args.symbol
    if args.capture_udid:
        if not args.still_pair:
            parser.error("--capture-udid requires --still-pair output paths")
        started = []
        captures = []
        for path in args.still_pair:
            if started:
                time.sleep(max(0, started[0] + 0.5 - time.monotonic()))
            started.append(time.monotonic())
            # Saving a full iPad/Duo PNG can take longer than the interval.
            # Start the next request on time while the first writes its image.
            captures.append(subprocess.Popen(
                ["xcrun", "simctl", "io", args.capture_udid, "screenshot",
                 "--display=" + args.display, str(path)], stdout=subprocess.PIPE, stderr=subprocess.PIPE))
        for capture in captures:
            _, error = capture.communicate(timeout=30)
            if capture.returncode:
                raise RuntimeError(error.decode(errors="replace"))
        result["captureStartedGapSeconds"] = started[1] - started[0]
        result["captureTimingMatches"] = abs(result["captureStartedGapSeconds"] - 0.5) < 0.05
    if args.still_pair:
        from PIL import Image, ImageChops

        x, y, width, height = result["frames"]["project-subagent-status-pill"]
        box = tuple(round(value * args.scale) for value in (x, y, x + width, y + height))
        first, second = (Image.open(path).convert("RGB") for path in args.still_pair)
        if first.size != second.size or box[2] > first.width or box[3] > first.height:
            raise ValueError("Capture dimensions must match and contain the AX pill bounds at --scale")
        difference = ImageChops.difference(first.crop(box), second.crop(box))
        red, green, blue = difference.split()
        changed = sum(ImageChops.lighter(ImageChops.lighter(red, green), blue).histogram()[1:])
        result["pillPixelBox"] = box
        result["pillChangedPixels"] = changed
        result["still"] = changed == 0
    print(json.dumps(result, indent=2))
    raise SystemExit(0 if result["nearestFrameGapPoints"] <= args.max_gap
                     and result.get("captureTimingMatches", True)
                     and result.get("still", True) else 1)
