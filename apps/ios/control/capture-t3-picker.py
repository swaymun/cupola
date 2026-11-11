#!/usr/bin/env python3
"""Run the owned T3 picker scenario and capture a matched baseline.

Acquire the matching wonderctl pool lane first. Build both checkouts first.
The baseline may have the documented launch-only showingModel hook.
Native input is the default on every profile; --input rig selects the
display-associated simulator touch rig. Semantic layout probes are explicit
and never qualify native touch.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[3]
CTL = ROOT / "scripts/wonderctl"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", choices=["iphone", "ipad", "duo"], required=True)
    parser.add_argument("--posture", choices=["folded", "unfolded"], default="folded")
    parser.add_argument("--theme", choices=["light", "dark"], default="light")
    parser.add_argument("--baseline-checkout", type=Path)
    parser.add_argument("--source-head", help="Build source revision when running a copied app; requires .capture-source.diff")
    parser.add_argument("--baseline-head", help="Copied baseline build revision; requires its .capture-source.diff")
    parser.add_argument("--large-text", action="store_true")
    parser.add_argument("--visual-only", action="store_true", help="Short semantic layout probe; no native touch claim")
    parser.add_argument("--semantic", action="store_true", help="Full semantic scenario with native captures; no native touch claim")
    parser.add_argument("--input", choices=["axe", "rig"], default="axe")
    args = parser.parse_args()
    semantic = args.visual_only or args.semantic
    name = "-".join([args.device, args.posture, args.theme] + (["ax5"] if args.large_text else []))
    started = time.time()
    output = ROOT / ".local/control/t3-picker" / name / time.strftime("%Y%m%d-%H%M%S")
    output.mkdir(parents=True, exist_ok=True)
    command = [str(CTL), "--device", args.device]

    def run(*words, env=None):
        subprocess.run(command + list(words), cwd=ROOT, env=env, check=True)

    if args.device == "duo":
        run("hinge", "180" if args.posture == "unfolded" else "0")
        run("orient", "portrait")
    display = ["--display", "primary-1" if args.posture == "unfolded" else "primary"] if args.device == "duo" else []
    appearance = ["--arg=-diagnostics-theme", "--arg=cupola-" + args.theme,
                  "--arg=-AppleInterfaceStyle", "--arg=" + args.theme.capitalize(),
                  "--arg=-UIPreferredContentSizeCategoryName",
                  "--arg=" + ("UICTContentSizeCategoryAccessibilityXXXL" if args.large_text else "UICTContentSizeCategoryL")]
    metadata = {"profile": name, "head": args.source_head or subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "kind": "native simulator captures; semantic actions, no native touch claim" if semantic else "native simulator captures and " + args.input + " input"}
    def binary_hash(checkout):
        derived = Path(os.environ["WONDER_CONTROL_DERIVED"]) if checkout == ROOT and os.environ.get("WONDER_CONTROL_DERIVED") else checkout / ".local/build/control-sim"
        app = derived / "Build/Products/Diagnostics-iphonesimulator/Wonder.app"
        binary = app / "Wonder.debug.dylib"
        return hashlib.sha256((binary if binary.exists() else app / "Wonder").read_bytes()).hexdigest()
    metadata["appSha256"] = binary_hash(ROOT)
    diff = (ROOT / ".capture-source.diff").read_bytes() if args.source_head else subprocess.check_output(["git", "diff", "HEAD"], cwd=ROOT)
    metadata["sourceRevisionProvided"] = args.source_head is not None
    (output / "source.diff").write_bytes(diff)
    metadata["sourceDiffSha256"] = hashlib.sha256(diff).hexdigest()
    if args.baseline_checkout:
        baseline = args.baseline_checkout.resolve()
        env = dict(os.environ, WONDER_CONTROL_DERIVED=str(baseline / ".local/build/control-sim"))
        metadata["baselineHead"] = args.baseline_head or subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=baseline, text=True).strip()
        metadata["baselineAppSha256"] = binary_hash(baseline)
        baseline_diff = (baseline / ".capture-source.diff").read_bytes() if args.baseline_head else subprocess.check_output(["git", "diff", "HEAD"], cwd=baseline)
        (output / "baseline-source.diff").write_bytes(baseline_diff)
        fixtures = ["subagent-fixture", "chat-layout", "chat-layout-unsaved", "send-states", "send-claude",
                    "t3-code", "t3-code-launch", "t3-code-default-on", "several-computers"]
        launch = [word for fixture in fixtures for word in ["--fixture", fixture]]
        run("install", env=env)
        for surface in ["chat", "catalog"]:
            hook = ["--arg=-diagnostics-picker-baseline-model"] if surface == "catalog" else []
            run("launch", *launch, *appearance, *hook,
                "--arg=-wonder.newchat.host", "--arg=diagnostic-host", env=env)
            run("wait", "--id", "sheet-top" if hook else "new-chat-draft", "--timeout", "30", env=env)
            run("shot", name + "-before-" + surface, *display, env=env)
    run("install")
    source = ROOT / "apps/ios/control/scenarios/t3-code-picker.wctl"
    lines = source.read_text().splitlines()
    if args.visual_only:
        launch = next(line for line in lines if line.startswith("launch "))
        lines = [launch,
                 'wait --id composer-start-in-t3 --timeout 30', "shot t3-picker-connected",
                 "tap --id project-agent-picker", "wait --id new-chat-model:t3:codex:gpt-6.1 --timeout 15",
                 "shot t3-picker-catalog", "dismiss", "tap --id connection-picker",
                 "tap --id connection-option:diagnostic-host-studio",
                 'wait --id new-chat-selection-note --contains "Starting in Cupola" --timeout 15',
                 # Prove the whole note is initially visible in native geometry,
                 # without a semantic control standing in for visible pixels.
                 "driver off", "reveal down --id new-chat-selection-note --max-drags 0",
                 "shot t3-picker-computer-without-t3", "driver on", "tap --id project-agent-picker",
                 "wait --id new-chat-model:claude:haiku --timeout 15", "shot t3-picker-studio-native"]
        metadata["kind"] = "native simulator layout probe; semantic actions; no native touch claim"
        if args.device == "ipad" or (args.device == "duo" and args.posture == "unfolded"):
            # Pane-sized Toolbar wrappers (and the Duo's stale portrait root)
            # make generic reveal report the note's own container as an
            # occluder. Keep native visibility and pixels for human review.
            lines = ["wait --id new-chat-selection-note --timeout 15" if line.startswith("reveal ") else line for line in lines]
            metadata["noteVisibility"] = "Native AX visibility and image review; full-pane Toolbar wrappers prevent strict reveal."
    steps = []
    if semantic:
        steps.append("driver on")
    for line in lines:
        if line.startswith("launch "):
            line += " " + " ".join(appearance)
        if semantic and line.startswith("swipe down --from sheet-top"):
            line = "dismiss"
        elif args.device == "duo" and args.posture == "unfolded" and line.startswith("swipe down --from sheet-top"):
            # A native anchored popover closes by tapping outside it. Its
            # dismissal region also avoids AXe's stale portrait swipe edge.
            line = "tap --id PopoverDismissRegion"
        if line.startswith("shot "):
            line = line.replace("shot ", "shot " + name + "-after-", 1) + " " + " ".join(display)
        steps.append(line)
        if semantic and line == "dismiss":
            steps.append("assert --path presented --equals '[]' --timeout 10")
    scenario = output / "picker.wctl"
    scenario.write_text("\n".join(steps) + "\n")
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    run(*(["--driver"] if semantic else []), "run", str(scenario), "--input", args.input)
    metadata["captures"] = [{"path": str(path.relative_to(ROOT)), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                            for path in sorted((ROOT / ".local/control/evidence").glob("*" + name + "*.png"))
                            if path.stat().st_mtime >= started]
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
