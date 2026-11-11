#!/usr/bin/env python3
"""Audit that Settings is one full-width page, from `wonderctl tree --json` frames.

Usage: check-settings-column-layout.py TREE.json
The search field, which spans the settings list's rows, reaches near the window's trailing edge
(less the Duo's side rail and the list's margins), so no detail pane sits beside it, and nothing named for a detail pane is on screen. Accessibility frames only.
"""
import json
import sys
from pathlib import Path

tree = json.loads(Path(sys.argv[1]).read_text())
if isinstance(tree, dict) and "accessibility" in tree:
    tree = json.loads(tree["accessibility"]["output"])


def nodes(value):
    if isinstance(value, list):
        for child in value:
            yield from nodes(child)
    elif isinstance(value, dict):
        yield value
        yield from nodes(value.get("children", []))


window, frames, labels = None, {}, []
for node in nodes(tree):
    frame = node.get("frame")
    if node.get("type") == "Application" and frame and window is None:
        window = frame
    if frame and node.get("AXUniqueId") and node["AXUniqueId"] not in frames:
        frames[node["AXUniqueId"]] = frame
    labels.append(node.get("AXLabel") or "")
# The list's own frame reads empty on the Duo cover; its first row is a reliable stand-in.
if window is None or "settings-search-field" not in frames:
    sys.exit("settings column: missing the Application frame or settings-search-field")
lst = frames["settings-search-field"]
errors = []
# Duo's vertical bar takes about 90 pt on the right when unfolded; rows are inset about 20 pt.
if lst["x"] + lst["width"] < window["x"] + window["width"] - 150:
    errors.append(f"search field ends at {lst['x'] + lst['width']:.0f}, window at {window['x'] + window['width']:.0f}: room for a second column")
if any("Settings detail" in label for label in labels):
    errors.append("a Settings detail pane is on screen")
if errors:
    sys.exit("settings column: " + "; ".join(errors))
print(f"settings column ok: search field {lst['x']:.0f}..{lst['x'] + lst['width']:.0f} of window {window['width']:.0f}")
