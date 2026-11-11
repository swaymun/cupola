#!/usr/bin/env python3
"""Audit an unfolded Duo (vertical hinge) chat-alone layout from `wonderctl tree --json` frames.

Usage: check-duo-chat-centre-layout.py TREE.json [--expect chat-alone|split]
Chats sits on the leading display and the chat alone on the trailing one, so the
title pill, composer and scroll-to-bottom arrow (when shown) lie wholly right of
the hinge (the window's middle) and the Chats sidebar wholly left of it, and the
pill and arrow are centred on the composer within 2 pt, nothing but chat sits behind the pill, and rail buttons do not overlap. Accessibility frames only; the matching PNG
shows the fills.
"""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("capture", type=Path)
parser.add_argument("--expect", default="chat-alone")
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


frames, window = {}, None
for node in nodes(tree):
    frame = node.get("frame")
    if not frame:
        continue
    if node.get("type") == "Application" and window is None:
        window = frame
        for child in node.get("children") or []:
            inner = child.get("frame") or {}
            if (inner.get("x") == 0 and inner.get("y") == 0
                    and inner.get("width") == frame["height"] and inner.get("height") == frame["width"]):
                window = inner
                break
    identifier = node.get("AXUniqueId")
    if identifier and identifier not in frames:
        frames[identifier] = frame
if window is None:
    raise SystemExit("no Application frame in the tree")
if window["width"] <= window["height"]:
    raise SystemExit(f"window {window['width']:.0f}x{window['height']:.0f} is not the wide (vertical hinge) posture")
hinge = window["x"] + window["width"] / 2
problems = []
for identifier in ("conversation-title-menu", "message-draft", "scroll-to-bottom"):
    frame = frames.get(identifier)
    if frame is None:
        if identifier != "scroll-to-bottom":  # the arrow only shows when scrolled up
            problems.append(f"{identifier} not in the tree")
        continue
    if args.expect == "chat-alone" and frame["x"] < hinge - 1:
        problems.append(f"{identifier} starts {hinge - frame['x']:.1f} pt left of the hinge (x={frame['x']:.1f}, hinge {hinge:.1f})")
sidebar = frames.get("chats-sidebar")
if args.expect != "chat-alone":
    pass  # beside a tool the chat shares the leading display with nothing; Chats stays off screen
elif sidebar is None:
    problems.append("chats-sidebar not in the tree")
elif sidebar["x"] + sidebar["width"] > hinge + 1:
    problems.append(f"chats-sidebar ends {sidebar['x'] + sidebar['width'] - hinge:.1f} pt right of the hinge")
# Centre: the title pill and the arrow (when shown) share the composer's centre line within 2 pt,
# chat alone or beside a tool (the composer excludes the side rail, so this is not the display's middle).
composer = frames.get("message-draft")
composer_mid = composer["x"] + composer["width"] / 2 if composer else None
for identifier in ("conversation-title-menu", "scroll-to-bottom"):
    frame = frames.get(identifier)
    if not frame or composer_mid is None:
        continue
    centre = frame["x"] + frame["width"] / 2
    offset = centre - composer_mid
    print(f"measured: {identifier} centre {centre:.1f}, composer centre {composer_mid:.1f}, offset {offset:+.1f}")
    if abs(offset) > 2:
        problems.append(f"{identifier} is {offset:+.1f} pt off the composer's centre (limit 2)")
# Nothing but the chat behind the pill: the pill must lie inside the chat column's top and
# no other element (rail, Chats, tool header) may overlap its frame.
pill = frames.get("conversation-title-menu")
if pill:
    for identifier, frame in frames.items():
        if identifier in ("conversation-title-menu", "project-conversation-header", "chevron.down") or not frame:
            continue
        if identifier.startswith(("message:", "activity-group:", "conversation-scroll", "chevron", "copy-reply", "reply-time")):
            continue
        overlap_x = min(pill["x"] + pill["width"], frame["x"] + frame["width"]) - max(pill["x"], frame["x"])
        overlap_y = min(pill["y"] + pill["height"], frame["y"] + frame["height"]) - max(pill["y"], frame["y"])
        if overlap_x > 1 and overlap_y > 1 and frame["width"] < window["width"] * 0.9:
            problems.append(f"{identifier} overlaps the title pill by {overlap_x:.0f}x{overlap_y:.0f} pt")
# Rail: every rail button shares one x-centre and none overlaps another.
rail = sorted((f for i, f in frames.items() if i in ("open-sidebar", "computer-status-pill", "conversation-simulators-pill", "conversation-files-pill", "conversation-terminal-pill", "conversation-goal-pill") and f), key=lambda f: f["y"])
for first, second in zip(rail, rail[1:]):
    if second["y"] < first["y"] + first["height"] - 1:
        problems.append(f"rail buttons overlap ({first['y']:.0f}+{first['height']:.0f} vs {second['y']:.0f})")
    if abs((first["x"] + first["width"] / 2) - (second["x"] + second["width"] / 2)) > 1:
        problems.append("rail buttons are not on one x-centre")
if problems:
    raise SystemExit(f"{args.capture.name}: " + "; ".join(problems))
print(f"{args.capture.name}: {args.expect}: pill, composer and arrow right of the hinge, Chats left of it ok")
