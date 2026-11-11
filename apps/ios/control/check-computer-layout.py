#!/usr/bin/env python3
"""Audit the Computer viewer's geometry from the .json written beside each shot.

Usage: check-computer-layout.py TREE.json [--controlling]
TREE is `wonderctl tree --json` (or a failed step's native-context .json) of the open
viewer. Checks, from accessibility frames only:
the header is one row (Close, the computer picker centred between it and the window
buttons, then More), and exactly one control chip sits in the single control row
under the picture. It does not judge pixels;
look at the matching PNG for the letterbox and the border.
"""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("capture", type=Path)
parser.add_argument("--controlling", action="store_true", help="the keys row may sit under the picture")
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
for node in nodes(tree):
    identifier = node.get("AXUniqueId")
    frame = node.get("frame")
    if identifier and frame and identifier.startswith("computer-") and identifier not in frames:
        frames[identifier] = frame


def need(identifier):
    if identifier not in frames:
        raise SystemExit(f"{identifier}: not in {args.capture.name}; capture the open viewer")
    return frames[identifier]


container = need("computer-session-container")
preview = need("computer-session-preview")
title = need("computer-session-connection-picker")
chips = [name for name in ("computer-session-take-control", "computer-session-done") if name in frames]
if len(chips) != 1:
    raise SystemExit(f"expected one control chip, found {chips}; take and give control share one spot")
chip = frames[chips[0]]
problems = []
controls = frames.get("computer-session-controls-row")
if controls and controls["height"] > 200:  # two rows (shortcuts, then the control bar; the dock joins on a phone without the keyboard), never a stack
    problems.append(f"the control area is {controls['height']:.0f} pt tall; it should be two rows")
if title["y"] > container["y"] + 140:
    problems.append(f"the title is not in the top row (y {title['y']:.0f})")
if chip["y"] < preview["y"] + preview["height"] * 0.5:
    problems.append("the control chip is not in the row under the picture")
close = frames.get("computer-session-close")
if close and close["x"] > title["x"]:
    problems.append("Close is not on the leading side of the title")
if close and abs(close["y"] - title["y"]) > 24:
    problems.append("Close is not in the header row")
centre = container["x"] + container["width"] / 2
if abs(title["x"] + title["width"] / 2 - centre) > 40:
    problems.append(f"the title is {title['x'] + title['width'] / 2 - centre:+.0f} pt off the pane's centre")
more = frames.get("computer-session-more")
if more and more["x"] < title["x"] + title["width"] - 1 and abs(more["y"] - title["y"]) < 20:
    problems.append("the More button overlaps the title")
if "computer-session-refresh" in frames:
    problems.append("Refresh is a separate button again; it belongs in the More menu")
for name, frame in (("title", title), ("chip", chip)):
    if frame["x"] < container["x"] - 1 or frame["x"] + frame["width"] > container["x"] + container["width"] + 1:
        problems.append(f"the {name} is clipped by the pane edge")
if problems:
    raise SystemExit(f"{args.capture.name}: " + "; ".join(problems))
print(f"{args.capture.name}: picture {preview['height']:.0f}/{container['height']:.0f} pt, title x {title['x']:.0f}, chip {chips[0]} ok")
