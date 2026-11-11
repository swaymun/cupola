#!/usr/bin/env python3
"""Audit the folded Duo's chat or New chat composer from `wonderctl tree --json` frames.

Usage: check-folded-layout.py TREE.json
Half an iPad mini: the composer rests within 24 pt of the window bottom, model and effort
share the button row with attach and send, and (when shown) the scroll-to-bottom arrow sits
centred above the composer. Accessibility frames only; the matching PNG shows
the header blur and the line at the pane edge, which AX cannot see.
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


frames = {}
window = None
for node in nodes(tree):
    frame = node.get("frame")
    if not frame:
        continue
    if node.get("type") == "Application" and window is None:
        window = frame
    identifier = node.get("AXUniqueId")
    if identifier and identifier not in frames:
        frames[identifier] = frame

newchat = "new-chat-send" in frames
ids = dict(send="new-chat-send", model="project-agent-picker", effort="new-chat-effort",
           attach="new-chat-attach", draft="new-chat-draft") if newchat else \
    dict(send="send-message", model="project-composer-model", effort="project-composer-effort",
         attach="composer-actions", draft="message-draft")
f = {k: frames.get(v) for k, v in ids.items()}
missing = [ids[k] for k, v in f.items() if v is None]
if window is None or missing:
    sys.exit(f"folded layout: missing {missing or 'Application frame'}")

errors = []
# The composer surface pads its controls by about 6 pt; New chat reports the surface itself.
surface = frames.get("new-chat-composer")
bottom = surface["y"] + surface["height"] if surface else f["send"]["y"] + f["send"]["height"] + 6.5
gap = window["height"] - bottom
if gap > 24:
    errors.append(f"composer bottom is {gap:.1f} pt from the window bottom (max 24)")
row = f["send"]["y"]
for name in ("model", "effort", "attach"):
    if abs(f[name]["y"] - row) > 8:
        errors.append(f"{ids[name]} is not on the send row (y {f[name]['y']:.0f} vs {row:.0f})")
if not f["attach"]["x"] < f["model"]["x"] < f["effort"]["x"] < f["send"]["x"]:
    errors.append("button row order must be attach, model, effort, send")
arrow = frames.get("scroll-to-bottom")
if arrow and not newchat:
    top = f["draft"]["y"] - 3
    # Centred on the chat pane (the composer's width), not the window: the Duo's side rail sits beside the pane.
    middle = arrow["x"] + arrow["width"] / 2
    pane = f["draft"]["x"] + f["draft"]["width"] / 2
    if abs(middle - pane) > 4:
        errors.append(f"scroll arrow centre {middle:.0f} vs composer centre {pane:.0f} (max 4 off)")
    above = top - (arrow["y"] + arrow["height"])
    if not -2 <= above <= 20:
        errors.append(f"scroll arrow bottom is {above:.1f} pt above the composer (want 0-20)")
if errors:
    sys.exit("folded layout: " + "; ".join(errors))
print(f"folded layout ok: composer gap {gap:.1f} pt, chip on the button row"
      + (f", arrow {above:.1f} pt above the composer" if arrow and not newchat else ""))
