#!/usr/bin/env python3
"""Capture owned layout fixtures and compare real AX controls with UIKit anchors.

Build this checkout first; the helper installs and verifies its binary. Acquire a pooled lane with wonderctl lane acquire; commands use its device profile. Duo states use launch fixtures, not HID input.
This proves fixture layout, not physical hinge or touch behavior.
"""
import argparse
import hashlib
import json
import os
import plistlib
import struct
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[4]
BASE = ["subagent-fixture", "chat-layout", "chat-layout-unsaved", "project-subagents",
        "project-files-send", "project-speed", "context-usage", "layout-frames", "fresh-agent-defaults"]
STATES = [
    ("new-chat-folded", 0, []),
    ("new-chat-unfolded", 180, []),
    ("new-chat-hinge", 90, []),
    ("new-chat-keyboard", 180, ["composer-keyboard"]),
    ("new-chat-computers", 180, ["new-chat-computers-open"]),
    ("context", 180, ["open-project-thread", "context-usage-open"]),
    ("context-folded", 0, ["open-project-thread", "context-usage-open"]),
    ("context-hinge", 90, ["open-project-thread", "context-usage-open"]),
    ("tasks", 180, ["open-project-thread", "project-claude-tasks", "task-roster-open"]),
    ("tasks-folded", 0, ["open-project-thread", "project-claude-tasks", "task-roster-open"]),
    ("tasks-hinge", 90, ["open-project-thread", "project-claude-tasks", "task-roster-open"]),
    ("settings", 180, ["settings-open"]),
    ("settings-folded", 0, ["settings-open"]),
    ("settings-hinge", 90, ["settings-open"]),
    ("terminal-folded", 0, ["open-project-thread", "terminal-preview", "companion-terminal"]),
    ("terminal-hinge", 90, ["open-project-thread", "terminal-preview", "companion-terminal"]),
    ("terminal-portrait", 180, ["open-project-thread", "terminal-preview", "companion-terminal"]),
    ("keyboard", 180, ["open-project-thread", "terminal-preview", "companion-terminal"]),
    ("computer", 180, ["open-project-thread", "companion-computer", "computer-session-fixture",
                       "computer-session-available-fixture", "computer-menu-fixture"]),
    ("computer-folded", 0, ["open-project-thread", "companion-computer", "computer-session-fixture",
                             "computer-session-available-fixture", "computer-menu-fixture"]),
    ("computer-hinge", 90, ["open-project-thread", "companion-computer", "computer-session-fixture",
                            "computer-session-available-fixture", "computer-menu-fixture"]),
    ("computer-sources", 180, ["open-project-thread", "companion-computer", "computer-session-fixture",
                               "computer-session-available-fixture", "computer-menu-fixture", "computer-sources-open"]),
]


def nodes(tree):
    for node in tree:
        yield node
        yield from nodes(node.get("children", []))


def frame(tree, identity):
    match = next((n for n in nodes(tree) if n.get("AXUniqueId") == identity), None)
    if match is None:
        raise RuntimeError(identity + " is absent from the native tree; select the fixture that makes this control visible")
    return match["frame"]


def new_chat_checks(tree, state, *, large_text=False):
    """Native frames own touch targets, wrapping and physical fold clearance."""
    identities = ["connection-picker", "destination-picker"]
    controls = {identity: frame(tree, identity) for identity in identities}
    division = state.get("displayDivision")
    window = state["layout"]["window"]
    for identity, rect in controls.items():
        assert rect["x"] >= 0 and rect["y"] >= 0 and rect["x"] + rect["width"] <= window["width"] + 0.5 and rect["y"] + rect["height"] <= window["height"] + 0.5, (identity, "off-screen pill", rect, window)
        assert min(rect["width"], rect["height"]) >= 44 - 0.5, (identity, "44 pt target", rect)
        if division:
            overlap = (min(rect["x"] + rect["width"], division["x"] + division["width"]) > max(rect["x"], division["x"]) + 0.5
                       and min(rect["y"] + rect["height"], division["y"] + division["height"]) > max(rect["y"], division["y"]) + 0.5)
            assert not overlap, (identity, "control crosses display fold", rect, division)
    rows = []
    for rect in controls.values():
        if not any(abs(rect["y"] - y) < 2 for y in rows): rows.append(rect["y"])
    assert large_text or len(rows) <= 2, ("setup pills exceed two rows", controls)
    assert not any(n.get("AXUniqueId") == "new-chat-title" for n in nodes(tree)), "Remove the redundant New chat heading"
    return {"pillFrames": controls, "pillRows": len(rows), "displayDivision": division,
            "hinge": state.get("hinge"), "displayDivisionActive": state.get("displayDivisionActive")}


