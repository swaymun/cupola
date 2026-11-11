#!/usr/bin/env python3
"""Audit an unfolded Duo (vertical hinge) chat-alone layout from `wonderctl tree --json` frames.

Usage: check-duo-chat-centre-layout.py TREE.json [--expect chat-alone]
Chats sits on the leading display and the chat alone on the trailing one, so the
title pill, composer and scroll-to-bottom arrow (when shown) lie wholly right of
the hinge (the window's middle) and the Chats sidebar wholly left of it, and the
pill is centred on the trailing display. Accessibility frames only; the matching PNG
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
    if frame["x"] < hinge - 1:
        problems.append(f"{identifier} starts {hinge - frame['x']:.1f} pt left of the hinge (x={frame['x']:.1f}, hinge {hinge:.1f})")
sidebar = frames.get("chats-sidebar")
if sidebar is None:
    problems.append("chats-sidebar not in the tree")
elif sidebar["x"] + sidebar["width"] > hinge + 1:
    problems.append(f"chats-sidebar ends {sidebar['x'] + sidebar['width'] - hinge:.1f} pt right of the hinge")
title = frames.get("conversation-title-menu")
if title:
    centre = title["x"] + title["width"] / 2
    middle = (hinge + window["x"] + window["width"]) / 2
    print(f"measured: pill centre {centre:.1f}, trailing display centre {middle:.1f}, hinge {hinge:.1f}")
if problems:
    raise SystemExit(f"{args.capture.name}: " + "; ".join(problems))
print(f"{args.capture.name}: {args.expect}: pill, composer and arrow right of the hinge, Chats left of it ok")
