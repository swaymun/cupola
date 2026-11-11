#!/usr/bin/env python3
"""Check the rendered recovery fixture, not just its accessibility state.

Usage: check-computer-recovery-captures.py RECONNECT.log CHAT_SWITCH.log
Run the two .wctl scenarios first. This verifies visible fixture pixels, dimming
and preserved picture size; inspect the captures for text readability and status.
"""
import argparse
import json
import re
from collections import Counter
from pathlib import Path

from PIL import Image

LIVE = (235, 240, 247)  # The decoded fixture's pale blue page surround.
DIMMED = (141, 144, 148)  # The same pixels under the 40% black scrim.
RECOVERY_DIMMED = (
    "brief-disconnect", "network-change", "background-paused",
    "foreground-reconnecting", "foreground-network-change", "host-restart", "unavailable-try-again",
)
RECOVERY_LIVE = (
    "brief-resumed", "network-resumed", "foreground-resumed",
    "actual-foreground", "host-resumed", "try-again-resumed",
)


def shots(log, capture_root=None):
    text = log.read_text()
    if not re.search(r"^PASS .* steps in ", text, re.M):
        raise ValueError(f"{log}: finish the scenario successfully before checking its pictures")
    result = {}
    for value in re.findall(r"^([^\n]+\.png)$", text, re.M):
        path = Path(value)
        if capture_root is not None:
            path = capture_root / path.name
        if "-recovery-" in path.stem:
            result[path.stem.split("-recovery-", 1)[1]] = path
    return result


def pixels(path, color):
    with Image.open(path) as source:
        picture = source.convert("RGB")
        counts = Counter(picture.get_flattened_data() if hasattr(picture, "get_flattened_data") else picture.getdata())
        count = sum(n for rgb, n in counts.items() if all(abs(a - b) <= 1 for a, b in zip(rgb, color)))
        return count, picture.size


def check(group, baseline, cases, results):
    if baseline not in group:
        raise ValueError(f"missing {baseline}: retain the scenario's original picture capture")
    reference, _ = pixels(group[baseline], LIVE)
    if reference < 1000:
        raise ValueError(f"{group[baseline]}: decoded fixture did not render; check its IOSurface/Metal buffer and renderer attachment")
    for name, dimmed in cases:
        if name not in group:
            raise ValueError(f"missing {name}: capture the held state before resuming it")
        path = group[name]
        count, dimensions = pixels(path, DIMMED if dimmed else LIVE)
        if abs(count - reference) > max(200, reference * 0.05):
            raise ValueError(f"{path}: last-picture pixels/size changed ({count} vs {reference}); preserve the decoded frame and its geometry through recovery")
        results.append(dict(case=name, capture=str(path), dimensions=dimensions,
                            dimmed=dimmed, fixturePixels=count, referencePixels=reference))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reconnect", type=Path)
    parser.add_argument("chat_switch", type=Path)
    parser.add_argument("--capture-root", type=Path, help="directory containing retained PNGs, for reruns after worktree cleanup")
    args = parser.parse_args()
    results = []
    check(shots(args.reconnect, args.capture_root), "live-frame",
          [(name, True) for name in RECOVERY_DIMMED] + [(name, False) for name in RECOVERY_LIVE], results)
    check(shots(args.chat_switch, args.capture_root), "chat-original",
          [("chat-restored-paused", True), ("chat-fast-return", True), ("chat-restored-view-only", False)], results)
    print(json.dumps(dict(result="passed", checked=len(results), captures=results), indent=2))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError) as error:
        raise SystemExit(str(error)) from error
