#!/usr/bin/env python3
"""Capture the uniform offline minimap, including the Duo inner display.

Requires this checkout's Diagnostics build and an acquired pooled lane. This proves simulator layout, not Duo HID delivery (#387).
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[4]
CTL = ROOT / "scripts/wonderctl"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", choices=["ipad", "duo"], required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline", action="store_true", help="Capture the retained pre-change geometry build")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)

    def run(*command):
        result = subprocess.run([str(CTL), "--device", args.device, *command], cwd=ROOT,
                                text=True, capture_output=True)
        if result.returncode:
            raise RuntimeError(" ".join(command) + "\n" + result.stdout + result.stderr)
        return result.stdout

    if not json.loads(run("lane", "status", "--kind", args.device)).get("held"):
        parser.error("Acquire a pooled lane with wonderctl lane acquire --kind " + args.device)
    device = run("--driver", "device").strip()
    run("install")
    if args.device == "duo":
        run("hinge", "180")
        run("orient", "portrait")
    metadata = {"device": device, "profile": args.device,
                "sourceHead": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "fixture": "minimap-comparison: 24 turns; reply line counts repeat 1,3,8,16",
                "evidence": "simulator launch fixture; no native Duo input claim", "models": {}}
    diff = subprocess.check_output(["git", "diff", "HEAD", "--", "apps/ios", "apps/native"], cwd=ROOT)
    (args.output / "source.diff").write_bytes(diff)
    metadata["sourceDiffSha256"] = hashlib.sha256(diff).hexdigest()
    app = Path(subprocess.check_output(["xcrun", "simctl", "get_app_container", device,
                                        "com.swaymun.wonder", "app"], text=True).strip())
    metadata["installedAppSha256"] = hashlib.sha256((app / "Wonder").read_bytes()).hexdigest()
    for model in ["baseline"] if args.baseline else ["uniform"]:
        launch = ["--driver", "launch"]
        for fixture in ["subagent-fixture", "chat-layout", "chat-layout-unsaved", "minimap",
                        "minimap-comparison", "minimap-held", "minimap-visible", "open-project-thread"]:
            launch += ["--fixture", fixture]
        for argument in ["-wonder.sidebar.hidden", "YES", "-UIPreferredContentSizeCategoryName",
                         "UICTContentSizeCategoryL", "-diagnostics-theme", "cupola-light", "-AppleInterfaceStyle", "Light"]:
            launch += ["--arg=" + argument]
        run(*launch)
        state = json.loads(run("--driver", "state"))
        if state.get("alerts") == ["Unlock this device to open saved notification settings."]:
            run("--driver", "tap", "--id", "notification-error-dismiss")
        elif state.get("alerts"):
            raise RuntimeError("Unexpected alert: " + repr(state["alerts"]))
        run("wait", "--contains", "Reply 24.", "--timeout", "30")
        # The launch fixture shows the rail without relying on unqualified Duo gestures.
        tree = run("tree", "--json")
        (args.output / f"{model}.ax.json").write_text(tree)
        def nodes(value):
            if isinstance(value, dict):
                yield value
                for child in value.values():
                    yield from nodes(child)
            elif isinstance(value, list):
                for child in value:
                    yield from nodes(child)
        maps = [node for node in nodes(json.loads(tree)) if node.get("AXUniqueId") == "conversation-minimap"]
        if len(maps) != 1 or maps[0].get("frame", {}).get("height", 0) < 44:
            raise RuntimeError("Expected exactly one visible 44pt-or-taller mini-map: " + repr(maps))
        metadata["models"][model] = {"nativeFrame": maps[0]["frame"]}
        state = json.loads(run("--driver", "state"))
        if state.get("alerts"):
            raise RuntimeError("Cannot capture behind an alert: " + repr(state["alerts"]))
        (args.output / f"{model}.state.json").write_text(json.dumps(state, indent=2) + "\n")
        metadata["models"][model]["geometry"] = state.get("minimap")
        image = args.output / f"{model}.png"
        subprocess.run(["xcrun", "simctl", "io", device, "screenshot", "--display=" +
                        ("primary-1" if args.device == "duo" else "1"), str(image)], check=True, capture_output=True)
        print(image, flush=True)
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
