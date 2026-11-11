#!/usr/bin/env python3
"""Audit the computer view's full-screen layout from `wonderctl tree --json` frames.

Usage: check-computer-fullscreen-layout.py TREE.json [--controlling]
The full-screen cover (and a phone or iPad turned sideways) spans the whole
screen. The computer's title pill is centred on the PICTURE (within 1 pt; the
picture is the preview beside the action rail, or the whole screen when the
actions are a row), the window buttons (More) sit beside the action rail, or end
at the trailing edge when there is no rail, and the key row (computer-session-controls-row) is centred on the
picture too.
With --controlling the trackpad toggle must also be on screen. Accessibility
frames only; the matching PNG shows the pill's fill and the hinge.
"""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("capture", type=Path)
parser.add_argument("--controlling", action="store_true")
args = parser.parse_args()
tree = json.loads(args.capture.read_text())
if isinstance(tree, dict) and "accessibility" in tree:
    tree = json.loads(tree["accessibility"]["output"])


def nodes(value):
    if isinstance(value, list):
        for child in value:
            yield from nodes(child)
    elif isinstance(value, dict):
        yield value
        yield from nodes(value.get("children", []))


frames = {}
window = None
for node in nodes(tree):
    frame = node.get("frame")
    if not frame:
        continue
    if node.get("type") == "Application" and window is None:
        window = frame
        # The unfolded Duo's Application frame has not rotated: take the
        # top-level window with the swapped dimensions, as wonderctl does.
        for child in node.get("children") or []:
            inner = child.get("frame") or {}
            if (inner.get("x") == 0 and inner.get("y") == 0
                    and inner.get("width") == frame["height"] and inner.get("height") == frame["width"]):
                window = inner
                break
    identifier = node.get("AXUniqueId")
    if identifier and identifier not in frames:
        frames[identifier] = frame


def need(identifier):
    if identifier not in frames:
        raise SystemExit(f"{identifier}: not in {args.capture.name}; capture the open full-screen viewer")
    return frames[identifier]


if window is None:
    raise SystemExit("no Application frame in the tree")
picture = frames.get("computer-session-preview")
rail = frames.get("computer-session-actions")
# The picture's own frame is the centre; a view without a preview frame falls
# back to the screen.
if picture:
    middle = picture["x"] + picture["width"] / 2
    where = "the picture's"
else:
    middle = window["x"] + window["width"] / 2
    where = "the screen's"
title = need("computer-session-connection-picker")
buttons = need("computer-session-window-buttons")
problems = []
off = title["x"] + title["width"] / 2 - middle
if abs(off) > 1:
    problems.append(f"the title pill's centre is {off:+.1f} pt from {where} centre; it must be within 1 pt")
# Beside a side rail the buttons end where the rail begins (the rail's column is
# the same width); otherwise where the system's own overlay does: the notch and
# rounded corners inset the trailing edge by ~70 pt, the Duo's status area by 100 pt.
if rail and rail["width"] < rail["height"]:
    gap = rail["x"] - (buttons["x"] + buttons["width"])
    if gap > 40 or gap < -rail["width"]:
        problems.append(f"the window buttons end {gap:.0f} pt from the action rail; they must sit beside it")
else:
    gap = window["x"] + window["width"] - (buttons["x"] + buttons["width"])
    if gap > 110:
        problems.append(f"the window buttons end {gap:.0f} pt short of the trailing edge")
if buttons["x"] < title["x"] + title["width"] - 1:
    problems.append("the window buttons overlap the title pill")
if "computer-session-exit-full-screen" not in frames:
    problems.append("the exit-full-screen control (in the action rail) is missing")
bar = frames.get("computer-session-control-bar") or frames.get("computer-session-controls-row")
if bar:
    off = bar["x"] + bar["width"] / 2 - middle
    if abs(off) > 1:
        problems.append(f"the key row's centre is {off:+.1f} pt from {where} centre; it must be within 1 pt")
if args.controlling:
    toggle = need("computer-session-trackpad-toggle")
    if toggle["width"] < 1 or toggle["x"] < window["x"] or toggle["x"] + toggle["width"] > window["x"] + window["width"] + 1:
        problems.append("the trackpad toggle is not on screen")
if problems:
    raise SystemExit(f"{args.capture.name}: " + "; ".join(problems))
print(f"{args.capture.name}: title centred, buttons {gap:.0f} pt from the trailing edge, key row {'centred' if bar else 'absent'} ok")
