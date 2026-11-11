#!/usr/bin/env python3
"""Check separate rail controls in matching actual app pixels and native AX.

Run with an explicit screenshot scale; the Duo's rotated Application AX bounds
can be stale, so clipping is checked against the screenshot, never that wrapper.
This proves geometry, labels, selection and visible gaps, not touch delivery.
"""
import argparse
import json
from pathlib import Path
from PIL import Image

PILL_IDS = {
    "chats": "open-sidebar", "context": "context-usage-ring",
    "computer": "computer-status-pill", "files": "conversation-files-pill",
    "terminal": "conversation-terminal-pill", "simulators": "conversation-simulators-pill", "preview": "preview-pages-menu",
    "attention": "question-dock-open", "goal": "goal-status-pill",
    "agentTasks": "project-subagent-status-pill", "git": "conversation-title-menu",
    "giveControlBack": "computer-session-done", "keyboard": "computer-session-keyboard",
    "escape": "computer-session-key-escape", "tab": "computer-session-key-tab",
    "appSwitcher": "computer-session-shortcut-app-switcher", "clipboard": "computer-session-clipboard",
    "windowsAndApps": "computer-session-windows-and-apps", "takeControl": "computer-session-take-control",
}


def walk(value):
    if isinstance(value, list):
        for child in value:
            yield from walk(child)
    elif isinstance(value, dict):
        yield value
        yield from walk(value.get("children", []))