def anchor_checks(tree, state, name, screenshot):
    identity = {"context": "context-usage-ring", "context-folded": "context-usage-ring", "context-hinge": "context-usage-ring",
                "tasks": "project-subagent-status-pill", "tasks-folded": "project-subagent-status-pill", "tasks-hinge": "project-subagent-status-pill",
                "new-chat-computers": "connection-picker",
                "computer-sources": "computer-session-connection-picker"}.get(name)
    if not identity:
        return {}
    control = frame(tree, identity)
    panels = state["layout"]["popovers"]
    assert len(panels) == 1, panels
    panel = panels[0]
    source, body = panel["source"], panel["body"]
    center_error = max(abs(control[axis] + control[dimension] / 2 - source[axis] - source[dimension] / 2)
                       for axis, dimension in [("x", "width"), ("y", "height")])
    assert center_error <= 1, (identity, control, source)
    # This is the hosted content frame, including neither the native content
    # inset nor the arrow. Measure the actual rendered arrow separately.
    direction = panel["arrowDirection"]
    gap = {1: body["y"] - control["y"] - control["height"],
           2: control["y"] - body["y"] - body["height"],
           4: body["x"] - control["x"] - control["width"],
           8: control["x"] - body["x"] - body["width"]}.get(direction)
    assert gap is not None and gap >= -1, panel
    checker_source = Path(__file__).with_name("popover-frame-check.swift")
    checker = ROOT / ".local/control/layout-tools" / state.get("udid", "pooled") / "popover-frame-check"
    if not checker.exists() or checker.stat().st_mtime < checker_source.stat().st_mtime:
        checker.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["xcrun", "swiftc", "-O", str(checker_source), "-o", str(checker)], check=True)
    window = state["layout"]["window"]
    values = [window["width"], window["height"]]
    for value in [body, control, source]:
        values += [value[key] for key in ["x", "y", "width", "height"]]
    values += [direction]
    pixels = json.loads(subprocess.run([str(checker), str(screenshot), *map(str, values)],
                                     check=True, capture_output=True, text=True, timeout=30).stdout)
    assert pixels["arrowFillDistance"] <= 24, (identity, pixels, control, panel)
    return {"control": identity, "controlFrame": control, "anchorCenterError": center_error,
            "contentControlGap": gap, "arrowDirection": direction, **pixels}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--device", choices=["duo", "iphone", "ipad"], required=True)
    parser.add_argument("--state", action="append")
    parser.add_argument("--orientation", choices=["portrait", "landscape-left", "landscape-right"], default="portrait")
    parser.add_argument("--dark", action="store_true")
    parser.add_argument("--flipped", action="store_true", help="Match the owner frame with the terminal on the leading display")
    parser.add_argument("--baseline", action="store_true", help="Record known before-layout failures without stopping later captures")
    parser.add_argument("--live-terminal-posture", action="store_true", help="Keep a real terminal open through wide, folded and tall postures")
    parser.add_argument("--live-posture", action="store_true", help="Keep context open while changing the Duo hinge; do not relaunch")
    parser.add_argument("--large-text", action="store_true", help="Capture at AX3 Dynamic Type")
    parser.add_argument("--close-edge", action="store_true", help="Natively tap the Close target below the visible capsule")
    args = parser.parse_args()
    if args.state:
        unknown = set(args.state) - {name for name, _, _ in STATES}
        if unknown:
            parser.error("Unknown capture states: " + ", ".join(sorted(unknown)))
        if args.device != "duo" and any(name.endswith(("-folded", "-hinge")) for name in args.state):
            parser.error("Folded/hinge states need --device duo; use --state terminal-portrait for an iPhone/iPad terminal sheet")
    if (args.live_posture or args.live_terminal_posture) and args.device != "duo":
        parser.error("--live-posture requires --device duo")
    args.output.mkdir(parents=True, exist_ok=True)

    def run(*parts):
        result = subprocess.run([str(ROOT / "scripts/wonderctl"), "--device", args.device, *parts],
                                cwd=ROOT, text=True, capture_output=True, timeout=120)
        if result.returncode:
            raise RuntimeError(" ".join(parts) + "\n" + result.stdout + result.stderr)
        return result.stdout

    lane = json.loads(run("lane", "status", "--kind", args.device))
    if not lane.get("held"):
        parser.error("Acquire a pooled lane first: scripts/wonderctl lane acquire --kind " + args.device)
    udid = run("device").strip().splitlines()[-1]
    derived = Path(os.environ.get("WONDER_CONTROL_DERIVED", ROOT / ".local/build/control-sim"))
    app = derived / "Build/Products/Diagnostics-iphonesimulator/Wonder.app"
    binary = app / "Wonder.debug.dylib"
    if not binary.exists():
        binary = app / "Wonder"
    with (app / "Info.plist").open("rb") as handle:
        info = plistlib.load(handle)
    metadata = {"device": args.device, "udid": udid, "orientation": args.orientation,
                "theme": "cupola-dark" if args.dark else "cupola-light", "dynamicType": "AX3" if args.large_text else "Large", "flipped": args.flipped,
                "sourceHead": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "sourceDirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip()),
                "appVersion": info.get("CFBundleShortVersionString"), "appBuild": info.get("CFBundleVersion"),
                "binarySha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "states": {}}
    provenance_file = derived / "capture-provenance.json"
    if provenance_file.exists():
        provenance = json.loads(provenance_file.read_text())
        assert provenance["binarySha256"] == metadata["binarySha256"], "Re-freeze the app with matching source provenance"
        metadata.update(captureToolHead=metadata["sourceHead"], captureToolDirty=metadata["sourceDirty"],
                        sourceHead=provenance["sourceHead"], sourceDirty=provenance["sourceDirty"])
    # Install the selected bundle on the leased clone and verify its binary.
    # Baselines use a separate derived-data directory and source receipt.
    run("install")
    installed = Path(subprocess.check_output(["xcrun", "simctl", "get_app_container", udid,
                                            info["CFBundleIdentifier"], "app"], text=True).strip()) / binary.name
    metadata["installedBinarySha256"] = hashlib.sha256(installed.read_bytes()).hexdigest()
    assert metadata["installedBinarySha256"] == metadata["binarySha256"], "Install this checkout's binary before claiming capture evidence"
    if args.device == "duo":
        # Installing while folded can leave the app on the inactive inner
        # display. Match wonderctl preview's warmup: foreground Cupola while
        # open before moving it onto the cover, then prove the native pose.
        run("hinge", "180")
        run("orient", "portrait")
        warmup = ["--driver", "launch"]
        for fixture in BASE:
            warmup += ["--fixture", fixture]
        run(*warmup)
        run("wait", "--id", "new-chat-draft", "--timeout", "30")
        time.sleep(0.5)
    previous_file = args.output / "measurements.json"
    if previous_file.exists():
        previous = json.loads(previous_file.read_text())
        if previous["binarySha256"] != metadata["binarySha256"]:
            raise RuntimeError("Choose a new output directory for this different app binary")
        if previous.get("orientation", "portrait") != args.orientation or previous["device"] != args.device:
            raise RuntimeError("Choose a new output directory for this device/orientation")
        metadata["states"] = previous["states"]
    for name, angle, fixtures in STATES:
        if args.live_posture and name != "context":
            continue
        if args.live_terminal_posture and name != "terminal-hinge":
            continue
        if args.state and name not in args.state:
            continue
        if args.device != "duo" and angle != 180:
            continue
        print(name, flush=True)
        if args.device == "duo":
            run("hinge", str(angle)); run("orient", args.orientation)
        launch = ["--driver", "launch"]
        selected = BASE + fixtures
        if name.startswith("tasks") and args.device == "duo":
            selected += ["task-roster-in-bar"]
        for fixture in selected:
            launch += ["--fixture", fixture]
        values = ["-wonder.sidebar.hidden", "YES", "-wonder.companionPane.flipped", "YES" if args.flipped else "NO",
                  "-diagnostics-theme", "cupola-dark" if args.dark else "cupola-light",
                  "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryAccessibilityXL" if args.large_text else "UICTContentSizeCategoryL"]
        if name.startswith("computer"):
            values += ["-composerPills", "files,-computer"]  # Its hidden pill makes the actual Close control visible.
        launch += ["--arg=" + value for value in values]
        run(*launch)
        if args.device == "duo":
            deadline = time.monotonic() + 20
            expected_wide = bool(angle) == (args.orientation == "portrait")
            while True:
                pose = json.loads(run("--driver", "state"))
                window = pose.get("layout", {}).get("window", {})
                if window and (window["width"] > window["height"]) == expected_wide:
                    break
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"{name}: Cupola is on the wrong display/pose: {window}; open and foreground the app before folding")
                time.sleep(0.25)
        state = json.loads(run("--driver", "state"))
        if state.get("alerts") == ["Unlock this device to open saved notification settings."]:
            run("--driver", "tap", "--id", "notification-error-dismiss")
        target = ("connection-options" if name == "new-chat-computers" else "new-chat-draft" if name.startswith("new-chat") else "settings-add-computer" if name.startswith("settings")
                  else "context-usage-window" if name.startswith("context") else "project-subagent-roster-list" if name.startswith("tasks")
                  else "terminal-output" if name in {"keyboard", "terminal-portrait", "terminal-folded", "terminal-hinge"} else "computer-session-connection-picker")
        run("wait", "--id", target, "--timeout", "30")
        if name in {"keyboard", "new-chat-keyboard", "terminal-portrait", "terminal-folded", "terminal-hinge"}:
            deadline = time.monotonic() + 10
            while True:
                keyboard_tree = json.loads(run("tree", "--json"))
                labels = {n.get("AXLabel", "") for n in nodes(keyboard_tree)}
                if any(isinstance(label, str) and "Speed up your typing by sliding" in label for label in labels):
                    if args.device == "duo":
                        raise RuntimeError("Apple's typing introduction covers the keyboard. Set DidShowContinuousPathIntroduction on this leased clone, then recapture; Duo native taps are unavailable")
                    if time.monotonic() >= deadline:
                        raise RuntimeError("Typing introduction did not dismiss on the owned simulator; inspect its native capture before retrying")
                    run("tap", "--label", "Continue")
                    continue
                if any(isinstance(label, str) and label.lower() == "q" for label in labels):
                    break
                elif time.monotonic() >= deadline:
                    raise RuntimeError("A focused editor is not keyboard evidence: show the software keyboard and recapture")
                elif "Continue" in labels:
                    run("tap", "--label", "Continue")  # The fresh simulator's typing introduction.
                time.sleep(0.25)
        time.sleep(1)
        state = json.loads(run("--driver", "state"))
        tree = json.loads(run("tree", "--json"))
        (args.output / (name + ".state.json")).write_text(json.dumps(state, indent=2) + "\n")
        (args.output / (name + ".tree.json")).write_text(json.dumps(tree, indent=2) + "\n")
        command = ["xcrun", "simctl", "io", udid, "screenshot"]
        if args.device == "duo":
            command += ["--display=" + ("primary-1" if angle else "primary")]
        subprocess.run(command + [str(args.output / (name + ".png"))], check=True, capture_output=True)
        if args.device == "duo":
            width, height = struct.unpack(">II", (args.output / (name + ".png")).read_bytes()[16:24])
            window = state["layout"]["window"]
            assert abs(width / height - window["width"] / window["height"]) < 0.01, (
                name, "screenshot is not the active Cupola display", (width, height), window)
        metadata["states"][name] = anchor_checks(tree, state, name, args.output / (name + ".png"))
        if name.startswith("new-chat"):
            if args.device == "duo" and angle and not args.baseline:
                assert state.get("hinge") == ("partiallyOpen" if angle == 90 else "fullyOpen"), (name, "native hinge telemetry", state.get("hinge"))
                assert state.get("displayDivisionActive") == (angle == 90), (name, "native division activity", state.get("displayDivisionActive"))
            try:
                metadata["states"][name].update(new_chat_checks(tree, state, large_text=args.large_text))
            except AssertionError as error:
                if not args.baseline: raise
                metadata["states"][name]["baselineFailure"] = str(error)
            composer = frame(tree, "new-chat-composer")
            metadata["states"][name]["composerFrame"] = composer
            if args.device == "duo" and name != "new-chat-keyboard" and "window" in state["layout"]:
                window = state["layout"]["window"]
                gap = window["height"] - state["layout"]["safeAreaBottom"] - composer["y"] - composer["height"]
                # The bottom-anchored Duo composer intentionally reclaims 14 pt
                # of the home-indicator safe area; it must still leave 8 pt.
                assert -14 <= gap <= 12, (name, composer, window, gap)
                assert window["height"] - composer["y"] - composer["height"] >= 8, (name, composer, window)
                # New chat has no heading of its own (owner feedback 3, 40).
                metadata["states"][name].update(bottomSafeAreaGap=gap)
        if name in {"keyboard", "terminal-portrait", "terminal-folded", "terminal-hinge"} and not args.baseline:
            window = state["layout"]["window"]
            expected = "regular" if window["width"] >= window["height"] * 1.2 else "compact"
            assert state.get("toolLayout") == expected, (name, "keyboard changed tool presentation", state.get("toolLayout"), window)
            if expected == "compact":
                assert "terminal" in state.get("sheets", []), (name, "portrait terminal must remain in its sheet", state.get("sheets"))
                assert state.get("pane") is None, (name, state.get("pane"))
            else:
                assert state.get("pane", {}).get("tool") == "terminal", (name, state.get("pane"))
                pane = frame(tree, "companion-pane")
                last_key = frame(tree, "terminal-key-symbols")
                right_inset = pane["x"] + pane["width"] - last_key["x"] - last_key["width"]
                assert 0 <= right_inset <= 10, (name, "terminal reserves the window rail twice; its slot already excludes the rail", pane, last_key, right_inset)
                metadata["states"][name]["terminalKeyRightInset"] = right_inset
            metadata["states"][name].update(toolLayout=expected, toolWindow=state.get("toolWindow"), keyboardVisible=True)
        if name in {"computer", "computer-folded", "computer-hinge"}:
            close = frame(tree, "computer-session-close")
            assert min(close["width"], close["height"]) >= 44, close
            metadata["states"][name]["closeFrame"] = close
        if args.close_edge and name == "computer":
            point = (close["x"] + close["width"] / 2, close["y"] + close["height"] - 1)
            run("tap", "--xy", str(point[0]) + "," + str(point[1]))
            run("wait", "--id", "computer-session-close", "--gone", "--timeout", "10")
            metadata["states"][name]["closeEdgeTap"] = {"point": point, "closed": True}
            after = json.loads(run("--driver", "state"))
            assert after.get("pane") is None, after.get("pane")
            (args.output / "computer-edge-dismissed.state.json").write_text(json.dumps(after, indent=2) + "\n")
            subprocess.run(["xcrun", "simctl", "io", udid, "screenshot", str(args.output / "computer-edge-dismissed.png")],
                           check=True, capture_output=True)
        (args.output / "measurements.json").write_text(json.dumps(metadata, indent=2) + "\n")

    if args.live_terminal_posture:
        for name, angle, orientation, mode in [
            ("terminal-live-wide", 180, "portrait", "regular"),
            ("terminal-live-folded", 0, "portrait", "compact"),
            ("terminal-live-tall", 90, "landscape-left", "compact")]:
            print(name, flush=True)
            run("hinge", str(angle)); run("orient", orientation)
            time.sleep(2)
            run("wait", "--id", "terminal-output", "--timeout", "15")
            state = json.loads(run("--driver", "state"))
            tree = json.loads(run("tree", "--json"))
            (args.output / (name + ".state.json")).write_text(json.dumps(state, indent=2) + "\n")
            (args.output / (name + ".tree.json")).write_text(json.dumps(tree, indent=2) + "\n")
            subprocess.run(["xcrun", "simctl", "io", udid, "screenshot",
                            "--display=" + ("primary-1" if angle else "primary"), str(args.output / (name + ".png"))], check=True, capture_output=True)
            assert state.get("toolLayout") == mode, (name, state.get("toolLayout"), state.get("toolWindow"))
            assert (state.get("pane") or {}).get("tool") == "terminal" if mode == "regular" else "terminal" in state.get("sheets", []), (name, state.get("pane"), state.get("sheets"))
            metadata["states"][name] = {"toolLayout": mode, "toolWindow": state.get("toolWindow"), "hinge": state.get("hinge")}
            (args.output / "measurements.json").write_text(json.dumps(metadata, indent=2) + "\n")

    if args.live_posture:
        failures = []
        # The context fixture was opened once above. No launch, driver action or
        # binding rewrite follows: the same real presentation sees each posture.
        for name, angle in [("context-live-hinge", 90), ("context-live-folded", 0), ("context-live-unfolded", 180)]:
            print(name, flush=True)
            run("hinge", str(angle))
            time.sleep(2)
            state = json.loads(run("--driver", "state"))
            tree = json.loads(run("tree", "--json"))
            screenshot = args.output / (name + ".png")
            (args.output / (name + ".state.json")).write_text(json.dumps(state, indent=2) + "\n")
            (args.output / (name + ".tree.json")).write_text(json.dumps(tree, indent=2) + "\n")
            subprocess.run(["xcrun", "simctl", "io", udid, "screenshot",
                            "--display=" + ("primary-1" if angle else "primary"), str(screenshot)],
                           check=True, capture_output=True)
            try:
                metadata["states"][name] = anchor_checks(tree, state, "context", screenshot)
            except (AssertionError, RuntimeError, KeyError) as error:
                failures.append(name + ": " + str(error))
                metadata["states"][name] = {"error": str(error), "popoverCount": len(state.get("layout", {}).get("popovers", []))}
            (args.output / "measurements.json").write_text(json.dumps(metadata, indent=2) + "\n")
        if failures:
            raise RuntimeError("Live posture acceptance failed; inspect captures: " + "; ".join(failures))


if __name__ == "__main__":
    main()
