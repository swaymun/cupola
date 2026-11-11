#!/usr/bin/env python3
"""Capture matched Duo rail launch fixtures without relying on Duo HID (#387).

Run under this checkout's wonderctl lease after building/installing Diagnostics.
Native trees and screenshots prove layout/accessibility, not touch delivery.
"""
import argparse
import json
import hashlib
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[4]
CTL = ROOT / "scripts/wonderctl"
SIMCTL = "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/Versions/A/Resources/bin/simctl"
BASE = ["subagent-fixture", "chat-layout", "chat-layout-unsaved", "project-subagents",
        "terminal-available", "project-files-send", "open-project-thread", "rail-fixture"]
PROFILE = "duo"
STATES = [
    ("folded", 0, []),
    ("folded-chats", 0, ["sidebar-open"]),
    ("folded-files", 0, ["companion-files"]),
    ("unfolded", 180, []),
    ("unfolded-files", 180, ["companion-files"]),
    ("goal-tasks", 180, ["goal-fixture", "context-usage"]),
    ("task-roster", 180, ["goal-fixture", "context-usage", "task-roster-open", "task-roster-in-bar"]),
    ("hinge-files", 90, ["companion-files"]),
    ("keyboard-terminal", 180, ["terminal-preview", "companion-terminal"]),
    ("terminal", 180, ["terminal-preview", "companion-terminal"]),
    ("computer", 180, ["companion-computer", "computer-session-fixture", "computer-session-available-fixture",
                         "computer-menu-fixture", "computer-simulator-fixture"]),
    ("computer-control", 180, ["companion-computer", "computer-session-fixture", "computer-session-available-fixture",
                                 "computer-control-active", "computer-menu-fixture"]),
]
COMMON_STATES = [("chat", None, []), ("chats", None, ["sidebar-open"]),
                 ("files", None, ["companion-files"])]

# U7: the remaining pane owners and their full-window state. These select
# production routes at launch while Duo native input is unqualified (#387).
PANE_TARGETS = {"pull-requests": "pull-requests", "agent-task": "project-subagent-transcript",
                "details": "thread-details-rename", "reply-changes": "reply-changes"}
for tool in PANE_TARGETS:
    COMMON_STATES.append(("pane-" + tool, None, ["project-pull-requests"] if tool == "pull-requests" else []))
    for expanded in (False, True):
        STATES.append(("pane-" + tool + ("-expanded" if expanded else ""), 180,
                       ["project-pull-requests"] if tool == "pull-requests" else []))



def run(*args):
    try:
        return subprocess.run([str(CTL), "--device", PROFILE, *args], cwd=ROOT,
                              text=True, capture_output=True, check=True).stdout
    except subprocess.CalledProcessError as error:
        error.add_note(error.stderr or error.stdout)
        raise


