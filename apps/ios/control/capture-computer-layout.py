#!/usr/bin/env python3
"""Capture the computer view's layout matrix for one simulator kind.

Usage: capture-computer-layout.py {iphone,ipad,duo} OUTDIR [--cell NAME ...] [--no-install]

For every (posture, state) cell of the kind, relaunches Cupola with the cell's launch
fixtures, opens the computer view, and writes beside each other under OUTDIR:
  <cell>.tree.json    `wonderctl tree --json` (accessibility frames)
  <cell>.state.json   `wonderctl --driver state` (carries displayDivision, the hinge band)
  <cell>.png          the active display
  <cell>.error        why a cell could not be reached (no .tree.json then)
`scripts/score-computer-layout` scores these; it also runs this for you.

The caller owns the pooled lane (`wonderctl lane acquire --kind K`); this script only
drives it. Duo sim taps are no-ops (#387), so the Duo opens the view through the
semantic driver tap and every state comes from launch fixtures and dock signals.
Synthetic host: nothing reaches a computer, a model or a real chat.
"""
import argparse
import json
import shlex
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
CTL = str(ROOT / "scripts/wonderctl")

BASE = ["computer-session-fixture", "computer-session-available-fixture", "computer-reconnect",
        "no-hardware-keyboard"]
# The Duo (and anything that shows the status pill) needs the entry fixtures; they are harmless elsewhere.
ENTRY = ["subagent-fixture", "chat-layout", "chat-layout-unsaved", "project-actions", "sidebar-status",
         "mac-waiting", "mac-permission", "computer-menu-fixture", "open-project-thread", "rail-fixture"]

# state -> (extra fixtures, extra raw args, then-steps)
STATES = {
    "idle": ([], [], []),
    "agent": (["computer-agent-preview", "computer-picture", "computer-simulator-fixture"], [], []),
    "controlling": (["computer-control-active"], [], ["control"]),
    "keyboard": (["computer-control-active"], ["-diagnostics-computer-keyboard"], ["control", "keyboard"]),
    "trackpad": (["computer-control-active"], ["-diagnostics-computer-trackpad"], ["control"]),
}

# posture -> dict(kinds, hinge, orient, display, fixtures, args)
POSTURES = {
    # iPhone/iPad landscape is not reachable: the rig's orientation control exists only on the Duo
    # simulator (wonderctl: "hinge/orientation requires the iPhone Duo simulator"), so phones and iPads
    # are scored in portrait only. Duo half-open covers the landscape (laptop) arrangement.
    "portrait": dict(kinds=("iphone", "ipad")),
    "folded": dict(kinds=("duo",), hinge=0, orient="portrait", display="primary", entry=True),
    "half-open": dict(kinds=("duo",), hinge=90, orient="landscape-left", display="primary-1", entry=True,
                      fixtures=["computer-fold"]),
    "flat": dict(kinds=("duo",), hinge=180, orient="portrait", display="primary-1", entry=True),
    "full-screen": dict(kinds=("duo",), hinge=180, orient="portrait", display="primary-1", entry=True,
                        args=["-diagnostics-computer-full-screen", "YES"]),
}


class PoseError(RuntimeError):
    pass


def cells(kind):
    for posture, spec in POSTURES.items():
        if kind in spec["kinds"]:
            for state in STATES:
                yield f"{kind}-{posture}-{state}", posture, state


def run(args, timeout=120, check=True):
    result = subprocess.run(args, cwd=ROOT, text=True, capture_output=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f"{shlex.join(map(str, args))}\n{result.stdout}\n{result.stderr}".strip())
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("kind", choices=("iphone", "ipad", "duo"))
    parser.add_argument("output", type=Path)
    parser.add_argument("--cell", action="append", help="only this cell (repeatable)")
    parser.add_argument("--no-install", action="store_true")
    args = parser.parse_args()
    ctl = [CTL, "--device", args.kind]
    lane = json.loads(run([CTL, "lane", "status", "--kind", args.kind]).stdout)
    if not lane.get("held"):
        parser.error(f"acquire a pooled lane first: wonderctl lane acquire --kind {args.kind}")
    device = run(ctl + ["device"]).stdout.strip()
    if device != lane["held"]:
        raise SystemExit("the selected simulator differs from the owned pooled lane")
    args.output.mkdir(parents=True, exist_ok=True)
    if not args.no_install:
        run(ctl + ["install"], timeout=300)
    failures = 0
    for name, posture, state in cells(args.kind):
        if args.cell and name not in args.cell:
            continue
        started = time.monotonic()
        for stale in args.output.glob(name + ".*"):
            stale.unlink()
        try:
            for attempt in (1, 2):
                try:
                    capture(ctl, device, args.output, name, POSTURES[posture], STATES[state])
                    break
                except PoseError:
                    if attempt == 2:
                        raise
                    print(f"{name}: pose did not take, retrying", flush=True)
            print(f"{name}: captured in {time.monotonic() - started:.0f}s", flush=True)
        except Exception as error:  # a cell that cannot be reached is a finding, not a crash
            failures += 1
            for partial in args.output.glob(name + ".*"):
                partial.unlink()  # a half-written cell must not be scored
            (args.output / (name + ".error")).write_text(str(error)[:2000] + "\n")
            print(f"{name}: NOT REACHED: {str(error).splitlines()[-1][:160]}", flush=True)
    print(f"{failures} cell(s) not reached")


