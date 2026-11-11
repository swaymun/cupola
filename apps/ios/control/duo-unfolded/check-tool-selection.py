#!/usr/bin/env python3
"""Check native pill values in a captured AX tree, independent of stale root bounds.

Capture with wonderctl --device duo tree --json; inspect the matching inner PNG
for the fill. This checks accessible state, not rendering or tap delivery.
"""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("capture", type=Path)
parser.add_argument("--selected", choices=["computer", "files", "terminal"], required=True)
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


identifiers = {"computer": "computer-status-pill", "files": "conversation-files-pill",
               "terminal": "conversation-terminal-pill"}
captured = list(nodes(tree))
for tool, identifier in identifiers.items():
    matches = [node for node in captured if node.get("AXUniqueId") == identifier]
    if len(matches) != 1:
        raise SystemExit(f"{identifier}: expected one native pill, found {len(matches)}; capture the active conversation")
    value = matches[0].get("AXValue") or ""
    expected = "Open" if tool == args.selected else "Closed"
    if value.split(",", 1)[0] != expected:
        raise SystemExit(f"{identifier}: expected {expected}, got {value!r}; pass the shared tool selection into the pill")
    print(f"{identifier}: {value}")
