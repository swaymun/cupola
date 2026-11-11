#!/usr/bin/env python3
"""Capture the T3 provider catalog and actual wonderd fixture snapshots, offline.

First run t3_native_followups_stay_in_t3_online_and_offline with
WONDER_T3_PROVIDER_SNAPSHOT_DIR set, and build Diagnostics with wonderctl.
Each invocation owns one pooled lane and releases it even on failure.
Semantic actions and native simulator pixels do not prove native touch or live T3.
"""
import argparse
import hashlib
import json
import os
import runpy
from pathlib import Path
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[3]
CTL = ROOT / "scripts/wonderctl"


def contact_sheets(output):
    """Use wonderctl's native AppKit sheet renderer; reject missing captures."""
    renderer = runpy.run_path(str(CTL))["preview_contact_sheet"]
    profiles = ["iphone", "ipad", "duo-folded", "duo-unfolded"]
    states = [("Catalog", "t3-providers-catalog"), ("OpenCode selected", "t3-providers-opencode-selected"),
              ("Cursor selected", "t3-providers-cursor-selected"), ("OpenCode error", "opencode-chat"),
              ("Cursor transcript", "cursor-chat")]
    manifests = {profile: json.loads((output / profile / "manifest.json").read_text()) for profile in profiles}
    for theme in ["light", "dark"]:
        entries = []
        for title, suffix in states:
            for profile in profiles:
                matches = [capture["path"] for capture in manifests[profile]["captures"]
                           if "-" + theme + "-" in capture["path"] and capture["path"].endswith("-" + suffix + ".png")]
                if len(matches) != 1:
                    raise RuntimeError(f"Expected one {profile}/{theme}/{suffix} capture, got {matches}")
                entries.append({"screen": title, "device": profile, "phase": "after",
                                "path": str((output / profile / matches[0]).resolve())})
        renderer(entries, output / ("contact-sheet-" + theme + ".png"))
    (output / "manifest.json").write_text(json.dumps({"kind": "native offline captures; no live acceptance claim",
        "profiles": manifests, "sheets": ["contact-sheet-light.png", "contact-sheet-dark.png"]}, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", choices=["iphone", "ipad", "duo"])
    parser.add_argument("--lane-timeout", type=int, default=900, help="pooled lane queue budget, seconds")
    parser.add_argument("--posture", choices=["folded", "unfolded"], default="folded")
    parser.add_argument("--snapshots", type=Path)
    parser.add_argument("--contact-sheets", action="store_true", help="assemble all four completed profile folders")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.contact_sheets:
        contact_sheets(args.output.resolve())
        return
    if not args.device or not args.snapshots:
        parser.error("capture requires --device and --snapshots")
    started = time.monotonic()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, WONDER_CONTROL_LANE_OWNER_PID=str(os.getpid()))
    metadata = {"device": args.device, "posture": args.posture,
                "kind": "offline protocol-fixture replay; semantic actions; native simulator captures",
                "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "captures": [], "snapshotSources": {}}
    diff = subprocess.check_output(["git", "diff", "HEAD"], cwd=ROOT)
    (output / "source.diff").write_bytes(diff)
    metadata["sourceDiffSha256"] = hashlib.sha256(diff).hexdigest()

    def run(*words):
        print("wonderctl", *words, flush=True)
        result = subprocess.run([str(CTL), "--device", args.device, *words], cwd=ROOT, env=env,
                                capture_output=True, text=True, timeout=max(240, args.lane_timeout + 60) if words[:2] == ("lane", "acquire") else 240)
        with (output / "commands.log").open("a") as log:
            log.write(" ".join(words) + "\n" + result.stdout + result.stderr)
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        return result.stdout.strip()

    def keep_capture(path, name):
        source = ROOT / path
        target = output / name
        shutil.copy2(source, target)
        metadata["captures"].append({"path": name, "sha256": hashlib.sha256(target.read_bytes()).hexdigest()})

    def shot(name, display):
        result = run("shot", name, *display)
        keep_capture(result.splitlines()[-1], name + ".png")

    acquired = False
    try:
        run("lane", "acquire", "--kind", args.device, "--owner-pid", str(os.getpid()), "--timeout", str(args.lane_timeout))
        acquired = True
        run("install")
        if args.device == "duo":
            run("hinge", "180")
            run("--driver", "launch", "--fixture", "subagent-fixture", "--fixture", "chat-layout",
                "--fixture", "chat-layout-unsaved", "--fixture", "open-project-thread")
            run("--driver", "wait", "--id", "message-draft", "--timeout", "30")
            run("hinge", "180" if args.posture == "unfolded" else "0")
            run("orient", "portrait")
        display = ["--display", "primary-1" if args.posture == "unfolded" else "primary"] if args.device == "duo" else []
        for theme in ["light", "dark"]:
            prefix = f"{args.device}-{args.posture}-{theme}"
            appearance = ["--arg=-diagnostics-theme", "--arg=cupola-" + theme,
                          "--arg=-AppleInterfaceStyle", "--arg=" + theme.capitalize()]
            if args.device == "ipad":
                appearance += ["--fixture", "no-hardware-keyboard"]
            lines = (ROOT / "apps/ios/control/scenarios/t3-code-providers.wctl").read_text().splitlines()
            steps = []
            for line in lines:
                if line.startswith("launch "):
                    line += " " + " ".join(appearance)
                if line.startswith("shot "):
                    line = "shot " + prefix + "-" + line[5:] + " " + " ".join(display)
                steps.append(line)
            recipe = output / (prefix + ".wctl")
            recipe.write_text("\n".join(steps) + "\n")
            result = run("--driver", "run", str(recipe))
            for line in result.splitlines():
                if line.startswith(".local/control/evidence/") and line.endswith(".png"):
                    keep_capture(line, Path(line).name)
            for family, expected in [("opencode", "maximum number of items is 128"), ("cursor", "DONE")]:
                source = args.snapshots.resolve() / (family + ".json")
                raw = source.read_bytes()
                metadata["snapshotSources"][family] = {"path": str(source), "sha256": hashlib.sha256(raw).hexdigest(),
                    "transform": "conversationId only, for the existing offline replay transport"}
                snapshot = json.loads(raw)
                snapshot["conversationId"] = "fixture-parent-conversation"
                container = Path(run("container").splitlines()[-1])
                staged = container / "Documents/history-replay.json"
                staged.parent.mkdir(parents=True, exist_ok=True)
                staged.write_text(json.dumps(snapshot))
                fixtures = ["subagent-fixture", "chat-layout", "chat-layout-unsaved", "open-project-thread",
                            family + "-fixture", "history-replay"]
                run("--driver", "launch", *[word for flag in fixtures for word in ["--fixture", flag]], *appearance)
                # Timeline text is a semantic row, not a control label. Assert
                # the resulting feed instead of waiting on the control inventory.
                deadline = time.monotonic() + 30
                while True:
                    state = run("--driver", "state")
                    timeline = json.loads(state).get("timeline", [])
                    replies = [row for entry in timeline for row in entry.get("rows", []) if not row.get("isUser")]
                    if any(expected in row.get("text", "") for row in replies):
                        break
                    if time.monotonic() >= deadline:
                        raise RuntimeError("Expected provider reply missing from the rendered timeline: " + expected)
                    time.sleep(0.25)
                counts = json.loads(state).get("userMessageCounts", {})
                if any(count != 1 for count in counts.values()):
                    raise RuntimeError("Duplicated user bubble in replay: " + json.dumps(counts))
                (output / f"{prefix}-{family}-state.json").write_text(state + "\n")
                shot(f"{prefix}-{family}-chat", display)
                staged.unlink(missing_ok=True)
    except Exception:
        if acquired:
            try:
                (output / "failure-state.json").write_text(run("--driver", "state") + "\n")
                shot("failure", display if "display" in locals() else [])
            except Exception:
                pass
        raise
    finally:
        if acquired:
            run("lane", "release", "--kind", args.device)
        metadata["elapsedSeconds"] = round(time.monotonic() - started, 2)
        (output / "manifest.json").write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
