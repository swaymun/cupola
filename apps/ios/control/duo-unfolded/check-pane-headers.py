#!/usr/bin/env python3
"""Check U7 against matched actual-app native AX, inner PNGs and driver state.

This proves visible geometry and retained routes, not native Duo touch delivery.
Run capture-rail.py for all pane-<tool>[-expanded] states first.
"""
import argparse
import json
from pathlib import Path
import struct
from statistics import median
from PIL import Image

TOOLS = {"agent-task": "project-subagent-status-pill",
         "details": "thread-details-done", "reply-changes": "reply-changes-done"}


def walk(value):
    if isinstance(value, list):
        for child in value:
            yield from walk(child)
    elif isinstance(value, dict):
        yield value
        yield from walk(value.get("children", []))


def check(entry, scale):
    image = Path(entry["image"])
    pixels = Image.open(image).convert("RGB")
    width, height = (x / scale for x in struct.unpack(">II", image.read_bytes()[16:24]))
    nodes = list(walk(json.loads(Path(entry["tree"]).read_text())))

    def visible(node):
        f = node.get("frame", {})
        return (f.get("width", 0) > 0 and f.get("height", 0) > 0
                and f.get("x", 0) < width and f.get("y", 0) < height
                and f.get("x", 0) + f.get("width", 0) > 0
                and f.get("y", 0) + f.get("height", 0) > 0)

    def one(identifier):
        found = [n for n in nodes if n.get("AXUniqueId") == identifier and visible(n)]
        assert len(found) == 1, f"{entry['state']}: {identifier}: expected one visible native control, got {len(found)}"
        frame = found[0]["frame"]
        assert frame["x"] >= 0 and frame["y"] >= 0, f"{identifier}: clipped at the leading/top edge"
        assert frame["x"] + frame["width"] <= width + 1 and frame["y"] + frame["height"] <= height + 1, f"{identifier}: clipped at the trailing/bottom edge"
        return frame

    tool = entry["state"][len("pane-"):].removesuffix("-expanded")
    assert entry["semanticState"].get("pane") == {"tool": tool, "expanded": False}, "A restored expanded preference must still occupy only one Duo display"
    chats, context = one("open-sidebar"), one("context-usage-ring")
    assert abs(chats["x"] + chats["width"] / 2 - context["x"] - context["width"] / 2) <= 2, "Keep Chats and context together in the vertical rail"
    assert chats["width"] <= 64, "Chats became a cross-pane row instead of a rail button"
    header = one("companion-pane-header")
    pane = one("companion-pane")
    pane_label = next(n.get("AXLabel") for n in nodes if n.get("AXUniqueId") == "companion-pane" and visible(n))
    # At accessibility sizes the PR filter becomes a menu whose label is also
    # "Pull requests". Count title text in the header band, not in page content.
    titles = [n for n in nodes if n.get("AXLabel") == pane_label
              and n.get("role") in {"AXStaticText", "AXHeading"} and visible(n)
              and n["frame"]["y"] < header["y"] + header["height"]
              and n["frame"]["y"] + n["frame"]["height"] > header["y"]]
    assert len(titles) == 1, "Replace the native title with one inline title"
    for identifier in ["companion-pane-flip", TOOLS[tool]]:
        frame = one(identifier)
        assert frame["width"] >= 44 and frame["height"] >= 44, f"{identifier}: preserve its accessible hit target"
        if identifier == TOOLS[tool] and tool in {"agent-task", "pull-requests"}:
            # These panes close through their selected rail button. Size and
            # bounds still apply; header containment belongs to header actions.
            continue
        # A safe-area inset's page-colored background can span the rail. Its
        # actions must stay inside the actual pane, whose AX frame excludes it.
        assert frame["x"] >= pane["x"] - 1 and frame["x"] + frame["width"] <= pane["x"] + pane["width"] + 1, f"{identifier}: keep the action clear of the rail"
        assert frame["y"] >= header["y"] - 1 and frame["y"] + frame["height"] <= header["y"] + header["height"] + 1, f"{identifier}: outside its inline header"
    assert not any(n.get("AXUniqueId") == "companion-pane-expand" and visible(n) for n in nodes), "Duo has no spread-wide expansion target"
    close = one(TOOLS[tool])
    left, top, right, bottom = [round(v * scale) for v in
        (close["x"], close["y"], close["x"] + close["width"], close["y"] + close["height"])]
    corners = [pixels.getpixel(p) for p in [(left + 1, top + 1), (right - 2, top + 1),
                                            (left + 1, bottom - 2), (right - 2, bottom - 2)]]
    background = [median(c[channel] for c in corners) for channel in range(3)]
    ink = sum(max(abs(pixel[c] - background[c]) for c in range(3)) > 48
              for y in range(top, bottom) for x in range(left, right)
              for pixel in [pixels.getpixel((x, y))])
    if tool in {"agent-task", "pull-requests"}:
        redundant = "project-subagent-sheet-done" if tool == "agent-task" else "pull-requests-done"
        assert not any(n.get("AXUniqueId") == redundant and visible(n) for n in nodes), "The selected pill closes its pane; remove redundant Done"
    assert ink >= 20, "Close must render in its actual native frame, not merely appear in AX"
    assert not any(n.get("AXUniqueId") == "compact-ipad-chats" and visible(n) for n in nodes), "The compact-iPad Chats row must not appear on the Duo"
    return {"state": entry["state"], "chats": chats, "context": context, "header": header,
            "pane": pane, "closeInkPixels": ink, "passed": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--scale", type=float, required=True)
    args = parser.parse_args()
    entries = [e for e in json.loads((args.directory / "manifest.json").read_text()) if e["state"].startswith("pane-")]
    expected = {"pane-" + tool + suffix for tool in TOOLS for suffix in ["", "-expanded"]}
    assert {e["state"] for e in entries} == expected, "Capture all four pane owners with ordinary and restored expanded preferences"
    results = [check(entry, args.scale) for entry in entries]
    print(json.dumps({"passed": len(results), "states": results}, indent=2))


if __name__ == "__main__":
    main()
