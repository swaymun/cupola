#!/usr/bin/env python3
"""UX-DUO-8: the Duo's side rail keeps its Chats button, and the composer stays in its column.

Usage: check-duo-rail-layout.py TREE.json --expect rail|bar[,keyboard|no-keyboard]
The scenario states the posture it just put the Duo in, so a stale bar axis fails instead of being
inferred from the button. From `wonderctl tree --json`: `open-sidebar` is present, enabled, at
least 44 x 44 (40 in the bar), wholly inside the window and not under the keyboard (hittable by
position, and no other rail tool over it; the scenario taps it for the real hit test). `rail` (folded, unfolded upright): it sits in
a vertical column hugging either window edge (the side follows the orientation) and the composer
(`message-draft`, `send-message`) stays out of that column. `bar` (the
stacked inner posture): it leads the horizontal bar, top left.
`keyboard`: the software keyboard (UIKeyboardLayoutStar) is up, inside the window, with the
composer above it; `no-keyboard`: it is not. `open` / `closed`: the button's label says Hide Chats /
Show Chats (the sidebar's state). `covered`: the sidebar fills the screen over the chat (folded, open),
so the composer and keyboard are not asserted. `toggled`: the label differs from the label at the last
passing check (a tap on the button changed the sidebar: it was hit); the label is kept in a temp file.
"""
import json
import sys
import tempfile
from pathlib import Path

args = sys.argv[1:]
if "--expect" not in args or len(args) < 3:
    sys.exit("duo rail: pass --expect rail|bar[,keyboard|no-keyboard] with the posture the scenario just set")
expect = set(args[args.index("--expect") + 1].split(","))
if not expect & {"rail", "bar"}:
    sys.exit("duo rail: --expect needs rail or bar")
tree = json.loads(Path(args[0]).read_text())
if isinstance(tree, dict) and "accessibility" in tree:
    tree = json.loads(tree["accessibility"]["output"])


def nodes(value):
    if isinstance(value, list):
        for child in value:
            yield from nodes(child)
    elif isinstance(value, dict):
        yield value
        yield from nodes(value.get("children", []))


frames, enabled, labels, window = {}, {}, {}, None
for node in nodes(tree):
    frame = node.get("frame")
    if not frame:
        continue
    # The Application frame can keep the other orientation's size on the inner display; the
    # navigation Toolbar group spans the window that is actually shown.
    if node.get("AXLabel") == "Toolbar" and node.get("type") == "Group":
        window = frame
    elif node.get("type") == "Application" and window is None:
        window = frame
    identifier = node.get("AXUniqueId")
    if identifier and identifier not in frames:
        frames[identifier] = frame
        enabled[identifier] = node.get("enabled", True)
        labels[identifier] = node.get("AXLabel")


def right(f): return f["x"] + f["width"]
def bottom(f): return f["y"] + f["height"]


errors = []
button = frames.get("open-sidebar")
keyboard = next((f for k, f in frames.items() if k.startswith("UIKeyboardLayoutStar")), None)
rail = "rail" in expect
if window is None:
    errors.append("no Application frame")
elif button is None:
    errors.append("the sidebar button (open-sidebar) is missing")
else:
    cx, cy = button["x"] + button["width"] / 2, button["y"] + button["height"] / 2
    edge = min(cx - window["x"], right(window) - cx)
    leading = cx < window["x"] + window["width"] / 2
    if rail and (edge > 90 or cy - window["y"] < 70):
        errors.append(f"expected the side rail but open-sidebar is at {button['x']},{button['y']} in a {window['width']}x{window['height']} window (bar axis stale?)")
    if not rail and (cy - window["y"] >= 70 or not leading):
        errors.append(f"expected the horizontal bar but open-sidebar is at {button['x']},{button['y']} in a {window['width']}x{window['height']} window (bar axis stale?)")
    least = 44 if rail else 40
    if button["width"] < least or button["height"] < least:
        errors.append(f"open-sidebar is {button['width']}x{button['height']}, under {least} pt")
    if button["x"] < window["x"] or button["y"] < window["y"] or right(button) > right(window) or bottom(button) > bottom(window):
        errors.append(f"open-sidebar {button} lies outside the window {window}")
    if not enabled.get("open-sidebar", True):
        errors.append("open-sidebar is disabled")
    if keyboard and bottom(button) > keyboard["y"] and right(button) > keyboard["x"] and button["x"] < right(keyboard):
        errors.append(f"open-sidebar {button} sits under the keyboard at y={keyboard['y']}")
    tools = frames.get("duo-rail-tools")
    if rail and "covered" not in expect and tools and tools["x"] < right(button) and right(tools) > button["x"] \
            and tools["y"] < bottom(button) - 0.5 and bottom(tools) > button["y"] + 0.5:
        errors.append(f"the rail's tools (y={tools['y']}..{bottom(tools)}) overlap open-sidebar (y={button['y']}..{bottom(button)}): a tap on the Chats button lands on a tool")
    if "open" in expect and labels.get("open-sidebar") != "Hide Chats":
        errors.append(f"expected the sidebar open (Hide Chats) but the button says {labels.get('open-sidebar')!r}")
    if "closed" in expect and labels.get("open-sidebar") != "Show Chats":
        errors.append(f"expected the sidebar closed (Show Chats) but the button says {labels.get('open-sidebar')!r}")
    for ident in ("message-draft", "send-message") if rail and "covered" not in expect else ():
        frame = frames.get(ident)
        if frame and frame["x"] < right(button) and right(frame) > button["x"]:
            errors.append(f"{ident} spans x={frame['x']}..{right(frame)}, across the rail's column x={button['x']}..{right(button)}")
    if "keyboard" in expect and "covered" not in expect:
        if keyboard is None:
            errors.append("the software keyboard is not up")
        else:
            if keyboard["y"] + keyboard["height"] > bottom(window) + 1 or keyboard["y"] < window["y"]:
                errors.append(f"the keyboard {keyboard} is outside the window {window}")
            draft = frames.get("message-draft")
            if draft is None:
                errors.append("message-draft is missing with the keyboard up")
            elif bottom(draft) > keyboard["y"] + 1:
                errors.append(f"message-draft ends at y={bottom(draft)}, under the keyboard at y={keyboard['y']}")
    if "no-keyboard" in expect and "covered" not in expect and keyboard is not None:
        errors.append("the software keyboard is up but none was expected")
state = Path(tempfile.gettempdir()) / "wonder-ux-duo-8-rail-label.txt"
if "toggled" in expect and not errors:
    last = state.read_text() if state.exists() else None
    if last == labels.get("open-sidebar"):
        errors.append(f"the tap did not change the sidebar: the button still says {last!r}")
if errors:
    sys.exit("duo rail: " + "; ".join(errors))
state.write_text(str(labels.get("open-sidebar")))
kb = "keyboard" if keyboard else "no keyboard"
print(f"duo {'rail' if rail else 'bar'} ok ({kb}): open-sidebar '{labels.get('open-sidebar')}' {button['x']},{button['y']} {button['width']}x{button['height']}")
