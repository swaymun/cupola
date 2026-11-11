#!/usr/bin/env python3
"""Audit New chat's setup pills (computer, Project) from `wonderctl tree --json` frames.

Usage: check-newchat-pills-layout.py TREE.json
The pills are one group flush with the composer's leading edge: the first capsule starts where
the composer starts, pills sharing a row sit side by side with a small gap (no pill floats
alone with a gap before the next), and a pill on a later row starts at the same edge.
A hinge across the row (the unfolded Duo; `check-newchat-pills-hinge-layout.py`) moves the whole group
flush to the start of the side that holds it, so there the first capsule may start right of the composer's
edge but the group stays contiguous and every row of it starts at the same x.
Accessibility frames only; the matching PNG shows the capsules.
"""
import json
import os
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


frames = {}
for node in nodes(tree):
    frame, identifier = node.get("frame"), node.get("AXUniqueId")
    if frame and identifier and identifier not in frames:
        frames[identifier] = frame

ids = ["connection-picker", "destination-picker"]
missing = [i for i in ["new-chat-composer"] + ids if i not in frames]
if missing:
    sys.exit(f"new chat pills: missing {missing}")
composer = frames["new-chat-composer"]
# The accessibility frame is the label; its capsule reaches 10 pt beyond it (SetupPill).
PAD = 10
pills = [dict(x=frames[i]["x"] - PAD, y=frames[i]["y"], width=frames[i]["width"] + 2 * PAD) for i in ids]
errors = []
hinge = os.environ.get("PILLS_HINGE") == "1"
edge = composer["x"]
if not hinge and abs(pills[0]["x"] - edge) > 2:
    errors.append(f"first pill starts at x={pills[0]['x']:.0f}, composer at {edge:.0f} (max 2 off)")
if hinge and pills[0]["x"] < edge - 2:
    errors.append(f"first pill starts at x={pills[0]['x']:.0f}, left of the composer at {edge:.0f}")
for name, pill in zip(ids, pills):
    if pill["x"] + pill["width"] > composer["x"] + composer["width"] + 2:
        errors.append(f"{name} runs past the composer's trailing edge")
first, second = pills
if abs(second["y"] - first["y"]) < 4:
    gap = second["x"] - (first["x"] + first["width"])
    if not 0 <= gap <= 10:
        errors.append(f"pills on one row are {gap:.0f} pt apart (0-10)")
elif abs(second["x"] - first["x"]) > 2:
    errors.append(f"wrapped pill starts at x={second['x']:.0f}, the group at {first['x']:.0f}")
if errors:
    sys.exit("new chat pills: " + "; ".join(errors))
print(f"new chat pills ok: first x={first['x']:.0f} composer x={edge:.0f}")