def capture(ctl, device, out, name, posture, state):
    fixtures = BASE + (ENTRY if posture.get("entry") else []) + posture.get("fixtures", []) + state[0]
    if "agent" in name and not posture.get("entry"):
        fixtures = fixtures + ["read-preview", "send-preview"]
    raw = state[1] + posture.get("args", [])
    launch = ctl + ["--driver", "launch"]
    for fixture in fixtures:
        launch += ["--fixture", fixture]
    for item in raw:
        launch += ["--arg=" + item]
    launch += ["--arg=-wonder.sidebar.hidden", "--arg=YES"] if posture.get("entry") else []
    # Pose first: a Duo's hinge and orientation, a phone's orientation.
    if posture.get("hinge") == 90:  # come in from flat/portrait so the rotation takes
        run(ctl + ["--driver", "hinge", "180"])
        run(ctl + ["--driver", "orient", "portrait"])
        run(ctl + ["pause", "3"])
    run(ctl + ["--driver", "hinge", str(posture["hinge"])]) if "hinge" in posture else None
    run(ctl + ["pause", "3"])  # the simulator drops a rotation sent just after a hinge change
    run(ctl + ["--driver", "orient", posture["orient"]]) if "orient" in posture else None
    run(ctl + ["pause", "2"])
    run(launch, timeout=180)
    # Reach the open viewer: direct on phone/iPad idle; through the status pill otherwise.
    if run(ctl + ["wait", "--id", "computer-session-live-status", "--timeout", "12"], check=False).returncode:
        run(ctl + ["--driver", "wait", "--id", "computer-status-pill", "--timeout", "30"], timeout=60)
        run(ctl + ["--driver", "tap", "--id", "computer-status-pill"])
        run(ctl + ["wait", "--id", "computer-session-live-status", "--timeout", "30"], timeout=60)
    run(ctl + ["wait", "--id", "computer-session-more", "--timeout", "20"], timeout=40)
    if "control" in state[2]:
        # Opening the view takes control by itself; the marker proves it. A dock signal is the fallback.
        if run(ctl + ["wait", "--id", "computer-session-control-active", "--timeout", "15"], check=False).returncode:
            run(ctl + ["signal", "computer-dock-take-control"], check=False)
            run(ctl + ["wait", "--id", "computer-session-control-active", "--timeout", "20"], timeout=40, check="keyboard" not in state[2])
    if "keyboard" in state[2]:
        run(ctl + ["signal", "computer-dock-keyboard"], check=False)
        run(ctl + ["wait", "--id", "computer-session-keyboard", "--contains", "Hide keyboard", "--timeout", "15"],
            timeout=40, check=False)
    run(ctl + ["pause", "3"])
    tree = run(ctl + ["tree", "--json"]).stdout
    (out / (name + ".tree.json")).write_text(tree)
    state_json = run(ctl + ["--driver", "state"], check=False).stdout
    (out / (name + ".state.json")).write_text(state_json)
    if posture.get("hinge") == 90:  # the laptop pose has a horizontal band; a dropped rotation leaves a vertical one
        band = (json.loads(state_json or "{}").get("displayDivision") or {})
        if not band or band.get("width", 0) <= band.get("height", 0):
            raise PoseError("half-open pose did not take (hinge band is not horizontal)")
    display = ["--display=" + posture["display"]] if posture.get("display") else []
    run(["xcrun", "simctl", "io", device, "screenshot"] + display + [str(out / (name + ".png"))], check=False)


if __name__ == "__main__":
    sys.exit(main())