def measure(shot, ax, scale, expected, selected=None, keyboard=False):
    im = Image.open(shot).convert("RGB")
    width, height = (value / scale for value in im.size)
    controls = {}
    captured = list(walk(json.loads(ax.read_text())))
    viewports = [node["frame"] for node in captured if node.get("AXUniqueId") == "duo-rail-tools" and node.get("frame", {}).get("height", 0) > 0]
    viewport = viewports[0] if viewports else None
    def visible_in_rail(frame):
        if viewport is None:
            return True
        top, bottom = viewport["y"], viewport["y"] + viewport["height"]
        y, end = frame["y"], frame["y"] + frame["height"]
        if end <= top + 0.5 or y >= bottom - 0.5:
            return False
        assert y >= top - 0.5 and end <= bottom + 0.5, "A rail button is partly clipped; size the viewport for complete rows"
        return True
    for node in captured:
        ident, frame = node.get("AXUniqueId"), node.get("frame", {})
        if ident not in PILL_IDS.values():
            continue
        x, y, w, h = (frame.get(key, 0) for key in ("x", "y", "width", "height"))
        if x + w <= 0 or y + h <= 0 or x >= width or y >= height:
            continue
        if ident != PILL_IDS["chats"] and not visible_in_rail(frame):
            continue
        assert ident not in controls, f"{ident}: duplicate visible control; keep one rail owner"
        assert w >= 44 and h >= 44, f"{ident}: {w}x{h}; give each rail button a 44 pt hit target"
        assert x >= 0 and y >= 0 and x + w <= width and y + h <= height, f"{ident}: clipped; keep the rail within the active display"
        assert node.get("AXLabel"), f"{ident}: provide its own accessible label"
        controls[ident] = node
    assert set(controls) == {PILL_IDS[name] for name in expected}, f"Expected {expected}; visible rail controls: {list(controls)}"
    ordered = sorted(controls.values(), key=lambda node: node["frame"]["y"])
    centers = [node["frame"]["x"] + node["frame"]["width"] / 2 for node in ordered]
    assert max(centers) - min(centers) <= 2, "A global control moved to the chat; keep Chats and context on the rail"
    gaps = []
    for first, second in zip(ordered, ordered[1:]):
        a, b = first["frame"], second["frame"]
        gap = b["y"] - a["y"] - a["height"]
        assert gap >= 4, f"{first['AXUniqueId']} / {second['AXUniqueId']}: {gap} pt gap; separate hit targets"
        if first["AXUniqueId"] != PILL_IDS["chats"]:
            assert gap <= 8, f"{first['AXUniqueId']} / {second['AXUniqueId']}: {gap} pt gap; do not reserve a rail slot for an unavailable tool"
        y = round((a["y"] + a["height"] + gap / 2) * scale)
        x = round(centers[0] * scale)
        # Popovers open to the left and cast a shadow there. Compare against
        # the clear outer edge of the rail, away from their content and arrow.
        outside = round((max(a["x"] + a["width"], b["x"] + b["width"]) + 12) * scale)
        assert outside < im.width, "The rail needs a visible outer margin"
        distance = max(abs(c - d) for c, d in zip(im.getpixel((x, y)), im.getpixel((outside, y))))
        assert distance <= 10, f"{shot.name}: {first['AXUniqueId']} / {second['AXUniqueId']}: shared background crosses gap (colour delta {distance}); draw individual surfaces"
        gaps.append({"after": first["AXUniqueId"], "points": gap, "backgroundDelta": distance})
    if keyboard:
        keys = [node for node in captured if node.get("AXLabel") == "q" and node.get("role") == "AXButton"]
        assert keys, "The software keyboard must be visible; focus alone is not evidence"
        top = min(node["frame"]["y"] for node in keys)
        assert all(node["frame"]["y"] + node["frame"]["height"] < top - 4 for node in ordered), "The keyboard covers a rail target; keep the rail controls visible above it"
    selection_delta = None
    if selected:
        node = controls[PILL_IDS[selected]]
        assert (node.get("AXValue") or "").split(",", 1)[0] in {"Open", "Expanded"}, f"{selected}: selected tool must report Open"
        # AXe omits UIKit's selected trait; hosted native tests own that trait.
        # Check the rendered fill/ring against an inactive tool at the same inset.
        others = [other for key, other in controls.items() if key in {PILL_IDS[n] for n in ("computer", "files", "terminal")} and key != PILL_IDS[selected]]
        if others:
            def surface(other):
                frame = other["frame"]
                return im.getpixel((round((frame["x"] + 2) * scale), round((frame["y"] + frame["height"] / 2) * scale)))
            delta = max(abs(a - b) for a, b in zip(surface(node), surface(others[0])))
            selection_delta = delta
            assert delta >= 15, f"{selected}: selected surface is indistinct (colour delta {delta}); show the accent ring and fill"
    return {"capture": str(shot), "accessibility": str(ax), "scale": scale,
            "viewport": viewport, "controls": {key: node["frame"] for key, node in controls.items()}, "gaps": gaps, "selected": selected, "selectionSurfaceDelta": selection_delta, "passed": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("shot", type=Path, nargs="?")
    parser.add_argument("ax", type=Path, nargs="?")
    parser.add_argument("--capture-dir", type=Path, help="Check every matched Duo state from capture-rail.py")
    parser.add_argument("--scale", type=float, required=True)
    parser.add_argument("--expect", help="Comma-separated control names")
    parser.add_argument("--keyboard", action="store_true")
    parser.add_argument("--selected", choices=PILL_IDS)
    args = parser.parse_args()
    if args.capture_dir:
        normal = "chats,context,computer,files,terminal,agentTasks"
        expected = {name: (normal, selected) for name, selected in [
            ("folded", None), ("folded-files", "files"), ("unfolded", None),
            ("unfolded-files", "files"), ("hinge-files", "files")]}
        expected.update({
            "folded-chats": ("chats", "chats"),
            "goal-tasks": (normal + ",goal", None),
            "task-roster": (normal + ",goal", "agentTasks"),
            "keyboard-terminal": ("chats,computer,files,terminal,agentTasks", "terminal"),
            "computer": (normal + ",takeControl", "computer"),
            "computer-control": ("chats,context,computer,giveControlBack,keyboard,escape,tab,appSwitcher,clipboard,windowsAndApps", "computer"),
        })
        rows = json.loads((args.capture_dir / "manifest.json").read_text())
        results = []
        for row in rows:
            name = row["state"]
            names, selected = expected[name]
            results.append(measure(args.capture_dir / (name + ".png"), args.capture_dir / (name + ".json"),
                                   args.scale, names.split(","), selected, name == "keyboard-terminal"))
        assert results, "No captures to check"
        print(json.dumps({"passed": True, "count": len(results), "results": results}, indent=2))
        return
    assert args.shot and args.ax and args.expect, "Supply shot, ax and --expect, or --capture-dir"
    expected = args.expect.split(",")
    assert args.scale > 0 and expected and set(expected) <= PILL_IDS.keys(), "Invalid scale or control names"
    print(json.dumps(measure(args.shot, args.ax, args.scale, expected, args.selected, args.keyboard), indent=2))


if __name__ == "__main__":
    main()
