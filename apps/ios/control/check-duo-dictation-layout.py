#!/usr/bin/env python3
"""UX-DUO-8: the composer grows with each dictated update and never runs under the rail.

Usage: check-duo-dictation-layout.py TREE.json [--expect settled]
Reads the fixture dictation stream (`-diagnostics-dictation-stream`: words word1..wordN, ten more
per step) from `wonderctl tree --json`. The composer's height may not shrink between updates
and, once the text no longer fits the first two lines (20 words and more), must be taller than
it was at 10 words (the lag measured before the fix left it at 68 pt with 20 words). With
`--expect settled` (a second reading of the same step, one tree fetch later) its height must equal the
first reading: a composer that is still growing after the update has lagged it. Beside a vertical rail
(on either window edge) it stays out of the Chats button's column. State between steps lives in a temp
file that the first step (10 words) resets.
"""
import json
import re
import sys
import tempfile
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


frames, draft = {}, None
for node in nodes(tree):
    frame = node.get("frame")
    identifier = node.get("AXUniqueId")
    if frame and identifier and identifier not in frames:
        frames[identifier] = frame
        if identifier == "message-draft":
            draft = node
if draft is None or "open-sidebar" not in frames:
    sys.exit("dictation layout: message-draft or open-sidebar missing")
words = len(re.findall(r"word\d+", str(draft.get("AXValue") or "")))
frame = frames["message-draft"]
height = frame["height"]
state = Path(tempfile.gettempdir()) / "wonder-ux-duo-8-dictation-height.json"
errors = []
settled = "settled" in sys.argv[1:]
seen = json.loads(state.read_text()) if state.exists() else None
if settled:
    if seen is None or seen.get("words") != words:
        errors.append(f"no first reading at {words} words to settle against")
    elif abs(height - seen["last"]) > 0.5:
        errors.append(f"the composer was {seen['last']} pt right after the update and {height} pt once settled, at {words} words: it lagged the text")
elif words <= 10 or seen is None:
    state.write_text(json.dumps({"first": height, "last": height, "words": words}))
else:
    if height < seen["last"] - 0.5:
        errors.append(f"the composer shrank from {seen['last']} to {height} pt at {words} words")
    if words >= 20 and height <= seen["first"] + 0.5:
        errors.append(f"the composer is still {height} pt at {words} words, as at 10 words: it did not grow with the text")
    state.write_text(json.dumps({"first": seen["first"], "last": height, "words": words}))
rail = frames["open-sidebar"]
window = next((n["frame"] for n in nodes(tree) if n.get("type") == "Group" and n.get("AXLabel") == "Toolbar" and n.get("frame")), None)
if window and rail["y"] + rail["height"] / 2 - window["y"] < 70:
    rail = None  # horizontal bar at the top: no column beside the composer
for ident in ("message-draft", "send-message", "stop-response"):
    other = frames.get(ident)
    if rail and other and other["x"] < rail["x"] + rail["width"] and other["x"] + other["width"] > rail["x"]:
        errors.append(f"{ident} spans x={other['x']}..{other['x'] + other['width']}, across the rail's column x={rail['x']}..{rail['x'] + rail['width']}")
if errors:
    sys.exit("dictation layout: " + "; ".join(errors))
print(f"dictation layout ok: {words} words, composer {height} pt")