def dismiss_unsigned_notification_alert():
    state = json.loads(run("--driver", "state"))
    if state.get("alerts") == ["Unlock this device to open saved notification settings."]:
        run("--driver", "tap", "--id", "notification-error-dismiss")
        run("wait", "--id", "notification-error-dismiss", "--gone", "--timeout", "10")
    elif state.get("alerts"):
        raise RuntimeError("Unexpected presentation alert: " + repr(state["alerts"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    global PROFILE
    parser.add_argument("output", type=Path)
    parser.add_argument("--device", choices=["duo", "iphone", "ipad"], default="duo")
    parser.add_argument("--fixture", action="append", default=[], help="Extra owned launch fixture for every state")
    parser.add_argument("--source-head", help="Recorded source revision for a retained baseline build")
    parser.add_argument("--large-text", action="store_true", help="Use accessibility XXXL text")
    parser.add_argument("--dark", action="store_true", help="Use Cupola Dark")
    parser.add_argument("--defer-roster", action="store_true", help="Open the roster after unsigned-build alerts are dismissed")
    parser.add_argument("--check-toggles", action="store_true", help="After goal/task captures, exercise two real presentation cycles per control")
    parser.add_argument("--keep-posture", action="store_true", help="Keep last posture for native keyboard/menu capture")
    parser.add_argument("--state", choices=[s[0] for s in STATES + COMMON_STATES], action="append")
    args = parser.parse_args()
    PROFILE = args.device
    args.output.mkdir(parents=True, exist_ok=True)
    run("device")
    device = json.loads(run("doctor"))["device"]["udid"]
    manifest = args.output / "manifest.json"
    results = json.loads(manifest.read_text()) if manifest.exists() else []
    # Direct launch/tree commands do not install a checkout's app. Install under
    # wonderctl's lease/device lock, then prove the exact installed executable.
    run("install")
    derived = Path(os.environ.get("WONDER_CONTROL_DERIVED", ROOT / ".local/build/control-sim"))
    binary = derived / "Build/Products/Diagnostics-iphonesimulator/Wonder.app/Wonder"
    metadata = {"sourceHead": args.source_head or subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "appSha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "device": device, "deviceProfile": PROFILE,
                "evidenceKind": "simulator launch fixtures; no native Duo tap or physical input claim"}
    diff = subprocess.check_output(["git", "diff", "HEAD", "--", "apps/ios"], cwd=ROOT)
    metadata["sourceDirty"] = bool(diff)
    metadata["sourceDiffSha256"] = hashlib.sha256(diff).hexdigest()
    (args.output / "source.diff").write_bytes(diff)
    app = Path(subprocess.check_output([SIMCTL, "get_app_container", device, "com.swaymun.wonder", "app"], text=True).strip())
    installed_hash = hashlib.sha256((app / "Wonder").read_bytes()).hexdigest()
    if installed_hash != metadata["appSha256"]:
        raise RuntimeError("Installed app differs from the selected build; install this checkout before capturing")
    metadata["installedAppSha256"] = installed_hash
    metadata_file = args.output / "metadata.json"
    if metadata_file.exists() and results:
        previous = json.loads(metadata_file.read_text())
        if previous.get("appSha256") != metadata["appSha256"]:
            raise RuntimeError("This capture directory belongs to another app build; choose a new output directory")
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    try:
        for name, hinge, fixtures in STATES if PROFILE == "duo" else COMMON_STATES:
            if args.state and name not in args.state:
                continue
            print(name, flush=True)
            if PROFILE == "duo":
                run("hinge", str(hinge))
                run("orient", "portrait")
            launch = ["--driver", "launch"]
            defer_roster = name == "task-roster" and args.defer_roster
            launch_fixtures = [fixture for fixture in fixtures if not defer_roster or not fixture.startswith("task-roster-")]
            pane_tool = next((tool for tool in PANE_TARGETS if name in {"pane-" + tool, "pane-" + tool + "-expanded"}), None)
            base = BASE
            for fixture in base + args.fixture + launch_fixtures:
                launch += ["--fixture", fixture]
            if pane_tool:
                if pane_tool == "reply-changes":
                    # Existing short coding snapshot has real saved edits and
                    # runs inside AppShell, unlike the older read-preview root.
                    launch.append("--arg=-diagnostics-app-store")
                launch.append("--arg=-diagnostics-companion-" + pane_tool)
                if name.endswith("-expanded"):
                    launch.append("--arg=-diagnostics-companion-expanded")
            for argument in ["-composerPills", "computer,files,terminal,preview,attention,goal,agentTasks,git",
                             "-wonder.sidebar.hidden", "NO" if PROFILE == "ipad" and name == "chats" else "YES", "-wonder.companionPane.flipped", "NO",
                             "-wonder.companionPane.fraction.portrait", "0.5",
                             "-wonder.companionPane.fraction.landscape", "0.5",
                             "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryAccessibilityXXXL" if args.large_text else "UICTContentSizeCategoryL",
                             "-diagnostics-theme", "cupola-dark" if args.dark else "cupola-light", "-AppleInterfaceStyle", "Dark" if args.dark else "Light"]:
                launch.append("--arg=" + argument)
            run(*launch)
            # Unsigned simulator builds cannot read notification Keychain state.
            # Dismiss only that known environment alert; never capture behind it.
            state = json.loads(run("--driver", "state"))
            alerts = state.get("alerts", [])
            if alerts == ["Unlock this device to open saved notification settings."]:
                run("--driver", "tap", "--id", "notification-error-dismiss")
            elif alerts:
                raise RuntimeError("Unexpected launch alert: " + repr(alerts))
            target = PANE_TARGETS[pane_tool] if pane_tool else "sidebar-cupola" if name in {"folded-chats", "chats"} else "terminal-output" if name == "keyboard-terminal" else "project-subagent-roster-list" if name == "task-roster" and not defer_roster else "message-draft"
            run("wait", "--id", target, "--timeout", "30")
            if pane_tool and PROFILE == "duo":
                run("wait", "--id", "companion-pane-header", "--timeout", "10")
                run("wait", "--id", "companion-pane-expand", "--timeout", "10")
            if "companion-files" in fixtures:
                run("wait", "--id", "workspace-file-list", "--timeout", "20")
            if name == "terminal":
                run("wait", "--id", "terminal-output", "--timeout", "20")
            if name == "keyboard-terminal":
                try:
                    run("wait", "--label", "q", "--timeout", "10")
                except subprocess.CalledProcessError as error:
                    raise RuntimeError("Show Device Hub > Device > Keyboard > Toggle Software Keyboard, then recapture keyboard-terminal; focus alone is not keyboard evidence") from error
            time.sleep(1)  # Finish the launch transition before the matched capture.
            state = json.loads(run("--driver", "state"))
            if state.get("alerts") == ["Unlock this device to open saved notification settings."]:
                run("--driver", "tap", "--id", "notification-error-dismiss")
                run("wait", "--id", "notification-error-dismiss", "--gone", "--timeout", "10")
                state = json.loads(run("--driver", "state"))
            if state.get("alerts"):
                raise RuntimeError("Do not capture behind an alert: " + repr(state["alerts"]))
            run("wait", "--id", "notification-error-dismiss", "--gone", "--timeout", "10")
            if defer_roster:
                run("--driver", "tap", "--id", "project-subagent-status-pill")
                run("wait", "--id", "project-subagent-roster-list", "--timeout", "20")
                state = json.loads(run("--driver", "state"))
            (args.output / (name + ".json")).write_text(run("tree", "--json"))
            image = args.output / (name + ".png")
            subprocess.run([SIMCTL, "io", device, "screenshot", "--display=" + ("1" if PROFILE != "duo" else "primary" if hinge == 0 else "primary-1"), str(image)], check=True, capture_output=True)
            results = [result for result in results if result["state"] != name]
            results.append({"state": name, "hinge": hinge, "fixtures": base + args.fixture + launch_fixtures, "postLaunchActions": ["toggle task roster"] if defer_roster else [],
                            "image": str(image), "tree": str(args.output / (name + ".json")),
                            "launchArguments": launch, "semanticState": state,
                            "appSha256": metadata["appSha256"], "sourceHead": metadata["sourceHead"]})
            checks = []
            if args.check_toggles and name in {"goal-tasks", "task-roster"}:
                tools = [("context-usage-ring", "context-usage", "context-usage-window"),
                         ("goal-status-pill", "goal", "goal-details-scroll")] if name == "goal-tasks" else [
                         ("project-subagent-status-pill", "agent-tasks", "project-subagent-roster-list")]
                if name == "task-roster":
                    run("--driver", "tap", "--id", "project-subagent-status-pill")
                    run("wait", "--id", "project-subagent-roster-list", "--gone", "--timeout", "10")
                for control, sheet, content in tools:
                    for cycle in range(2):
                        # Returning from a popover retries notification storage;
                        # unsigned simulator apps cannot read its Keychain group.
                        dismiss_unsigned_notification_alert()
                        run("--driver", "tap", "--id", control)
                        run("wait", "--id", content, "--timeout", "10")
                        opened = json.loads(run("--driver", "state"))
                        assert sheet in opened["sheets"], (control, "did not open", opened)
                        if cycle == 0:
                            (args.output / (control + "-open.json")).write_text(run("tree", "--json"))
                            subprocess.run([SIMCTL, "io", device, "screenshot", "--display=primary-1",
                                            str(args.output / (control + "-open.png"))], check=True, capture_output=True)
                        run("--driver", "tap", "--id", control)
                        run("wait", "--id", content, "--gone", "--timeout", "10")
                        # AX disappearance can precede UIKit's dismissal completion.
                        time.sleep(0.6)
                        closed = json.loads(run("--driver", "state"))
                        assert sheet not in closed["sheets"], (control, "did not close", closed)
                        checks.append({"control": control, "cycle": cycle + 1, "opened": opened["sheets"], "closed": closed["sheets"], "passed": True})
                results[-1]["toggleChecks"] = checks
            (args.output / "manifest.json").write_text(json.dumps(results, indent=2) + "\n")
    except Exception:
        # Preserve the affected posture before cleanup changes it.
        (args.output / "failure-semantic.json").write_text(run("--driver", "state"))
        (args.output / "failure-tree.json").write_text(run("tree", "--json"))
        display = "1" if PROFILE != "duo" else "primary-1" if hinge else "primary"
        subprocess.run([SIMCTL, "io", device, "screenshot", "--display=" + display, str(args.output / "failure.png")], check=True, capture_output=True)
        raise
    finally:
        if PROFILE == "duo" and not args.keep_posture:
            run("hinge", "0")
            run("orient", "portrait")


if __name__ == "__main__":
    main()
